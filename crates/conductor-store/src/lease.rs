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

/// Move a run out of `required` into the route's state, checking §5.2's table.
///
/// Two guards, and they catch different mistakes. The `state = ?` clause catches
/// a caller that skipped a state entirely — `RECONCILING` is "mandatory and
/// unskippable". The legality check catches a caller that is in the right state
/// and asked for a destination the machine does not draw; without it,
/// `route_reconciled` would happily write `COMPLETE` given a token, and §5.2
/// puts `COMPLETE` downstream of `VERIFYING`.
fn advance_from(
    conn: &mut Connection,
    fence: &Fence,
    required: RunState,
    route: conductor_core::ReconciledRoute,
    detail: &str,
    now_ms: i64,
) -> StoreResult<RunState> {
    let next = route.state();
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
            route.state(),
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
