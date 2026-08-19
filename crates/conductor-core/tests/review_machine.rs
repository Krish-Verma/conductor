//! §5.2's *Review (3 states)* machine, and the five decisions its `DECIDED` edge
//! carries.
//!
//! ```text
//! PENDING ─export─► EXPORTED ─import─► DECIDED
//!           {accept · repair · revise_plan · pause · stop}
//! ```
//!
//! # Why a review needs a state machine at all
//!
//! A review is the one Conductor object whose transitions are driven entirely by
//! a human, and that is exactly why they have to be checked rather than trusted.
//! §6.5 makes importing *"a **mutating** operation … never a file an agent could
//! write"*, so the import path is a place an attacker would like to reach twice:
//! once to have a decision applied, and again to have it applied differently.
//! `DECIDED` being terminal is what makes the second call a refusal instead of a
//! second answer.

use conductor_core::{ReviewDecision, ReviewState};

#[test]
fn the_three_states_are_exactly_section_5_2s() {
    assert_eq!(
        ReviewState::ALL,
        &[
            ReviewState::Pending,
            ReviewState::Exported,
            ReviewState::Decided
        ]
    );
    assert_eq!(ReviewState::Pending.as_str(), "PENDING");
    assert_eq!(ReviewState::Exported.as_str(), "EXPORTED");
    assert_eq!(ReviewState::Decided.as_str(), "DECIDED");
}

#[test]
fn decided_is_the_only_terminal_state() {
    assert!(ReviewState::Decided.is_terminal());
    assert!(!ReviewState::Pending.is_terminal());
    assert!(!ReviewState::Exported.is_terminal());
}

#[test]
fn the_only_edges_are_export_and_import() {
    assert_eq!(ReviewState::Pending.successors(), &[ReviewState::Exported]);
    assert_eq!(ReviewState::Exported.successors(), &[ReviewState::Decided]);
    assert_eq!(ReviewState::Decided.successors(), &[]);
}

#[test]
fn a_review_cannot_be_decided_without_being_exported_first() {
    // The decision is *about* a packet. Deciding before one exists would be a
    // human answering a question nobody asked, and the packet hash the decision
    // is bound to would have nothing to match.
    assert!(
        ReviewState::Pending
            .transition_to(ReviewState::Decided)
            .is_err()
    );
    assert!(
        ReviewState::Pending
            .transition_to(ReviewState::Exported)
            .is_ok()
    );
    assert!(
        ReviewState::Exported
            .transition_to(ReviewState::Decided)
            .is_ok()
    );
}

#[test]
fn a_decided_review_cannot_be_decided_again() {
    for to in ReviewState::ALL {
        assert!(
            ReviewState::Decided.transition_to(*to).is_err(),
            "DECIDED must be terminal, but accepted {to:?}"
        );
    }
}

#[test]
fn re_exporting_an_exported_review_is_refused_rather_than_silently_reissued() {
    // A self-transition is refused everywhere in this codebase for the reason
    // `TaskState` gives: writing the state a row is already in looks like
    // progress and is not. For a review it is stronger than cosmetic — a second
    // export would mint a second packet hash for one review, and the decision
    // could then be bound to whichever of the two the attacker preferred.
    assert!(
        ReviewState::Exported
            .transition_to(ReviewState::Exported)
            .is_err()
    );
    assert!(
        ReviewState::Pending
            .transition_to(ReviewState::Pending)
            .is_err()
    );
}

#[test]
fn the_five_decisions_are_exactly_section_6_5s() {
    assert_eq!(
        ReviewDecision::ALL,
        &[
            ReviewDecision::Accept,
            ReviewDecision::Repair,
            ReviewDecision::RevisePlan,
            ReviewDecision::Pause,
            ReviewDecision::Stop,
        ]
    );
    assert_eq!(ReviewDecision::Accept.as_str(), "accept");
    assert_eq!(ReviewDecision::Repair.as_str(), "repair");
    assert_eq!(ReviewDecision::RevisePlan.as_str(), "revise_plan");
    assert_eq!(ReviewDecision::Pause.as_str(), "pause");
    assert_eq!(ReviewDecision::Stop.as_str(), "stop");
}

#[test]
fn a_decision_conductor_does_not_know_is_refused_and_not_defaulted() {
    // §4.4's rule for unknown actions, applied to the one field a human types by
    // hand: an unrecognised decision must not become the most permissive known
    // one. `accept` is the decision that advances a task to `COMPLETE`, so a
    // typo silently reading as `accept` is the worst available failure.
    for bad in ["", "ACCEPT", "acccept", "approve", "merge", "yes", "revise"] {
        assert!(
            bad.parse::<ReviewDecision>().is_err(),
            "{bad:?} must not parse as a decision"
        );
    }
    // Positive control: the five real spellings do parse, so the refusal above
    // is not a parser that refuses everything.
    for (text, expected) in [
        ("accept", ReviewDecision::Accept),
        ("repair", ReviewDecision::Repair),
        ("revise_plan", ReviewDecision::RevisePlan),
        ("pause", ReviewDecision::Pause),
        ("stop", ReviewDecision::Stop),
    ] {
        assert_eq!(text.parse::<ReviewDecision>().expect(text), expected);
    }
}

#[test]
fn only_pause_leaves_the_task_where_it_is() {
    // §5.2 gives `AWAITING_REVIEW` exactly four successors — `Complete`,
    // `Repairing`, `Superseded`, `Cancelled` — which is four labels for five
    // decisions. `pause` is the one that is *not* a transition: the human looked
    // and is not deciding yet, and a self-transition is refused. Recording that
    // as "no target state" here is what stops a caller inventing one.
    use conductor_core::TaskState;
    assert_eq!(
        ReviewDecision::Accept.task_target(),
        Some(TaskState::Complete)
    );
    assert_eq!(
        ReviewDecision::Repair.task_target(),
        Some(TaskState::Repairing)
    );
    assert_eq!(
        ReviewDecision::RevisePlan.task_target(),
        Some(TaskState::Superseded)
    );
    assert_eq!(ReviewDecision::Pause.task_target(), None);
    assert_eq!(
        ReviewDecision::Stop.task_target(),
        Some(TaskState::Cancelled)
    );
}

#[test]
fn every_decisions_target_is_an_edge_section_5_2_actually_draws() {
    // The guard that keeps the table above honest: a target that §5.2 does not
    // draw from `AWAITING_REVIEW` would be caught here rather than at the first
    // real import.
    use conductor_core::TaskState;
    for decision in ReviewDecision::ALL {
        if let Some(target) = decision.task_target() {
            assert!(
                TaskState::AwaitingReview.transition_to(target).is_ok(),
                "{decision:?} targets {target:?}, which §5.2 does not draw from AWAITING_REVIEW"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ReviewOutcome — the proof discipline, applied to the review edge
// ---------------------------------------------------------------------------

#[test]
fn an_accepted_outcome_cannot_be_built_without_the_completion_token() {
    // The whole of ADR-0019 in one assertion. `ReconciledRoute::Complete` earns
    // its `RunState::Complete` by carrying a `VerifiedComplete` that only
    // `completion::evaluate` can mint; the review edge writes the same terminal
    // state, so it must be held to the same standard or `accept` becomes the one
    // door into `COMPLETE` that needs no evidence.
    //
    // `ReviewOutcome::Accepted` therefore carries the token too. The compile-fail
    // proof lives on the type's own doc comment, next to `ReconciledRoute`'s,
    // because that is where a reader looking for the back door will go.
    use conductor_core::completion::{
        AcceptanceEvidence, CheckEvidence, ChecksEvidence, CompletionEvidence, FindingsEvidence,
        PolicyEvidence, ReconciliationEvidence, Slice, evaluate,
    };
    use conductor_core::{ReviewOutcome, RunState, VerificationOutcome};

    let verified = evaluate(&CompletionEvidence {
        tree_hash: "tree-abc".to_string(),
        required: ChecksEvidence::new([CheckEvidence {
            check_id: "unit-tests".to_string(),
            outcome: VerificationOutcome::Pass,
            tree_hash: "tree-abc".to_string(),
        }]),
        conditional: ChecksEvidence::new([]),
        invariants: ChecksEvidence::new([]),
        findings: FindingsEvidence::unresolved(0),
        reconciliation: ReconciliationEvidence::AcceptedAtReview {
            verdict: "CONTRADICTED".to_string(),
            authorization: "AG-review-1".to_string(),
        },
        acceptance: AcceptanceEvidence::NoCriteria,
        policy: PolicyEvidence::NotEvaluated {
            owner: Slice::S7Policy,
        },
    })
    .expect("the gate certifies an accepted review whose checks passed");

    assert_eq!(
        ReviewOutcome::Accepted(verified).state(),
        RunState::Complete
    );
}

#[test]
fn the_three_decisions_that_need_no_proof_carry_none() {
    // Non-vacuity control for the test above: if *every* outcome needed a token,
    // the assertion there would be about ceremony rather than about `COMPLETE`.
    // These three write states that are refusals or abandonments, and nothing is
    // being certified, so they are bare variants — exactly as
    // `ReconciledRoute::Repairing` is.
    use conductor_core::{ReviewOutcome, RunState};
    assert_eq!(ReviewOutcome::Repairing.state(), RunState::Repairing);
    assert_eq!(ReviewOutcome::Superseded.state(), RunState::Superseded);
    assert_eq!(ReviewOutcome::Cancelled.state(), RunState::Cancelled);
}

#[test]
fn every_outcome_writes_a_state_section_5_2_draws_from_awaiting_review() {
    use conductor_core::{ReviewOutcome, TaskState};
    for outcome in [
        ReviewOutcome::Repairing,
        ReviewOutcome::Superseded,
        ReviewOutcome::Cancelled,
    ] {
        assert!(
            TaskState::AwaitingReview
                .transition_to(outcome.state().as_task_state())
                .is_ok(),
            "{outcome:?} writes a state §5.2 does not draw from AWAITING_REVIEW"
        );
    }
}
