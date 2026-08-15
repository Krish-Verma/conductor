//! Schema v1 and the connection pragmas, both transcribed from master plan
//! Part 5.1 (as amended by ADR-0004).

use std::collections::BTreeMap;

use rusqlite::Connection;
use rusqlite::types::Value;
use serde::Serialize;

use crate::error::{StoreError, StoreResult};

/// The highest schema version this binary understands.
pub const SUPPORTED_SCHEMA_VERSION: i64 = 6;

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

/// Schema v4 — the ref a run integrates into.
///
/// §4.1 requires that "at integration, if the target ref moved, the run enters
/// `AWAITING_REVIEW` with the divergence attached" (acceptance row 16). Deciding
/// that takes two facts: **which** ref, and what it pointed at when the run
/// started. Part 5.1's `run` table has the second — `base_commit` — and had no
/// place at all for the first, so the question could not be asked. `run_branch`
/// is not it: that is the branch the *agent's* work lives on inside the clone.
///
/// Nullable, because rows written before v4 genuinely do not know their target
/// and inventing `'main'` for them would be exactly the guess §4.7 forbids
/// elsewhere. A run with no recorded target is refused at integration rather
/// than integrated into a branch nobody chose.
pub const SCHEMA_V4: &str = r#"
ALTER TABLE run ADD COLUMN target_branch TEXT;
"#;

/// Schema v5 — what repair observed about each attempt that did not succeed.
///
/// §4.6's three loop-breakers are decided from the history of a run's attempts,
/// and §4.6's acceptance property is that "no configuration of the fake agent
/// can produce more than `max_attempts` agent invocations". A history held in
/// memory cannot deliver either: §4.7's whole premise is that Conductor is
/// killed and restarted, and a bound that resets on restart is not a bound —
/// crash-restart cycles would produce unbounded agent invocations while every
/// in-memory counter read zero.
///
/// # The inputs are the truth; `fingerprint` is a convenience
///
/// `failing_checks`, `assertion` and `tree_hash` are the **inputs** to §4.6's
/// definition. `fingerprint` is the derived digest, stored so that a human
/// reading the table — or a repair packet quoting it — does not have to run
/// Conductor to see which failure recurred.
///
/// **Nothing reads `fingerprint` back to make a decision.** Loading rebuilds the
/// `Failure` from the three inputs and recomputes the digest, so a stored
/// digest that disagreed with its own inputs (a normalizer change, a partial
/// write, a hand edit) can never steer a loop-breaker. A denormalized column
/// that decisions depend on is a second source of truth; one that only humans
/// read is a comment with an index.
///
/// # Why the primary key is the attempt
///
/// One attempt produces at most one observation, and `attempt` rows are written
/// **before** the agent is spawned (two committed transactions ahead of it), so
/// keying on the attempt makes the observation's identity as durable as the
/// invocation it describes. `UNIQUE(run_id, ordinal)` restates
/// `attempt`'s own uniqueness so that the ordering this table is read in —
/// oldest attempt first — cannot contain two rows claiming the same position.
///
/// `kind` is `FAILED` | `NO_CHANGE` | `CRASHED` | `INFRASTRUCTURE`; the last is
/// the §4.7 distinction that must never be conflated with the first three,
/// "because conflating them is how a broken API key silently exhausts a task's
/// budget".
pub const SCHEMA_V5: &str = r#"
CREATE TABLE repair_observation (
  attempt_id     TEXT PRIMARY KEY REFERENCES attempt(id),
  run_id         TEXT NOT NULL REFERENCES run(id),
  ordinal        INTEGER NOT NULL,
  kind           TEXT NOT NULL,       -- FAILED|NO_CHANGE|CRASHED|INFRASTRUCTURE
  failing_checks TEXT NOT NULL,       -- json array, sorted
  assertion      TEXT NOT NULL,
  tree_hash      TEXT NOT NULL,
  fingerprint    TEXT NOT NULL,       -- derived; never read back to decide
  recorded_at    INTEGER NOT NULL,
  UNIQUE(run_id, ordinal)
);
"#;

/// Schema v6 — the approval tables can hold §4.3's **four** kinds.
///
/// # What v1 could not express
///
/// §4.3 lines 569-578 name four approval kinds and say they must **never**
/// collapse into `approved: bool`, "because collapsing them would let a plan
/// approval satisfy a deployment gate". Part 5.1's tables cannot hold four
/// kinds. Three specific mismatches, each found by trying to write the row:
///
/// | §4.3 says | v1 says | consequence |
/// |---|---|---|
/// | four kinds, never collapsed | no `kind` column | the kinds *are* collapsed — every row reads as the one kind v1's other columns describe |
/// | a plan approval authorizes a **plan version**; a review acceptance authorizes a **review packet** | `run_id TEXT NOT NULL REFERENCES run(id)` | neither is run-scoped, so recording one means inventing a run |
/// | plan approval and review acceptance **do not expire** | `expires_at INTEGER NOT NULL` on both tables | a perpetual approval must be given a fabricated TTL, and would then silently lapse |
///
/// v1's shape is exactly §4.3's *worked example*, which is a policy approval.
/// The other three kinds were specified in prose and never given a row.
///
/// # What v6 changes, and nothing more
///
/// * `kind` — which of the four. `NOT NULL`, no default on the new table:
///   every writer must say. Pre-existing rows are migrated to
///   `POLICY_APPROVAL`, the only kind v1's columns could have meant.
/// * `subject` — what is authorized when it is not a run: a plan version id, a
///   review packet id, a rule id. Nullable, because a policy approval's subject
///   is already in `action` + `facts`.
/// * `run_id` — nullable. A plan approval has no run, and inventing one would
///   be a lie the schema tells every later reader (ADR-0007's reasoning about
///   the `sha256` column, applied to a foreign key).
/// * `expires_at` — nullable on **both** tables. `NULL` means "does not
///   expire", which is a distinct fact from any timestamp. A sentinel far in
///   the future was rejected: it would make every expiry query read as if the
///   approval expires, and one arithmetic slip away from expiring an
///   authoritative plan.
/// * `resolved_at` on the grant — when it left `GRANTED`. Mirrors
///   `side_effect.resolved_at`, which exists for the same reason: an audit
///   reading a terminal row needs to know *when* without joining the event log.
///
/// Deliberately **not** added: a `kind` column on `approval_grant`. A grant's
/// kind is its request's kind, and denormalizing it would create a second
/// source of truth that can disagree — the hazard schema v5 records about
/// `repair_observation.fingerprint`.
///
/// # Why this is a table rebuild
///
/// SQLite's `ALTER TABLE` cannot drop `NOT NULL`, so the two columns that must
/// become nullable force the copy-and-swap. `approval_grant` is rebuilt as well
/// because a rename rewrites the foreign keys that point at the renamed table:
/// measured, not assumed — renaming `approval_request` alone leaves
/// `approval_grant` referencing `"approval_request_v1"`. Dropping the old grant
/// table before the old request table keeps `PRAGMA foreign_keys = ON`
/// satisfied throughout, and the migration harness runs
/// `PRAGMA integrity_check` after the commit.
pub const SCHEMA_V6: &str = r#"
CREATE TABLE approval_request_v6 (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,                         -- PLAN_APPROVAL|POLICY_APPROVAL|POLICY_EXCEPTION|REVIEW_ACCEPTANCE
  subject TEXT,                               -- plan version, review packet or rule; NULL for a policy approval
  run_id TEXT REFERENCES run(id),             -- NULL: a plan approval has no run
  action TEXT NOT NULL, facts TEXT NOT NULL, facts_source TEXT NOT NULL,
  policy_hash TEXT NOT NULL, matched_rules TEXT NOT NULL, explanation TEXT NOT NULL,
  evidence_ref TEXT, state TEXT NOT NULL,     -- REQUESTED|GRANTED|DENIED|EXPIRED
  requested_at INTEGER NOT NULL, expires_at INTEGER   -- NULL: does not expire
);

INSERT INTO approval_request_v6
  (id, kind, subject, run_id, action, facts, facts_source, policy_hash,
   matched_rules, explanation, evidence_ref, state, requested_at, expires_at)
  SELECT id, 'POLICY_APPROVAL', NULL, run_id, action, facts, facts_source,
         policy_hash, matched_rules, explanation, evidence_ref, state,
         requested_at, expires_at
    FROM approval_request;

CREATE TABLE approval_grant_v6 (
  id TEXT PRIMARY KEY, request_id TEXT NOT NULL REFERENCES approval_request_v6(id),
  binding_hash TEXT NOT NULL, scope TEXT NOT NULL, reuse INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL,                        -- GRANTED|CONSUMED|EXPIRED|REVOKED
  nonce_hash TEXT, channel TEXT NOT NULL,
  granted_by TEXT NOT NULL, granted_at INTEGER NOT NULL,
  expires_at INTEGER,                         -- NULL: does not expire
  resolved_at INTEGER                         -- when it left GRANTED
);

INSERT INTO approval_grant_v6
  (id, request_id, binding_hash, scope, reuse, state, nonce_hash, channel,
   granted_by, granted_at, expires_at, resolved_at)
  SELECT id, request_id, binding_hash, scope, reuse, state, nonce_hash, channel,
         granted_by, granted_at, expires_at, NULL
    FROM approval_grant;

DROP TABLE approval_grant;
DROP TABLE approval_request;
ALTER TABLE approval_request_v6 RENAME TO approval_request;
ALTER TABLE approval_grant_v6 RENAME TO approval_grant;

CREATE INDEX ix_grant_binding ON approval_grant(binding_hash, state);
-- §4.7 step 9 sweeps expired requests on every start, and the socket lists
-- pending ones. Both range over the tiny non-terminal set while the terminal
-- set grows without bound, so the index is partial for the same reason
-- `ix_attempt_in_flight` is.
CREATE INDEX ix_request_pending ON approval_request(state, expires_at)
  WHERE state = 'REQUESTED';
"#;
