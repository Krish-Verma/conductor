//! §5.2's task machine and S5's minimal task spec.
//!
//! The property that matters most here is the one §5.2 states as an **invalid**
//! transition: `RUNNING → COMPLETE`. S3 made that unrepresentable for a *run* by
//! typing the exit from `RUNNING` (`leave_running` takes a `TerminalAttempt` and
//! returns one destination) and S4 kept it by making `ReconciledRoute::Complete`
//! carry a token only the completion gate can mint. Neither of those covers the
//! `task` row, which is a second place the same lie can be written — so the
//! legality table below is the task's half of the same guarantee.

use conductor_core::TaskState;
use conductor_core::task::{TaskSpec, TaskSpecError, TransitionError};

#[test]
fn running_cannot_reach_complete_without_reconciling() {
    // §5.2, "Invalid: `RUNNING → COMPLETE`". Stated first because it is the one
    // the whole slice exists to preserve.
    let error = TaskState::Running
        .transition_to(TaskState::Complete)
        .expect_err("RUNNING → COMPLETE must be refused");
    assert_eq!(
        error,
        TransitionError {
            from: TaskState::Running,
            to: TaskState::Complete,
        }
    );

    // And the route that *is* legal goes through reconciliation.
    assert!(
        TaskState::Running
            .transition_to(TaskState::Reconciling)
            .is_ok()
    );
    assert!(
        TaskState::Reconciling
            .transition_to(TaskState::Verifying)
            .is_ok()
    );
    assert!(
        TaskState::Verifying
            .transition_to(TaskState::Complete)
            .is_ok()
    );
}

#[test]
fn reconciling_is_the_only_exit_from_running() {
    // §4.8: "**Every exit from `RUNNING` passes through it** — success, crash,
    // timeout, cancel." Cancellation is a human action and §5.2 gives `→
    // CANCELLED` to the human from anywhere, so it is the single exception the
    // diagram itself draws.
    for state in TaskState::ALL {
        let legal = TaskState::Running.transition_to(*state).is_ok();
        let expected = matches!(*state, TaskState::Reconciling | TaskState::Cancelled);
        assert_eq!(
            legal,
            expected,
            "RUNNING → {state} should be {}",
            if expected { "legal" } else { "refused" }
        );
    }
}

#[test]
fn a_terminal_state_never_moves_again() {
    // §5.2: "Terminal: `COMPLETE`, `CANCELLED`, `SUPERSEDED`."
    for terminal in [
        TaskState::Complete,
        TaskState::Cancelled,
        TaskState::Superseded,
    ] {
        for target in TaskState::ALL {
            assert!(
                terminal.transition_to(*target).is_err(),
                "{terminal} is terminal but was allowed to move to {target}"
            );
        }
    }
}

#[test]
fn the_happy_path_of_the_slice_is_legal_end_to_end() {
    // PENDING → READY → RUNNING → RECONCILING → VERIFYING → COMPLETE.
    let path = [
        TaskState::Pending,
        TaskState::Ready,
        TaskState::Running,
        TaskState::Reconciling,
        TaskState::Verifying,
        TaskState::Complete,
    ];
    for pair in path.windows(2) {
        pair[0]
            .transition_to(pair[1])
            .unwrap_or_else(|e| panic!("the slice's own happy path is illegal: {e}"));
    }
}

#[test]
fn every_state_the_machine_draws_is_reachable_from_pending() {
    // A legality table with an unreachable state is a table with a typo in it.
    // Breadth-first from PENDING; every one of the twelve must be found.
    let mut seen = vec![TaskState::Pending];
    let mut frontier = vec![TaskState::Pending];
    while let Some(state) = frontier.pop() {
        for next in TaskState::ALL {
            if state.transition_to(*next).is_ok() && !seen.contains(next) {
                seen.push(*next);
                frontier.push(*next);
            }
        }
    }
    for state in TaskState::ALL {
        assert!(
            seen.contains(state),
            "{state} is in §5.2's diagram but nothing can reach it"
        );
    }
    assert_eq!(seen.len(), 12);
}

#[test]
fn a_state_never_transitions_to_itself() {
    // Not pedantry: `route_reconciled` writes a state and a self-transition
    // would let a caller "advance" a run without anything having happened.
    for state in TaskState::ALL {
        assert!(
            state.transition_to(*state).is_err(),
            "{state} → {state} must not be a transition"
        );
    }
}

// ---------------------------------------------------------------------------
// The task spec (S5's minimal file — S11 replaces it with the plan ledger).
// ---------------------------------------------------------------------------

fn valid_spec() -> TaskSpec {
    TaskSpec {
        id: "T-0012".to_string(),
        objective: "Add a greeting helper".to_string(),
        scope: vec!["src/**".to_string()],
        verification_profile: "verification.yaml".to_string(),
        attempt_budget: 3,
    }
}

#[test]
fn a_valid_spec_carries_exactly_what_a_run_needs() {
    let spec = valid_spec();
    let validated = spec.validate().expect("valid");
    assert_eq!(validated.id().as_str(), "T-0012");
    assert_eq!(validated.scope(), ["src/**"]);
    assert_eq!(validated.attempt_budget(), 3);
    assert_eq!(validated.verification_profile(), "verification.yaml");
}

#[test]
fn a_spec_with_no_scope_is_refused() {
    // An empty scope glob list makes every change out of scope, so nothing could
    // ever reconcile clean — but worse, a *missing* scope is indistinguishable
    // from "everything is permitted" to a reader. §3.7's `plan validate` refuses
    // "scope globs matching no path" for the same reason; this is the smallest
    // version of that rule the slice can hold.
    let spec = TaskSpec {
        scope: Vec::new(),
        ..valid_spec()
    };
    assert_eq!(
        spec.validate().expect_err("must be refused"),
        TaskSpecError::EmptyScope
    );
}

#[test]
fn a_spec_with_no_attempt_budget_is_refused() {
    let spec = TaskSpec {
        attempt_budget: 0,
        ..valid_spec()
    };
    assert_eq!(
        spec.validate().expect_err("must be refused"),
        TaskSpecError::ZeroAttemptBudget
    );
}

#[test]
fn a_spec_with_no_verification_profile_is_refused() {
    // §3.7: "**any acceptance criterion not bound to at least one check**" is
    // what `plan validate` refuses hardest, because an unbound criterion is "the
    // mechanism by which a task reaches `COMPLETE` on an agent's word". A task
    // with no verification profile at all is that mechanism in its purest form.
    let spec = TaskSpec {
        verification_profile: "  ".to_string(),
        ..valid_spec()
    };
    assert_eq!(
        spec.validate().expect_err("must be refused"),
        TaskSpecError::NoVerificationProfile
    );
}

#[test]
fn a_spec_with_no_objective_is_refused() {
    // The objective is the only thing in the file that tells an agent what to
    // do. A blank one produces an attempt that cannot succeed and a review
    // packet that cannot be judged.
    let spec = TaskSpec {
        objective: String::new(),
        ..valid_spec()
    };
    assert_eq!(
        spec.validate().expect_err("must be refused"),
        TaskSpecError::NoObjective
    );
}

#[test]
fn a_spec_with_an_unusable_id_is_refused() {
    let spec = TaskSpec {
        id: "  ".to_string(),
        ..valid_spec()
    };
    assert!(matches!(
        spec.validate().expect_err("must be refused"),
        TaskSpecError::Id(_)
    ));
}
