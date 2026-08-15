//! Revocation — §4.3's Scenario S, and acceptance row 25.
//!
//! > | State at revocation | Result |
//! > |---|---|
//! > | Not yet consumed | Effect never happens; run → `AWAITING_APPROVAL` |
//! > | `INTENDED`, effect not started | Aborted before starting |
//! > | `INTENDED`, effect in flight | **Cannot be cancelled.** Complete or fail it, record the receipt, halt with a finding |
//! > | `CONFIRMED` | Cannot be undone. Record revocation, raise `POST_HOC_REVOCATION` finding |
//!
//! Four rows, four outcomes, one function. They are not four ways of saying
//! "stop": the third *cannot* stop, and the fourth is too late to.
//!
//! # Where each row's state is read from
//!
//! Three of the four are decided by S5's **side-effect ledger**
//! (`side_effect`, `INTENDED|CONFIRMED|FAILED|AMBIGUOUS`). That ledger is the
//! existing mechanism for "did this effect happen?", and building a parallel
//! record here would be the two-mechanisms-for-one-problem the plan rejects
//! (§5.2's note on removing `COMMITTING`).
//!
//! **The ledger cannot distinguish rows 2 and 3.** Both are `INTENDED`; the
//! difference is whether the effect has begun, and only the process performing
//! it knows that. So [`InFlight`] is a parameter, supplied by the caller,
//! rather than a guess made here. A guess in this position would be exactly the
//! failure §4.7 forbids — recording *unknown* as *known*.
//!
//! # What revocation does **not** do to the run's state
//!
//! §4.3 row 1 says "run → `AWAITING_APPROVAL`", and §5.2's legality table only
//! draws that edge from `RECONCILING`. A run whose grant is revoked while it is
//! `RUNNING` cannot be moved there without violating "every exit from `RUNNING`
//! passes through `RECONCILING`", and a run already sitting in
//! `AWAITING_APPROVAL` cannot self-transition. So this module moves the run
//! **only when §5.2 draws the edge**, and otherwise reports [`Halt::Deferred`]:
//! the grant is `REVOKED` either way, so the effect cannot be authorized no
//! matter which state the run is in, and reconciliation routes it. Recorded as
//! a master-plan finding rather than resolved by force.

use conductor_core::effect::OperationId;
use conductor_core::{Fence, ReconciledRoute, RunState};
use rusqlite::Connection;

use super::store::{self, ApprovalError, ApprovalResult, GrantState};

/// `finding.kind` for §4.3 row 4.
pub const POST_HOC_REVOCATION: &str = "POST_HOC_REVOCATION";

/// `finding.kind` for §4.3 row 3 — the effect that could not be cancelled.
pub const REVOKED_WHILE_IN_FLIGHT: &str = "REVOKED_WHILE_IN_FLIGHT";

/// `finding.severity` for both. Neither auto-resolves (§4.8).
const SEVERITY: &str = "HIGH";

/// Whether the effect the grant authorizes is executing **right now**.
///
/// The ledger records `INTENDED` before the effect and the receipt after it, so
/// it cannot tell §4.3's row 2 from its row 3. Only the caller can, and it must
/// say rather than let this module infer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlight {
    /// The effect has not begun. It can still be abandoned.
    No,
    /// The effect is executing. §4.3: it **cannot be cancelled**.
    Yes,
}

/// What the run must do next, and whether §5.2 let it happen here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Halt {
    /// The run was moved. §5.2 draws the edge from where it was.
    Applied {
        /// Where it went.
        to: RunState,
    },
    /// The run is already there.
    AlreadyThere {
        /// Where it already is.
        at: RunState,
    },
    /// §5.2 draws no edge from the run's current state to the one §4.3 asks
    /// for. The grant is revoked regardless, so the effect cannot happen;
    /// reconciliation is what routes the run.
    Deferred {
        /// Where the run is.
        from: RunState,
        /// Where §4.3 wants it.
        wanted: RunState,
    },
    /// There is no run — §4.3's plan approval and review acceptance.
    NoRun,
}

/// Which of §4.3's four rows the revocation fell into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationOutcome {
    /// Row 1: not yet consumed. The effect never happens.
    NotYetConsumed {
        /// What became of the run.
        halt: Halt,
    },
    /// Row 2: `INTENDED`, effect not started. Aborted before starting, and the
    /// ledger row resolved `FAILED` so a restart does not re-check and perform
    /// it.
    AbortedBeforeStarting {
        /// The ledger row that was resolved.
        operation_id: OperationId,
        /// What became of the run.
        halt: Halt,
    },
    /// Row 3: `INTENDED`, effect in flight. **Cannot be cancelled.**
    CannotCancelInFlight {
        /// The ledger row, left `INTENDED` — the effect's outcome is still
        /// unknown and recording one would be a guess.
        operation_id: OperationId,
        /// The finding raised. Never auto-resolves (§4.8).
        finding_id: String,
    },
    /// Row 4: `CONFIRMED`. Cannot be undone.
    PostHocRevocation {
        /// The ledger row.
        operation_id: OperationId,
        /// The `POST_HOC_REVOCATION` finding.
        finding_id: String,
    },
    /// The grant was already revoked. Idempotent: a human hitting revoke twice
    /// is not an error, and the second call must not re-raise a finding.
    AlreadyRevoked {
        /// Which grant.
        grant_id: String,
    },
}

/// Revoke one grant — §4.3's four rows.
///
/// `operation` is the ledger row for the effect this grant authorizes, when one
/// exists. `in_flight` answers the one question the ledger cannot.
///
/// The fence is required because two of the four outcomes write **findings**
/// and one moves the **run**, and those are statements about a run (§4.7). The
/// grant row itself is written unfenced, for the reason
/// [`super::store`] records.
pub fn revoke(
    conn: &mut Connection,
    fence: &Fence,
    grant_id: &str,
    operation: Option<&OperationId>,
    in_flight: InFlight,
    now_ms: i64,
    reason: &str,
) -> ApprovalResult<RevocationOutcome> {
    let row = store::grant_row(conn, grant_id)?.ok_or_else(|| ApprovalError::NoSuchGrant {
        id: grant_id.to_string(),
    })?;
    if row.state == GrantState::Revoked {
        return Ok(RevocationOutcome::AlreadyRevoked {
            grant_id: grant_id.to_string(),
        });
    }

    let ledger = match operation {
        Some(id) => conductor_store::side_effect::side_effect(conn, id)
            .map_err(ApprovalError::Store)?
            .map(|effect| effect.state),
        None => None,
    };

    // Row 4 first: a confirmed effect cannot be undone, and §5.2 makes
    // `CONSUMED` terminal, so the grant does not move. The revocation is
    // recorded as a finding — which never auto-resolves — rather than as a
    // state change that would erase the fact that the effect happened.
    if ledger == Some(conductor_core::SideEffectState::Confirmed) {
        let operation_id = operation
            .expect("a ledger state implies an operation")
            .clone();
        let finding_id = raise(
            conn,
            fence,
            POST_HOC_REVOCATION,
            grant_id,
            &format!(
                "grant {grant_id} was revoked after its effect {operation_id} was \
                 CONFIRMED; the effect cannot be undone. reason: {reason}"
            ),
            now_ms,
        )?;
        return Ok(RevocationOutcome::PostHocRevocation {
            operation_id,
            finding_id,
        });
    }

    // Row 3: in flight. The grant is revoked so nothing *else* can be
    // authorized by it, but the effect already running is not cancellable and
    // its ledger row is left `INTENDED` — its outcome is genuinely unknown.
    if ledger == Some(conductor_core::SideEffectState::Intended) && in_flight == InFlight::Yes {
        let operation_id = operation
            .expect("a ledger state implies an operation")
            .clone();
        mark_revoked(conn, &row.state, grant_id, now_ms)?;
        let finding_id = raise(
            conn,
            fence,
            REVOKED_WHILE_IN_FLIGHT,
            grant_id,
            &format!(
                "grant {grant_id} was revoked while effect {operation_id} was in \
                 flight; §4.3: it cannot be cancelled — complete or fail it, record \
                 the receipt, and halt. reason: {reason}"
            ),
            now_ms,
        )?;
        return Ok(RevocationOutcome::CannotCancelInFlight {
            operation_id,
            finding_id,
        });
    }

    mark_revoked(conn, &row.state, grant_id, now_ms)?;

    // Row 2: intended but not started. Resolve the ledger row `FAILED` — the
    // effect did not happen and is *known* not to have happened, which is
    // exactly what `fail_effect` means. Leaving it `INTENDED` would put it in
    // §4.7's crash-window set, and a restart would re-check the precondition
    // and might then perform the very effect a human just revoked.
    if ledger == Some(conductor_core::SideEffectState::Intended) {
        let operation_id = operation
            .expect("a ledger state implies an operation")
            .clone();
        conductor_store::side_effect::fail_effect(
            conn,
            fence,
            &operation_id,
            &format!("aborted before starting: grant {grant_id} revoked ({reason})"),
            now_ms,
        )
        .map_err(ApprovalError::Store)?;
        let halt = halt_the_run(conn, fence, now_ms, reason)?;
        return Ok(RevocationOutcome::AbortedBeforeStarting { operation_id, halt });
    }

    // Row 1: never consumed, no effect declared. The effect never happens.
    let halt = halt_the_run(conn, fence, now_ms, reason)?;
    Ok(RevocationOutcome::NotYetConsumed { halt })
}

/// `GRANTED → REVOKED`, or `CONSUMED` left alone.
///
/// §5.2 makes `CONSUMED` terminal. A consumed grant whose effect never reached
/// the ledger is still consumed; the revocation is recorded by the caller's
/// finding, not by rewriting a terminal state.
fn mark_revoked(
    conn: &mut Connection,
    from: &GrantState,
    grant_id: &str,
    now_ms: i64,
) -> ApprovalResult<()> {
    if *from != GrantState::Granted {
        return Ok(());
    }
    store::transition_grant(
        conn,
        grant_id,
        GrantState::Granted,
        GrantState::Revoked,
        now_ms,
    )?;
    Ok(())
}

/// §4.3 row 1's "run → `AWAITING_APPROVAL`", applied only where §5.2 draws it.
///
/// §5.2's legality table draws `AWAITING_APPROVAL` from exactly one state,
/// `RECONCILING`, and `route_reconciled` refuses from anywhere else. So there
/// are three answers, not one, and the third is the honest one: the grant is
/// revoked, the effect cannot be authorized, and reconciliation — which every
/// exit from `RUNNING` passes through (§4.8) — is what routes the run.
fn halt_the_run(
    conn: &mut Connection,
    fence: &Fence,
    now_ms: i64,
    reason: &str,
) -> ApprovalResult<Halt> {
    let Some(state) = run_state(conn, fence)? else {
        return Ok(Halt::NoRun);
    };
    match state {
        RunState::AwaitingApproval => Ok(Halt::AlreadyThere { at: state }),
        RunState::Reconciling => {
            let to = conductor_store::lease::route_reconciled(
                conn,
                fence,
                ReconciledRoute::AwaitingApproval,
                reason,
                now_ms,
            )?;
            Ok(Halt::Applied { to })
        }
        _ => Ok(Halt::Deferred {
            from: state,
            wanted: RunState::AwaitingApproval,
        }),
    }
}

fn run_state(conn: &Connection, fence: &Fence) -> ApprovalResult<Option<RunState>> {
    use rusqlite::OptionalExtension;
    let stored: Option<String> = conn
        .query_row(
            "SELECT state FROM run WHERE id = ?1",
            rusqlite::params![fence.run_id().as_str()],
            |row| row.get(0),
        )
        .optional()?;
    stored
        .map(|state| {
            state
                .parse::<RunState>()
                .map_err(|e| ApprovalError::Unreadable {
                    id: fence.run_id().as_str().to_string(),
                    detail: e.to_string(),
                })
        })
        .transpose()
}

fn raise(
    conn: &mut Connection,
    fence: &Fence,
    kind: &str,
    grant_id: &str,
    evidence: &str,
    now_ms: i64,
) -> ApprovalResult<String> {
    // Derived from the grant and the kind, so a restart that revokes the same
    // grant again writes the same id and `INSERT OR IGNORE` keeps one finding
    // rather than accumulating duplicates of one fact.
    let id = format!("F-{kind}-{grant_id}");
    conductor_store::run::record_finding(conn, fence, &id, kind, SEVERITY, evidence, now_ms)?;
    Ok(id)
}
