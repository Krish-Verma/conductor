//! The atomic run claim — master plan §4.7.
//!
//! One `BEGIN IMMEDIATE` transaction containing a single `UPDATE … RETURNING`
//! with the selection subquery, plus the `RUN_CLAIMED` event insert. The
//! statement below is the plan's statement; ADR-0004 measured it and adopted it
//! as specified.
//!
//! `lease_epoch` is the fencing token and increments on claim. S1 stores it.
//! Enforcing it on subsequent writes, expiring leases and heartbeating are S3.

use conductor_core::{EventKind, PolicyHash, RunClaimedPayload, RunId, TaskId};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};
use crate::tx::with_immediate;

/// A run this worker now owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimedRun {
    /// `run.id`.
    pub run_id: RunId,
    /// `run.task_id`.
    pub task_id: TaskId,
    /// `run.policy_hash`.
    pub policy_hash: PolicyHash,
    /// `run.lease_epoch` after the claim — the fencing token.
    pub lease_epoch: i64,
    /// `run.lease_owner` as written.
    pub lease_owner: String,
    /// `run.lease_expires_at` as written, epoch milliseconds.
    pub lease_expires_at: i64,
}

/// The claim statement, verbatim from master plan §4.7.
pub const CLAIM_SQL: &str = "\
UPDATE run
   SET state='RUNNING', lease_owner=?1, lease_expires_at=?2,
       lease_epoch = lease_epoch + 1
 WHERE id = (SELECT id FROM run
              WHERE state IN ('READY','RECOVERING')
                AND (lease_expires_at IS NULL OR lease_expires_at < ?3)
              ORDER BY priority, created_at LIMIT 1)
RETURNING id, task_id, policy_hash, lease_epoch";

/// `event.kind` written by a successful claim.
pub const CLAIM_EVENT_KIND: EventKind = EventKind::RunClaimed;

/// Claim the next eligible run, or `Ok(None)` when there is none.
///
/// `now_ms` and `lease_ms` are supplied by the caller rather than read from the
/// clock here, so that the transaction is a pure function of its inputs and the
/// tests can pin time.
pub fn claim_next_run(
    conn: &mut Connection,
    owner: &str,
    now_ms: i64,
    lease_ms: i64,
) -> StoreResult<Option<ClaimedRun>> {
    let lease_expires_at = now_ms + lease_ms;

    with_immediate(conn, |tx| {
        let returned = {
            let mut stmt = tx.prepare_cached(CLAIM_SQL)?;
            let rows = stmt
                .query_map(params![owner, lease_expires_at, now_ms], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, rusqlite::Error>>()?;
            // LIMIT 1 in the subquery makes this impossible; assert it anyway,
            // because "impossible" is what a duplicate claim would be.
            if rows.len() > 1 {
                return Err(StoreError::ClaimMatchedMultipleRows(rows.len()));
            }
            rows.into_iter().next()
        };

        let Some((run_id, task_id, policy_hash, lease_epoch)) = returned else {
            return Ok(None);
        };

        let claimed = ClaimedRun {
            run_id: RunId::new(run_id)?,
            task_id: TaskId::new(task_id)?,
            policy_hash: PolicyHash::new(policy_hash)?,
            lease_epoch,
            lease_owner: owner.to_string(),
            lease_expires_at,
        };

        // Same transaction as the UPDATE: the evidence and the ownership move
        // together or not at all. BEGIN IMMEDIATE serialises writers, so
        // MAX(seq)+1 cannot race.
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM event WHERE run_id = ?1",
            params![claimed.run_id.as_str()],
            |row| row.get(0),
        )?;
        let payload = serde_json::to_string(&claim_payload(&claimed))?;
        tx.execute(
            "INSERT INTO event (run_id, seq, kind, payload, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                claimed.run_id.as_str(),
                seq,
                CLAIM_EVENT_KIND.as_str(),
                payload,
                now_ms
            ],
        )?;

        Ok(Some(claimed))
    })
}

/// The payload written with the `RUN_CLAIMED` event.
pub fn claim_payload(claimed: &ClaimedRun) -> RunClaimedPayload {
    RunClaimedPayload {
        run_id: claimed.run_id.clone(),
        lease_owner: claimed.lease_owner.clone(),
        lease_epoch: claimed.lease_epoch,
        lease_expires_at: claimed.lease_expires_at,
    }
}
