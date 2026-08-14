//! Schema v1 and the connection pragmas, both transcribed from master plan
//! Part 5.1 (as amended by ADR-0004).

use std::collections::BTreeMap;

use rusqlite::Connection;
use rusqlite::types::Value;
use serde::Serialize;

use crate::error::{StoreError, StoreResult};

/// The highest schema version this binary understands.
pub const SUPPORTED_SCHEMA_VERSION: i64 = 3;

/// Pragmas as they are *set*, in application order.
///
/// `journal_mode` is persistent in the database file; every other entry here is
/// per-connection and must be re-applied on every open.
pub const PRAGMAS_SET: &[(&str, &str)] = &[
    ("journal_mode", "WAL"),
    ("synchronous", "FULL"),
    ("fullfsync", "1"),
    ("checkpoint_fullfsync", "1"),
    ("foreign_keys", "ON"),
    ("busy_timeout", "5000"),
];

/// Pragmas as they must *read back*.
///
/// The set value and the readback value are not the same string: `journal_mode`
/// answers with the mode name in lower case, `synchronous=FULL` answers `2`, and
/// the boolean pragmas answer `1`. ADR-0004 left "a pragma silently dropped" as
/// an open item for S1; this table is what closes it.
pub const PRAGMAS_EXPECTED: &[(&str, &str)] = &[
    ("journal_mode", "wal"),
    ("synchronous", "2"),
    ("fullfsync", "1"),
    ("checkpoint_fullfsync", "1"),
    ("foreign_keys", "1"),
    ("busy_timeout", "5000"),
];

/// What the connection actually reports after [`apply_pragmas`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PragmaReport {
    /// Pragma name to the value SQLite reports, rendered as text.
    pub values: BTreeMap<String, String>,
}

impl PragmaReport {
    /// The reported value of one pragma.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Every pragma whose readback differs from [`PRAGMAS_EXPECTED`], as
    /// `(name, expected, actual)`.
    pub fn mismatches(&self) -> Vec<(&'static str, &'static str, String)> {
        PRAGMAS_EXPECTED
            .iter()
            .filter_map(|(name, expected)| {
                let actual = self.get(name).unwrap_or("<absent>");
                if actual.eq_ignore_ascii_case(expected) {
                    None
                } else {
                    Some((*name, *expected, actual.to_string()))
                }
            })
            .collect()
    }
}

/// Apply every pragma and verify the readback. Fails closed on a mismatch.
pub fn apply_pragmas(conn: &Connection) -> StoreResult<PragmaReport> {
    for (name, value) in PRAGMAS_SET {
        // execute_batch, not execute: several of these return a row, and one
        // (journal_mode) always does.
        conn.execute_batch(&format!("PRAGMA {name} = {value};"))?;
    }
    let report = read_pragmas(conn)?;
    if let Some((name, expected, actual)) = report.mismatches().into_iter().next() {
        return Err(StoreError::PragmaMismatch {
            pragma: name,
            expected,
            actual,
        });
    }
    Ok(report)
}

/// Read back every pragma in [`PRAGMAS_EXPECTED`] without setting anything.
pub fn read_pragmas(conn: &Connection) -> StoreResult<PragmaReport> {
    let mut values = BTreeMap::new();
    for (name, _) in PRAGMAS_EXPECTED {
        let value: Value = conn.query_row(&format!("PRAGMA {name};"), [], |row| row.get(0))?;
        let rendered = match value {
            Value::Null => "<null>".to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Real(r) => r.to_string(),
            Value::Text(t) => t,
            Value::Blob(_) => "<blob>".to_string(),
        };
        values.insert((*name).to_string(), rendered);
    }
    Ok(PragmaReport { values })
}

/// Schema v1 — master plan Part 5.1, verbatim.
pub const SCHEMA_V1: &str = r#"
CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);

CREATE TABLE project (
  id             TEXT PRIMARY KEY,           -- p-<short>
  root_path      TEXT NOT NULL UNIQUE,
  repo_identity  TEXT NOT NULL,              -- blake3(first_commit ‖ normalized_origin)
  default_branch TEXT NOT NULL,
  config_hash    TEXT NOT NULL,
  created_at     INTEGER NOT NULL
);

-- index over .conductor/plans/vN/ ; git is authoritative
CREATE TABLE plan_version (
  id           TEXT PRIMARY KEY,
  project_id   TEXT NOT NULL REFERENCES project(id),
  version      INTEGER NOT NULL,
  content_hash TEXT NOT NULL,                -- of canonical semantic content
  state        TEXT NOT NULL,                -- DRAFT|VALIDATED|AWAITING_APPROVAL|APPROVED|SUPERSEDED
  approved_at  INTEGER, approved_by TEXT,
  source_path  TEXT NOT NULL,
  UNIQUE(project_id, version)
);

CREATE TABLE decision (
  id              TEXT PRIMARY KEY,          -- D-0007
  project_id      TEXT NOT NULL REFERENCES project(id),
  status          TEXT NOT NULL,             -- OPEN|ACCEPTED|REJECTED|SUPERSEDED
  supersedes      TEXT REFERENCES decision(id),
  content_hash    TEXT NOT NULL,
  source_path     TEXT NOT NULL
);

CREATE TABLE task (
  id                   TEXT PRIMARY KEY,     -- T-0012
  plan_version_id      TEXT NOT NULL REFERENCES plan_version(id),
  slice_id             TEXT NOT NULL,
  state                TEXT NOT NULL,
  scope_globs          TEXT NOT NULL,        -- json
  verification_profile TEXT NOT NULL,
  attempt_budget       INTEGER NOT NULL DEFAULT 3,
  created_at           INTEGER NOT NULL
);

CREATE TABLE run (
  id               TEXT PRIMARY KEY,         -- r-0041
  task_id          TEXT NOT NULL REFERENCES task(id),
  policy_hash      TEXT NOT NULL REFERENCES policy_snapshot(hash),
  workspace_id     TEXT REFERENCES workspace(id),
  base_commit      TEXT NOT NULL,
  run_branch       TEXT NOT NULL,
  state            TEXT NOT NULL,
  priority         INTEGER NOT NULL DEFAULT 100,
  lease_owner      TEXT,
  lease_expires_at INTEGER,
  lease_epoch      INTEGER NOT NULL DEFAULT 0,   -- fencing token
  created_at       INTEGER NOT NULL
);
CREATE UNIQUE INDEX ix_run_one_active_per_task ON run(task_id)
  WHERE state NOT IN ('COMPLETE','CANCELLED','SUPERSEDED');
CREATE INDEX ix_run_claim ON run(state, lease_expires_at);

CREATE TABLE attempt (
  id               TEXT PRIMARY KEY,
  run_id           TEXT NOT NULL REFERENCES run(id),
  ordinal          INTEGER NOT NULL,
  kind             TEXT NOT NULL,            -- IMPLEMENT|REPAIR|CONTINUE
  adapter          TEXT NOT NULL,
  launcher         TEXT NOT NULL,            -- none|codex-sandbox|sandbox-exec
  caps_snapshot    TEXT NOT NULL,            -- measured ExecutionCapabilities, json
  agent_session_id TEXT,
  pid              INTEGER, pid_start_time INTEGER,
  started_at       INTEGER, ended_at INTEGER,
  exit_code        INTEGER, signal INTEGER,
  outcome          TEXT,                     -- EXITED|CRASHED|TIMED_OUT|STALE|RECONCILED
  UNIQUE(run_id, ordinal)
);

CREATE TABLE workspace (
  id          TEXT PRIMARY KEY,
  run_id      TEXT NOT NULL REFERENCES run(id),
  path        TEXT NOT NULL UNIQUE,
  kind        TEXT NOT NULL DEFAULT 'CLONE_NO_HARDLINKS',
  source_repo TEXT NOT NULL,
  state       TEXT NOT NULL,                 -- ACTIVE|RETAINED|QUARANTINED|REMOVED
  created_at  INTEGER NOT NULL, removed_at INTEGER
);

-- append-only EVIDENCE log. NOT event sourcing; state is never replayed from it.
CREATE TABLE event (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id  TEXT REFERENCES run(id),
  seq     INTEGER NOT NULL,
  kind    TEXT NOT NULL,
  payload TEXT NOT NULL,
  at      INTEGER NOT NULL
);
-- UNIQUE, not a plain index: a duplicate claim must fail at INSERT time rather
-- than be recorded and noticed later by an offline checker. S0's claim harness
-- carried this constraint and the shipping DDL had lost it (ADR-0005).
CREATE UNIQUE INDEX ix_event_run ON event(run_id, seq);

CREATE TABLE verification_check (
  id                    TEXT PRIMARY KEY,
  run_id                TEXT NOT NULL REFERENCES run(id),
  attempt_id            TEXT REFERENCES attempt(id),
  tree_hash             TEXT NOT NULL,
  commit_sha            TEXT NOT NULL,
  toolchain_fingerprint TEXT NOT NULL,
  check_id              TEXT NOT NULL,
  command_hash          TEXT NOT NULL,
  exit_code             INTEGER,
  duration_ms           INTEGER,
  outcome               TEXT NOT NULL,       -- PASS|FAIL|INCONCLUSIVE|VOID
  log_path              TEXT
);
CREATE UNIQUE INDEX ix_verif_cache
  ON verification_check(tree_hash, check_id, command_hash, toolchain_fingerprint)
  WHERE outcome IN ('PASS','FAIL');

CREATE TABLE policy_snapshot (
  hash TEXT PRIMARY KEY, canonical_blob TEXT NOT NULL, created_at INTEGER NOT NULL
);

CREATE TABLE approval_request (
  id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES run(id),
  action TEXT NOT NULL, facts TEXT NOT NULL, facts_source TEXT NOT NULL,
  policy_hash TEXT NOT NULL, matched_rules TEXT NOT NULL, explanation TEXT NOT NULL,
  evidence_ref TEXT, state TEXT NOT NULL,     -- REQUESTED|GRANTED|DENIED|EXPIRED
  requested_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);

CREATE TABLE approval_grant (
  id TEXT PRIMARY KEY, request_id TEXT NOT NULL REFERENCES approval_request(id),
  binding_hash TEXT NOT NULL, scope TEXT NOT NULL, reuse INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL,                        -- GRANTED|CONSUMED|EXPIRED|REVOKED
  nonce_hash TEXT, channel TEXT NOT NULL,
  granted_by TEXT NOT NULL, granted_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);
CREATE INDEX ix_grant_binding ON approval_grant(binding_hash, state);

CREATE TABLE side_effect (
  operation_id TEXT PRIMARY KEY,              -- blake3(kind ‖ run ‖ ordinal ‖ tree_hash)
  run_id       TEXT NOT NULL REFERENCES run(id),
  kind         TEXT NOT NULL,
  state        TEXT NOT NULL,                 -- INTENDED|CONFIRMED|FAILED|AMBIGUOUS
  precondition TEXT NOT NULL, receipt TEXT,
  intended_at  INTEGER NOT NULL, resolved_at INTEGER
);

CREATE TABLE finding (
  id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES run(id),
  kind TEXT NOT NULL, severity TEXT NOT NULL, evidence_ref TEXT NOT NULL,
  resolution TEXT, created_at INTEGER NOT NULL   -- never auto-resolves
);

CREATE TABLE artifact (
  id TEXT PRIMARY KEY, run_id TEXT REFERENCES run(id),
  kind TEXT NOT NULL, path TEXT NOT NULL, sha256 TEXT NOT NULL, created_at INTEGER NOT NULL
);

CREATE TABLE containment_probe (
  id TEXT PRIMARY KEY, adapter TEXT NOT NULL, adapter_version TEXT NOT NULL,
  launcher TEXT NOT NULL, launcher_version TEXT NOT NULL, os_version TEXT NOT NULL,
  capabilities TEXT NOT NULL, probed_at INTEGER NOT NULL,
  UNIQUE(adapter, adapter_version, launcher, launcher_version, os_version)
);
"#;

/// Schema v2 — `attempt.state`.
///
/// **Why a migration and not an edit to [`SCHEMA_V1`].** Migrations are
/// forward-only and v1 has shipped; editing it in place would make two databases
/// that both call themselves version 1 disagree about their columns, which is
/// the failure mode versioned migrations exist to prevent. The DDL fidelity test
/// also compares `SCHEMA_V1` against master plan Part 5.1 statement for
/// statement, and that comparison must keep meaning what it says.
///
/// **Why the column is needed.** §5.2's attempt machine has eight states; v1's
/// `attempt.outcome` can express five, none of which is `CREATED`, `STARTING` or
/// `ACTIVE`. A supervisor therefore had no way to record that an attempt was in
/// flight — which is precisely what §4.7's startup recovery reads in order to
/// tell "this run had a process" from "this run never started one". The
/// contradiction was surfaced at S1 and left for S3 on the grounds that S3 knows
/// what the supervisor actually needs.
///
/// `DEFAULT 'CREATED'` is the honest value for a pre-existing row: v1 recorded
/// no lifecycle position, so the only thing that can be said about such an
/// attempt is that it exists. Recovery treats it as in-flight and looks at the
/// world, which is the safe direction — the unsafe direction would be defaulting
/// to a terminal state and skipping reconciliation.
pub const SCHEMA_V2: &str = r#"
ALTER TABLE attempt ADD COLUMN state TEXT NOT NULL DEFAULT 'CREATED';

-- Startup recovery's first question is "which attempts were in flight?", and
-- it asks it on every start. Partial, because the in-flight set is tiny and
-- permanently so while the terminal set grows without bound.
CREATE INDEX ix_attempt_in_flight ON attempt(state)
  WHERE state IN ('CREATED','STARTING','ACTIVE');
"#;

/// Schema v3 — name the artifact digest column for the hash Conductor computes.
///
/// §4.5 says "sha256 recorded" and Part 5.1 named the column `sha256`, but §2.2's
/// dependency list authorises `blake3` and **no SHA-2 implementation**, and S3 had
/// already established `content_hash()` = `blake3:<hex>`. A column named `sha256`
/// holding a BLAKE3 digest is a lie the schema tells every later reader, and the
/// natural "fix" would be to add an unauthorised dependency to satisfy incidental
/// wording. Renamed instead (ADR-0007). Nothing had written the table yet.
pub const SCHEMA_V3: &str = r#"
ALTER TABLE artifact RENAME COLUMN sha256 TO content_hash;
"#;
