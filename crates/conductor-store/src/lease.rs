//! Leases, fencing and the lease timer — master plan §4.7.
//!
//! Three rules, all from §4.7:
//!
//! 1. **Leases are 60 s.** The duration is coupled to the worst-case claim
//!    latency, which S1 measured at **5147 ms** under `rusqlite` (M26,
//!    ADR-0005) — a ~12× margin. Shrinking the lease without re-deriving that
//!    number is how a healthy worker gets its run stolen mid-agent.
//! 2. **`lease_epoch` is the fencing token.** Every write below takes a
//!    [`Fence`] and is rejected when the epoch has moved. There is no unfenced
//!    write path in this crate.
//! 3. **Expiry is one of the two timers** (`WHERE … < now()` on a tick), and it
//!    forces the run to `RECONCILING` per §5.2's restart rule.
//!
//! **Expiry moves the epoch.** §4.7 only says a claim increments it, which
//! leaves a window: between a lease lapsing and a successor claiming, the stale
//! worker's epoch is still current, so its writes would land. Bumping at expiry
//! closes the window at the moment ownership is revoked instead of at the moment
//! somebody else takes it. `tests/fencing.rs` pins both orderings.

use conductor_core::{EventKind, Fence, RunId, RunState, TerminalAttempt};
use rusqlite::{Connection, Transaction, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};
use crate::tx::with_immediate;

/// Master plan §4.7: leases are 60 s.
pub const LEASE_MS: i64 = 60_000;

/// Master plan §4.7: heartbeat every 15 s, conditional on the child being alive.
pub const HEARTBEAT_MS: i64 = 15_000;

/// A run whose lease lapsed and which the sweep forced to `RECONCILING`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpiredLease {
    /// The run.
    pub run_id: RunId,
    /// Who held the lease, when anybody did.
    pub previous_owner: Option<String>,
    /// The state the run was in when the lease lapsed.
    pub previous_state: RunState,
    /// The epoch after the sweep. The previous owner's epoch is now stale.
    pub lease_epoch: i64,
}

/// Verify a fence inside an open transaction, or fail.
///
/// Returns the epoch the database actually holds so the error can say what the
/// disagreement was. A missing run is fenced out too: writing to a run that is
/// not there is never right.
pub(crate) fn check_fence(tx: &Transaction<'_>, fence: &Fence) -> StoreResult<()> {
    let actual: Option<i64> = tx
        .query_row(
            "SELECT lease_epoch FROM run WHERE id = ?1",
            params![fence.run_id().as_str()],
            |row| row.get(0),
        )
        .ok();
    match actual {
        Some(epoch) if epoch == fence.lease_epoch() => Ok(()),
        other => Err(StoreError::FencedOut {
            expected: fence.lease_epoch(),
            actual: other,
        }),
    }
}

/// Append one `event` row for a run, inside an open transaction.
///
/// `seq` is `MAX(seq)+1` for the run. `BEGIN IMMEDIATE` serialises writers, so
/// the read-then-write cannot race, and `ix_event_run` is UNIQUE so a mistake
/// fails at INSERT time rather than being noticed later by a checker.
pub(crate) fn append_event(
    tx: &Transaction<'_>,
    run_id: &RunId,
    kind: EventKind,
    payload: &str,
    now_ms: i64,
) -> StoreResult<i64> {
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM event WHERE run_id = ?1",
        params![run_id.as_str()],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO event (run_id, seq, kind, payload, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![run_id.as_str(), seq, kind.as_str(), payload, now_ms],
    )?;
    Ok(seq)
}

/// Extend the lease this worker holds.
///
/// The caller must have observed its child alive first (§4.7: "a supervisor that
/// heartbeats while its child is dead is worse than one that crashes"). That
/// obligation is enforced one level up, in `conductor-run`'s heartbeat, which
/// takes a liveness witness; this function is the durable half.
pub fn renew_lease(
    conn: &mut Connection,
    fence: &Fence,
    now_ms: i64,
    lease_ms: i64,
) -> StoreResult<i64> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        let expires_at = now_ms + lease_ms;
        tx.execute(
            "UPDATE run SET lease_expires_at = ?2 WHERE id = ?1 AND lease_epoch = ?3",
            params![fence.run_id().as_str(), expires_at, fence.lease_epoch()],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::LeaseRenewed,
            &format!("{{\"lease_expires_at\":{expires_at}}}"),
            now_ms,
        )?;
        Ok(expires_at)
    })
}

/// The lease timer: force every run whose lease has lapsed to `RECONCILING`.
///
/// §4.7 step 2 scans `RUNNING`/`RECONCILING`/`VERIFYING`; §5.2's restart rule
/// says the destination is `RECONCILING`. A `RECONCILING` run is included even
/// though its state does not change, because the epoch must still move: its old
/// owner is gone either way.
pub fn expire_leases(conn: &mut Connection, now_ms: i64) -> StoreResult<Vec<ExpiredLease>> {
    with_immediate(conn, |tx| {
        let states: Vec<&str> = RunState::LEASE_BEARING.iter().map(|s| s.as_str()).collect();
        let placeholders = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE run
                SET state = 'RECONCILING',
                    lease_owner = NULL,
                    lease_expires_at = NULL,
                    lease_epoch = lease_epoch + 1
              WHERE state IN ({placeholders})
                AND lease_expires_at IS NOT NULL
                AND lease_expires_at < ?{}
              RETURNING id, lease_epoch",
            states.len() + 1
        );

        // Read the pre-image first: RETURNING reports the new row, and the
        // report has to say what the run was doing when its worker vanished.
        let before = {
            let select = format!(
                "SELECT id, state, lease_owner FROM run
                  WHERE state IN ({placeholders})
                    AND lease_expires_at IS NOT NULL
                    AND lease_expires_at < ?{}",
                states.len() + 1
            );
            let mut stmt = tx.prepare(&select)?;
            let mut params: Vec<&dyn rusqlite::ToSql> =
                states.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            params.push(&now_ms);
            stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?
        };

        let epochs = {
            let mut stmt = tx.prepare(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> =
                states.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            params.push(&now_ms);
            stmt.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?
        };

        let mut expired = Vec::with_capacity(before.len());
        for (id, previous_state, previous_owner) in before {
            let lease_epoch = epochs
                .iter()
                .find(|(rid, _)| rid == &id)
                .map(|(_, e)| *e)
                .ok_or_else(|| {
                    StoreError::Domain(format!("run {id} was selected for expiry but not updated"))
                })?;
            let run_id = RunId::new(id)?;
            let record = ExpiredLease {
                run_id: run_id.clone(),
                previous_owner,
                previous_state: previous_state.parse::<RunState>()?,
                lease_epoch,
            };
            let payload = serde_json::to_string(&record)?;
            append_event(tx, &run_id, EventKind::LeaseExpired, &payload, now_ms)?;
            expired.push(record);
        }
        Ok(expired)
    })
}

/// `RUNNING → RECONCILING`, the only exit from `RUNNING` (§4.8).
///
/// Takes a [`TerminalAttempt`] because §5.2 requires evidence for the
/// transition, and that type cannot be constructed from an attempt that is still
/// running. The destination is not a parameter: there is nowhere else to go.
pub fn advance_to_reconciling(
    conn: &mut Connection,
    fence: &Fence,
    evidence: &TerminalAttempt,
    now_ms: i64,
) -> StoreResult<RunState> {
    let next = RunState::leave_running(evidence);
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "UPDATE run SET state = ?2 WHERE id = ?1 AND lease_epoch = ?3",
            params![fence.run_id().as_str(), next.as_str(), fence.lease_epoch()],
        )?;
        let payload = format!(
            "{{\"to\":\"{}\",\"attempt\":\"{}\",\"outcome\":\"{}\"}}",
            next.as_str(),
            evidence.attempt_id(),
            evidence.outcome()
        );
        append_event(
            tx,
            fence.run_id(),
            EventKind::RunStateChanged,
            &payload,
            now_ms,
        )?;
        Ok(next)
    })
}

/// Route a reconciled run onwards (§4.8).
///
/// The statement requires the run to *be* in `RECONCILING`, so the step cannot
/// be skipped by a caller that never went there.
///
/// **On `COMPLETE`.** S3's version of this comment said the route type had no
/// `COMPLETE` variant and therefore this could not write a terminal state. S4
/// adds one, and the guarantee moves rather than disappearing: the variant
/// carries a `VerifiedComplete`, which only `completion::evaluate` can mint, so
/// reaching `COMPLETE` still requires verification bound to a tree hash (§5.2).
/// This function deliberately does not re-check that — it would be checking a
/// proof it was handed — and it is not where the check belongs.
pub fn route_reconciled(
    conn: &mut Connection,
    fence: &Fence,
    route: conductor_core::ReconciledRoute,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    advance_from(conn, fence, RunState::Reconciling, route, detail, now_ms)
}

/// Leave `VERIFYING` — §5.2's "`VERIFYING → COMPLETE` requires §4.5's seven
/// criteria".
///
/// The same shape as [`route_reconciled`] and for the same reason: the statement
/// requires the run to *be* in `VERIFYING`, so a caller cannot complete a run
/// that never verified. The evidence is in the route — `ReconciledRoute::
/// Complete` carries a `VerifiedComplete`, whose only constructor is the
/// completion gate — and this function deliberately does not re-check it. It
/// would be checking a proof it was handed.
pub fn route_verified(
    conn: &mut Connection,
    fence: &Fence,
    route: conductor_core::ReconciledRoute,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    advance_from(conn, fence, RunState::Verifying, route, detail, now_ms)
}

/// Re-open a `REPAIRING` run for another agent attempt — §4.6's repair edge.
///
/// **`REPAIRING → READY`, not `REPAIRING → RECONCILING`.** §5.2's diagram draws
/// the latter and it cannot work: §4.6's repair *is* an agent invocation, and
/// §5.2's only route to one is `READY ──claim+eligibility──► RUNNING`; §4.7's
/// claim additionally preserves `RECONCILING` instead of setting `RUNNING`, so a
/// run pushed to `RECONCILING` would be re-reconciled with no agent ever
/// running. `conductor-core`'s legality table already records `READY` as the
/// edge S6 owns; this is the run-row half of it.
///
/// It goes through [`advance_state`] — the same statement, the same fence check,
/// the same §5.2 legality check — precisely so that S6 does not acquire a
/// private way to write `run.state`. A second, weaker path to the column is how
/// the guarantees the first one enforces stop being guarantees.
pub fn reopen_for_repair(
    conn: &mut Connection,
    fence: &Fence,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    advance_state(
        conn,
        fence,
        RunState::Repairing,
        RunState::Ready,
        detail,
        now_ms,
    )
}

/// Hand a `REPAIRING` run to a person — §4.6's `escalate_after`.
///
/// The other exit from `REPAIRING`, and the one every bound in §4.6 eventually
/// takes: a loop-breaker fired, the budget ran out, or the invocation ceiling
/// was reached. Same statement and same checks as [`reopen_for_repair`].
pub fn escalate_from_repairing(
    conn: &mut Connection,
    fence: &Fence,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    advance_state(
        conn,
        fence,
        RunState::Repairing,
        RunState::AwaitingReview,
        detail,
        now_ms,
    )
}

/// Move a run out of `required` into the route's state, checking §5.2's table.
fn advance_from(
    conn: &mut Connection,
    fence: &Fence,
    required: RunState,
    route: conductor_core::ReconciledRoute,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    advance_state(conn, fence, required, route.state(), detail, now_ms)
}

/// Move a run from `required` to `next`, checking §5.2's table.
///
/// Two guards, and they catch different mistakes. The `state = ?` clause catches
/// a caller that skipped a state entirely — `RECONCILING` is "mandatory and
/// unskippable". The legality check catches a caller that is in the right state
/// and asked for a destination the machine does not draw; without it,
/// `route_reconciled` would happily write `COMPLETE` given a token, and §5.2
/// puts `COMPLETE` downstream of `VERIFYING`.
fn advance_state(
    conn: &mut Connection,
    fence: &Fence,
    required: RunState,
    next: RunState,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    required
        .as_task_state()
        .transition_to(next.as_task_state())
        .map_err(|e| StoreError::IllegalTaskTransition(e.to_string()))?;

    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        let changed = tx.execute(
            "UPDATE run SET state = ?2 WHERE id = ?1 AND lease_epoch = ?3 AND state = ?4",
            params![
                fence.run_id().as_str(),
                next.as_str(),
                fence.lease_epoch(),
                required.as_str(),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotInState {
                run_id: fence.run_id().as_str().to_string(),
                required,
            });
        }
        let payload = format!(
            "{{\"to\":\"{}\",\"route\":\"{}\",\"detail\":{}}}",
            next.as_str(),
            next.as_str(),
            serde_json::to_string(detail)?
        );
        append_event(
            tx,
            fence.run_id(),
            EventKind::RunStateChanged,
            &payload,
            now_ms,
        )?;
        Ok(next)
    })
}

/// Append one evidence row, fenced.
pub fn record_event(
    conn: &mut Connection,
    fence: &Fence,
    kind: EventKind,
    payload: &str,
    now_ms: i64,
) -> StoreResult<i64> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        append_event(tx, fence.run_id(), kind, payload, now_ms)
    })
}

/// Release the lease voluntarily: the worker is done with this run.
///
/// Clears the owner and the expiry but does **not** move the epoch, because
/// nothing was revoked — the worker is handing the run back. The next claim
/// moves it.
pub fn release_lease(conn: &mut Connection, fence: &Fence, now_ms: i64) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "UPDATE run SET lease_owner = NULL, lease_expires_at = NULL
              WHERE id = ?1 AND lease_epoch = ?2",
            params![fence.run_id().as_str(), fence.lease_epoch()],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::LeaseRenewed,
            "{\"released\":true}",
            now_ms,
        )?;
        Ok(())
    })
}

/// Refuse to launch a `READY` run whose measured execution capabilities do not
/// meet its requirements — §4.2's gate, acceptance row 30.
///
/// **Unfenced, deliberately, and this is the only unfenced state write in the
/// module.** A fence proves a worker still holds the lease it was given; a
/// `READY` run has no lease and no owner, so there is no token to be stale
/// against and demanding one would mean claiming the run first. Claiming it
/// would move it to `RUNNING`, and §4.8's "every exit from `RUNNING` passes
/// through reconciliation" is the invariant that makes an agent's self-report
/// non-authoritative — a run that never launched an agent has nothing to
/// reconcile, so putting it through `RUNNING` to reach `BLOCKED` would weaken a
/// load-bearing statement in order to satisfy bookkeeping.
///
/// Concurrency is handled by the same mechanism the claim itself uses: the
/// `WHERE state = 'READY'` clause. Exactly one caller can win, and a caller that
/// loses sees `changed == 0` and is told the run was not `READY`, rather than
/// blocking a run somebody else has already launched.
///
/// The finding and the state change are one transaction, because a run blocked
/// with no recorded reason is a run nobody can act on, and a finding attached to
/// a run that is still `READY` would be a refusal that did not refuse.
pub fn refuse_ineligible_launch(
    conn: &mut Connection,
    run_id: &RunId,
    finding_id: &str,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    // §5.2's table is still consulted: this must be an edge the machine draws.
    RunState::Ready
        .as_task_state()
        .transition_to(RunState::Blocked.as_task_state())
        .map_err(|e| StoreError::IllegalTaskTransition(e.to_string()))?;

    with_immediate(conn, |tx| {
        let changed = tx.execute(
            "UPDATE run SET state = 'BLOCKED' WHERE id = ?1 AND state = 'READY'",
            params![run_id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotInState {
                run_id: run_id.as_str().to_string(),
                required: RunState::Ready,
            });
        }
        tx.execute(
            "INSERT OR IGNORE INTO finding
               (id, run_id, kind, severity, evidence_ref, resolution, created_at)
             VALUES (?1, ?2, 'INELIGIBLE_EXECUTION_MODE', 'CRITICAL', ?3, NULL, ?4)",
            params![finding_id, run_id.as_str(), detail, now_ms],
        )?;
        append_event(
            tx,
            run_id,
            EventKind::FindingRaised,
            &format!(
                "{{\"finding\":{},\"kind\":\"INELIGIBLE_EXECUTION_MODE\",\"severity\":\"CRITICAL\"}}",
                serde_json::to_string(finding_id)?
            ),
            now_ms,
        )?;
        append_event(
            tx,
            run_id,
            EventKind::RunStateChanged,
            &format!(
                "{{\"to\":\"BLOCKED\",\"route\":\"BLOCKED\",\"detail\":{}}}",
                serde_json::to_string(detail)?
            ),
            now_ms,
        )?;
        Ok(RunState::Blocked)
    })
}

/// Re-enter reconciliation after a human answered an approval request —
/// acceptance rows 12 and 13's "resumes on grant".
///
/// **Unfenced, and guarded by state**, for the same reason as
/// [`refuse_ineligible_launch`]: a run in `AWAITING_APPROVAL` has released its
/// lease and is waiting for a person, so there is no lease-holder to fence
/// against. `WHERE state = 'AWAITING_APPROVAL'` is the guard, and two operators
/// answering the same request at the same moment produce one winner.
///
/// **`RECONCILING`, not `READY`.** `READY` means "an agent may be launched",
/// and launching one would re-capture the baseline from a workspace that
/// already contains the approved work — which reconciles as `NO_CHANGE` and
/// discards exactly what the human authorised. `RECONCILING` rejoins §4.7's
/// recovery path, which compares against the **stored baseline artifact**, so
/// the approved change is still a change. See `conductor-core`'s legality table
/// for the full argument.
///
/// This does **not** consume the grant. §4.3 is explicit that a grant is
/// consumed "immediately before the side effect", and the side effect here is
/// whatever the run does after it reconciles. The consumption happens at the
/// policy gate on the way through, where the binding is recomputed — so a run
/// resumed on a grant that has since been revoked does not proceed, which is
/// what makes row 25 reachable.
pub fn resume_after_grant(
    conn: &mut Connection,
    run_id: &RunId,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    RunState::AwaitingApproval
        .as_task_state()
        .transition_to(RunState::Reconciling.as_task_state())
        .map_err(|e| StoreError::IllegalTaskTransition(e.to_string()))?;

    with_immediate(conn, |tx| {
        let changed = tx.execute(
            "UPDATE run SET state = 'RECONCILING' WHERE id = ?1 AND state = 'AWAITING_APPROVAL'",
            params![run_id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotInState {
                run_id: run_id.as_str().to_string(),
                required: RunState::AwaitingApproval,
            });
        }
        append_event(
            tx,
            run_id,
            EventKind::RunStateChanged,
            &format!(
                "{{\"to\":\"RECONCILING\",\"route\":\"RECONCILING\",\"detail\":{}}}",
                serde_json::to_string(detail)?
            ),
            now_ms,
        )?;
        Ok(RunState::Reconciling)
    })
}
