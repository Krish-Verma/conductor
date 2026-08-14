//! The content-addressed verification cache — master plan §4.5, Part 5.1.
//!
//! ```sql
//! CREATE UNIQUE INDEX ix_verif_cache
//!   ON verification_check(tree_hash, check_id, command_hash, toolchain_fingerprint)
//!   WHERE outcome IN ('PASS','FAIL');
//! ```
//!
//! # Why `INCONCLUSIVE` and `VOID` are not in that predicate
//!
//! The index is partial, and the partiality is the design.
//!
//! **`INCONCLUSIVE` describes a moment, not a tree.** A timeout, a vanished
//! toolchain, a full disk: none of these are properties of the source. Caching
//! one would freeze a transient condition into a permanent answer for that
//! tree — and §4.5's remedy for `INCONCLUSIVE` is "bounded infra retry, then
//! human", which a cache hit makes impossible. It would also be self-defeating
//! in exactly the way §4.5 warns about: "collapsing them is how a broken cache
//! turns into three wasted agent attempts."
//!
//! **`VOID` is the absence of a result.** It means the tree moved under the
//! check, so nothing is known about which tree was observed. Filing that under
//! a tree hash *as an answer for that tree* would be a lie the schema would
//! then serve back with a straight face.
//!
//! Both are still **written**. The row is the audit trail — how many times a
//! check timed out, how often a tree was mutated mid-check — and Part 5.1's
//! index simply does not constrain them, so repeated attempts at one key do not
//! collide.
//!
//! # What a hit buys
//!
//! §4.5: re-verifying an unchanged tree becomes a lookup, "which matters most
//! in repair loops", and after a daemon crash "an unchanged tree with a valid
//! result is not re-run" (§4.7 step 6).

use conductor_core::{EventKind, Fence, VerificationOutcome};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};
use crate::lease::{append_event, check_fence};
use crate::tx::with_immediate;

/// The four components §4.5 keys a result by.
///
/// A struct rather than four positional arguments: three of the four are hashes
/// rendered the same way, and a transposed pair would produce a cache that
/// answers confidently about the wrong thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheKey<'a> {
    /// The **working**-tree hash the check observed.
    pub tree_hash: &'a str,
    /// `check.id` from the profile.
    pub check_id: &'a str,
    /// `blake3` over the resolved argv.
    pub command_hash: &'a str,
    /// `blake3` over the toolchain fingerprint commands and their output.
    pub toolchain_fingerprint: &'a str,
}

/// Everything about a result that is not part of its key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationRecord {
    /// `verification_check.id`.
    pub id: String,
    /// The attempt this ran under, when there is one.
    pub attempt_id: Option<String>,
    /// `HEAD` at the time, recorded alongside the tree hash because a tree can
    /// belong to many commits and a human reading a finding wants the commit.
    pub commit_sha: String,
    /// The check's exit code, when it produced one.
    pub exit_code: Option<i32>,
    /// Wall time.
    pub duration_ms: Option<i64>,
    /// The §4.5 outcome.
    pub outcome: VerificationOutcome,
    /// Where the log went. **Never the log itself** (§4.5).
    pub log_path: Option<String>,
}

/// A row as read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CachedResult {
    /// `verification_check.id`.
    pub id: String,
    /// The outcome.
    pub outcome: VerificationOutcome,
    /// The exit code that produced it.
    pub exit_code: Option<i32>,
    /// How long it took the first time.
    pub duration_ms: Option<i64>,
    /// The log from that run.
    pub log_path: Option<String>,
}

/// What [`record`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RecordOutcome {
    /// A new row.
    Inserted,
    /// A cacheable result was already stored at this key, and it agrees.
    AlreadyPresent,
    /// A cacheable result was already stored at this key and it **disagrees**.
    ///
    /// The stored answer is kept — flip-flopping would make the cache depend on
    /// the order results happened to arrive in — and the caller is told, so it
    /// can raise a finding. A check that returns two answers for one tree, one
    /// command and one toolchain is nondeterministic, and that is precisely the
    /// defect a content-addressed cache would otherwise conceal forever.
    Contradicted {
        /// What the cache already holds.
        stored: VerificationOutcome,
    },
}

/// Look up a result. Only `PASS` and `FAIL` are ever returned.
pub fn cached(conn: &Connection, key: &CacheKey<'_>) -> StoreResult<Option<CachedResult>> {
    let mut stmt = conn.prepare(
        "SELECT id, outcome, exit_code, duration_ms, log_path
           FROM verification_check
          WHERE tree_hash = ?1 AND check_id = ?2
            AND command_hash = ?3 AND toolchain_fingerprint = ?4
            AND outcome IN ('PASS','FAIL')
          LIMIT 1",
    )?;
    let mut rows = stmt.query(params![
        key.tree_hash,
        key.check_id,
        key.command_hash,
        key.toolchain_fingerprint
    ])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let outcome: String = row.get(1)?;
    Ok(Some(CachedResult {
        id: row.get(0)?,
        outcome: outcome.parse::<VerificationOutcome>()?,
        exit_code: row.get(2)?,
        duration_ms: row.get(3)?,
        log_path: row.get(4)?,
    }))
}

/// Record a result. Fenced, like every write in this crate (§4.7).
pub fn record(
    conn: &mut Connection,
    fence: &Fence,
    key: &CacheKey<'_>,
    record: &VerificationRecord,
    now_ms: i64,
) -> StoreResult<RecordOutcome> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;

        if record.outcome.is_cacheable() {
            // Read inside the same transaction as the insert: `BEGIN
            // IMMEDIATE` means no other writer can slip between them.
            let existing: Option<String> = tx
                .query_row(
                    "SELECT outcome FROM verification_check
                      WHERE tree_hash = ?1 AND check_id = ?2
                        AND command_hash = ?3 AND toolchain_fingerprint = ?4
                        AND outcome IN ('PASS','FAIL')",
                    params![
                        key.tree_hash,
                        key.check_id,
                        key.command_hash,
                        key.toolchain_fingerprint
                    ],
                    |row| row.get(0),
                )
                .ok();
            if let Some(existing) = existing {
                let stored = existing.parse::<VerificationOutcome>()?;
                return Ok(if stored == record.outcome {
                    RecordOutcome::AlreadyPresent
                } else {
                    RecordOutcome::Contradicted { stored }
                });
            }
        }

        tx.execute(
            "INSERT INTO verification_check
               (id, run_id, attempt_id, tree_hash, commit_sha, toolchain_fingerprint,
                check_id, command_hash, exit_code, duration_ms, outcome, log_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                record.id,
                fence.run_id().as_str(),
                record.attempt_id,
                key.tree_hash,
                record.commit_sha,
                key.toolchain_fingerprint,
                key.check_id,
                key.command_hash,
                record.exit_code,
                record.duration_ms,
                record.outcome.as_str(),
                record.log_path,
            ],
        )?;

        append_event(
            tx,
            fence.run_id(),
            EventKind::VerificationRecorded,
            &serde_json::json!({
                "check": key.check_id,
                "tree_hash": key.tree_hash,
                "outcome": record.outcome.as_str(),
            })
            .to_string(),
            now_ms,
        )?;
        Ok(RecordOutcome::Inserted)
    })
}

/// Every result recorded for one tree, newest row first.
pub fn results_for_tree(conn: &Connection, tree_hash: &str) -> StoreResult<Vec<CachedResult>> {
    let mut stmt = conn.prepare(
        "SELECT id, outcome, exit_code, duration_ms, log_path
           FROM verification_check WHERE tree_hash = ?1 ORDER BY rowid DESC",
    )?;
    let rows = stmt
        .query_map(params![tree_hash], |row| {
            let outcome: String = row.get(1)?;
            Ok((
                CachedResult {
                    id: row.get(0)?,
                    outcome: VerificationOutcome::Pass,
                    exit_code: row.get(2)?,
                    duration_ms: row.get(3)?,
                    log_path: row.get(4)?,
                },
                outcome,
            ))
        })?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;
    rows.into_iter()
        .map(|(mut result, outcome)| {
            result.outcome = outcome.parse::<VerificationOutcome>()?;
            Ok(result)
        })
        .collect::<Result<Vec<_>, StoreError>>()
}
