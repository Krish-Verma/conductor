//! What repair observed about each attempt — schema v5, master plan §4.6/§4.7.
//!
//! §4.6 decides three loop-breakers from the history of a run's attempts, and
//! §4.6's acceptance property bounds *agent invocations*. §4.7's premise is that
//! the process running that loop is killed and restarted. A history that lived
//! only in memory would therefore satisfy neither: every restart would read an
//! empty history, every loop-breaker would look at one attempt, and the bound on
//! invocations would be a bound per process rather than per run.
//!
//! So the observations are rows. This module writes and reads them, and nothing
//! else: the interpretation — which failure is the same failure, whether the
//! loop progressed — belongs to `conductor-run`'s `repair` module, which is
//! where §4.6's definitions live.
//!
//! **Fenced, like every other write about a run** (§4.7). There is no unfenced
//! write path in this crate, and an observation is a statement about a run.

use conductor_core::{Fence, RunId};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::StoreResult;
use crate::lease::check_fence;
use crate::tx::with_immediate;

/// What repair needs recorded about one attempt that did not succeed.
///
/// The three fields §4.6 hashes — `failing_checks`, `assertion`, `tree_hash` —
/// are carried as inputs, and `fingerprint` alongside them as the digest the
/// writer computed. See [`crate::schema::SCHEMA_V5`] for why the digest is
/// stored and why nothing may read it back to decide anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRepairObservation {
    /// `attempt.id` this observation is about.
    pub attempt_id: String,
    /// `attempt.ordinal`, which is also the order observations are read in.
    pub ordinal: i64,
    /// `FAILED` | `NO_CHANGE` | `CRASHED` | `INFRASTRUCTURE`.
    pub kind: String,
    /// The ids of the checks that failed, **sorted** — §4.6 hashes
    /// `sorted(failing_check_ids)`, and sorting at the write keeps the stored
    /// row and the digest talking about the same sequence.
    pub failing_checks: Vec<String>,
    /// The first failing assertion, as read from the check's log.
    pub assertion: String,
    /// The tree the checks observed.
    pub tree_hash: String,
    /// The digest the writer derived from the three fields above.
    pub fingerprint: String,
}

/// One `repair_observation` row as read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairObservationRow {
    /// The run.
    pub run_id: RunId,
    /// `attempt.id`.
    pub attempt_id: String,
    /// `attempt.ordinal`.
    pub ordinal: i64,
    /// `FAILED` | `NO_CHANGE` | `CRASHED` | `INFRASTRUCTURE`.
    pub kind: String,
    /// The ids of the checks that failed, sorted.
    pub failing_checks: Vec<String>,
    /// The first failing assertion.
    pub assertion: String,
    /// The tree the checks observed.
    pub tree_hash: String,
    /// The digest as it was stored. **Evidence for a reader, never an input to
    /// a decision** — callers rebuild the failure from the three inputs and
    /// recompute.
    pub stored_fingerprint: String,
    /// When the observation was written.
    pub recorded_at: i64,
}

/// Record one observation. Fenced.
///
/// `INSERT OR IGNORE`, for the reason `record_finding` uses it: a restart that
/// re-derives an observation for an attempt that already has one is re-deriving
/// it from the same durable attempt and the same verification rows, so the
/// second write carries the same facts. Failing there would turn a benign
/// crash-window replay into a stuck run, and overwriting would let a later,
/// less-informed pass erase the record made when the evidence was fresh.
pub fn record_observation(
    conn: &mut Connection,
    fence: &Fence,
    observation: &NewRepairObservation,
    now_ms: i64,
) -> StoreResult<()> {
    let mut checks = observation.failing_checks.clone();
    checks.sort();
    let encoded = serde_json::to_string(&checks)?;
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "INSERT OR IGNORE INTO repair_observation
               (attempt_id, run_id, ordinal, kind, failing_checks, assertion,
                tree_hash, fingerprint, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                observation.attempt_id,
                fence.run_id().as_str(),
                observation.ordinal,
                observation.kind,
                encoded,
                observation.assertion,
                observation.tree_hash,
                observation.fingerprint,
                now_ms,
            ],
        )?;
        Ok(())
    })
}

/// Every observation of one run, **oldest attempt first**.
///
/// The order is the whole point: §4.6's oscillation window is "the last 4", and
/// a window over an unordered read is not a window.
pub fn observations_for_run(
    conn: &Connection,
    run_id: &RunId,
) -> StoreResult<Vec<RepairObservationRow>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, attempt_id, ordinal, kind, failing_checks, assertion,
                tree_hash, fingerprint, recorded_at
           FROM repair_observation WHERE run_id = ?1 ORDER BY ordinal",
    )?;
    let mut rows = stmt.query(params![run_id.as_str()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let checks: String = row.get(4)?;
        out.push(RepairObservationRow {
            run_id: RunId::new(row.get::<_, String>(0)?)?,
            attempt_id: row.get(1)?,
            ordinal: row.get(2)?,
            kind: row.get(3)?,
            failing_checks: serde_json::from_str(&checks)?,
            assertion: row.get(5)?,
            tree_hash: row.get(6)?,
            stored_fingerprint: row.get(7)?,
            recorded_at: row.get(8)?,
        });
    }
    Ok(out)
}
