//! §5.2's task machine.
//!
//! The property that matters most here is the one §5.2 states as an **invalid**
//! transition: `RUNNING → COMPLETE`. S3 made that unrepresentable for a *run* by
//! typing the exit from `RUNNING` (`leave_running` takes a `TerminalAttempt` and
//! returns one destination) and S4 kept it by making `ReconciledRoute::Complete`
//! carry a token only the completion gate can mint. Neither of those covers the
//! `task` row, which is a second place the same lie can be written — so the
//! legality table below is the task's half of the same guarantee.
//!
//! # The spec tests that used to be at the bottom of this file (S12)
//!
//! This file also exercised `TaskSpec`, S5's minimal `.conductor/task.yaml`, and
//! its five refusals. That type was the stopgap Part 8's S5 scope called a
//! "minimal task-spec file (not yet the plan ledger)", and S12 deleted it when
//! `conductor task run` finally moved onto the plan ledger — until then, two
//! files claimed to define what a task is and the command read the wrong one.
//!
//! The refusals moved unevenly, and a reader looking for the spec's coverage
//! deserves the exact map rather than a reassurance:
//!
//! * **A blank task id** and **an acceptance criterion bound to no check** are
//!   §3.7's, asserted in `crates/conductor-run/tests/plan_validate.rs` by
//!   `a_blank_task_id_is_refused_because_it_addresses_nothing` and
//!   `an_acceptance_criterion_bound_to_nothing_is_a_hard_error_and_not_a_warning`.
//! * **A verification profile that is not there** moved to the *verb*, not the
//!   validator: §4.5's clarification 3 settles the field as a path relative to
//!   the repository root, and only `task run` holds that root. It is asserted in
//!   `crates/conductor-cli/tests/task.rs` by
//!   `a_task_naming_a_verification_profile_that_is_not_there_is_refused_before_anything_runs`.
//! * **A blank objective**, **an empty scope** and **a zero attempt budget** have
//!   no successor. `plan::validate` refuses none of the three, and
//!   `plan::model::Task` gives all three a `serde` default rather than requiring
//!   them, so a plan can declare a task with no objective and validate. §3.7's
//!   nearest rule — "scope globs matching no path" — is a *different* rule and is
//!   deliberately deferred (see `plan`'s module docs: it needs a working tree).
//!   This is a gap the deletion *exposed* rather than created: the spec's version
//!   of it stopped being reachable the moment `task run` stopped reading the spec.
//!   It is recorded here rather than closed in a slice that does not own §3.7.

use conductor_core::TaskState;
use conductor_core::task::TransitionError;

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

// The task-spec tests that stood here were deleted at S12 along with the type
// they exercised. See this file's module docs for where their subject went.
