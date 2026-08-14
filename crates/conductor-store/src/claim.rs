//! The atomic run claim — master plan §4.7.
//!
//! One `BEGIN IMMEDIATE` transaction containing a single `UPDATE … RETURNING`
//! with the selection subquery, plus the `RUN_CLAIMED` event insert. The
//! statement below is the plan's statement; ADR-0004 measured it and adopted it
//! as specified.
//!
//! `lease_epoch` is the fencing token and increments on claim; [`crate::lease`]
//! enforces it on every subsequent write.
//!
//! **S3 resolved §4.7's recorded contradiction.** The published predicate was
//! `state IN ('READY','RECOVERING')`, but `RECOVERING` is not one of §5.2's
//! states and the run state "mirrors its task", so half the predicate could
//! never match. Two changes, both driven by §5.2 being authoritative about which
//! transitions exist:
//!
//! * the predicate selects `RECONCILING` instead — §5.2's restart rule forces a
//!   crashed run there, so that is the state a restarting worker must be able to
//!   take;
//! * the claim **preserves** `RECONCILING` rather than overwriting it with
//!   `RUNNING`. §5.2 has no `RECONCILING → RUNNING` edge, and the state is the
//!   record that Conductor still owes this run a look at the repository. A claim
//!   that erased it would turn "reconciliation outstanding" into "running
//!   normally" at exactly the moment recovery depends on the difference.

use conductor_core::{EventKind, Fence, PolicyHash, RunClaimedPayload, RunId, RunState, TaskId};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};
use crate::tx::with_immediate;

/// A run this worker now owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimedRun {
    /// `run.state` **after** the claim.
    ///
    /// `RUNNING` for fresh work, `RECONCILING` for a run whose previous worker
    /// died before reconciling. The worker needs the difference: one spawns an
    /// agent, the other must not.
    pub state: RunState,
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

/// The claim statement — master plan §4.7 as resolved by S3 (see the module
/// documentation for why the two differences from the published text exist).
pub const CLAIM_SQL: &str = "\
UPDATE run
   SET state = CASE WHEN state='RECONCILING' THEN 'RECONCILING' ELSE 'RUNNING' END,
       lease_owner=?1, lease_expires_at=?2,
       lease_epoch = lease_epoch + 1
 WHERE id = (SELECT id FROM run
              WHERE state IN ('READY','RECONCILING')
                AND (lease_expires_at IS NULL OR lease_expires_at < ?3)
              ORDER BY priority, created_at LIMIT 1)
RETURNING id, task_id, policy_hash, lease_epoch, state";

/// Claim **one named run**, under the same predicate as [`CLAIM_SQL`].
///
/// Startup recovery needs the run it is recovering, not "the next one": §4.7's
/// nine steps walk a specific set of runs found by their state and lease. The
/// predicate is otherwise identical, and
/// `tests/claim.rs::the_targeted_claim_shares_the_general_claims_predicate`
/// holds the two together — two claim statements that could drift apart is
/// exactly the situation that produces a double-claimed run.
pub const CLAIM_ONE_SQL: &str = "\
UPDATE run
   SET state = CASE WHEN state='RECONCILING' THEN 'RECONCILING' ELSE 'RUNNING' END,
       lease_owner=?1, lease_expires_at=?2,
       lease_epoch = lease_epoch + 1
 WHERE id = (SELECT id FROM run
              WHERE id = ?4
                AND state IN ('READY','RECONCILING')
                AND (lease_expires_at IS NULL OR lease_expires_at < ?3)
              ORDER BY priority, created_at LIMIT 1)
RETURNING id, task_id, policy_hash, lease_epoch, state";

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
    claim_with(conn, CLAIM_SQL, None, owner, now_ms, lease_ms)
}

/// Claim one named run, or `Ok(None)` when it is not claimable.
pub fn claim_run(
    conn: &mut Connection,
    run_id: &RunId,
    owner: &str,
    now_ms: i64,
    lease_ms: i64,
) -> StoreResult<Option<ClaimedRun>> {
    claim_with(
        conn,
        CLAIM_ONE_SQL,
        Some(run_id.as_str().to_string()),
        owner,
        now_ms,
        lease_ms,
    )
}

fn claim_with(
    conn: &mut Connection,
    sql: &str,
    only: Option<String>,
    owner: &str,
    now_ms: i64,
    lease_ms: i64,
) -> StoreResult<Option<ClaimedRun>> {
    let lease_expires_at = now_ms + lease_ms;

    with_immediate(conn, |tx| {
        let returned = {
            let mut stmt = tx.prepare_cached(sql)?;
            let mut args: Vec<&dyn rusqlite::ToSql> = vec![&owner, &lease_expires_at, &now_ms];
            if let Some(id) = &only {
                args.push(id);
            }
            let rows = stmt
                .query_map(args.as_slice(), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
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

        let Some((run_id, task_id, policy_hash, lease_epoch, state)) = returned else {
            return Ok(None);
        };

        let claimed = ClaimedRun {
            run_id: RunId::new(run_id)?,
            task_id: TaskId::new(task_id)?,
            policy_hash: PolicyHash::new(policy_hash)?,
            state: state.parse::<RunState>()?,
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

impl ClaimedRun {
    /// Authority to write to this run at the epoch the claim established.
    ///
    /// Every write the worker makes afterwards takes one of these, so a write
    /// that forgot to fence does not compile rather than silently racing.
    pub fn fence(&self) -> Fence {
        Fence::new(self.run_id.clone(), self.lease_epoch)
    }

    /// Whether this claim is a run that still owes a reconciliation.
    pub fn owes_reconciliation(&self) -> bool {
        self.state == RunState::Reconciling
    }
}
