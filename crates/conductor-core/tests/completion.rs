//! §4.5's seven completion criteria, and the gate that is the only way to name
//! `COMPLETE`.
//!
//! S3 left `ReconciledRoute` with no `Complete` variant at all, and said why:
//! "until S4 supplies verification, no code path, tested or untested, can route
//! a run to `COMPLETE`." S4 supplies it — so the variant appears, and it appears
//! carrying a token that only [`evaluate`] can mint.

use conductor_core::completion::{
    AcceptanceEvidence, CheckEvidence, ChecksEvidence, CompletionEvidence, Criterion,
    CriterionEvidence, FindingsEvidence, PolicyEvidence, ReconciliationEvidence, Slice, evaluate,
};
use conductor_core::{ReconciledRoute, RunState, VerificationOutcome};

const TREE: &str = "tree-abc";

fn passing(check_id: &str) -> CheckEvidence {
    CheckEvidence {
        check_id: check_id.to_string(),
        outcome: VerificationOutcome::Pass,
        tree_hash: TREE.to_string(),
    }
}

fn evidence() -> CompletionEvidence {
    CompletionEvidence {
        tree_hash: TREE.to_string(),
        required: ChecksEvidence::new([passing("typecheck"), passing("unit-tests")]),
        conditional: ChecksEvidence::new([]),
        invariants: ChecksEvidence::new([passing("no-secrets")]),
        findings: FindingsEvidence::unresolved(0),
        reconciliation: ReconciliationEvidence::Clean,
        acceptance: AcceptanceEvidence::NotEvaluated {
            owner: Slice::S11PlanLedger,
        },
        policy: PolicyEvidence::NotEvaluated {
            owner: Slice::S7Policy,
        },
    }
}

// ---------------------------------------------------------------------------
// the gate
// ---------------------------------------------------------------------------

#[test]
fn everything_satisfied_yields_a_token_that_names_complete() {
    let verified = evaluate(&evidence()).expect("all S4-owned criteria hold");
    assert_eq!(verified.tree_hash(), TREE);
    assert_eq!(
        ReconciledRoute::Complete(verified).state(),
        RunState::Complete
    );
}

#[test]
fn a_required_check_that_did_not_pass_refuses_completion() {
    for outcome in [
        VerificationOutcome::Fail,
        VerificationOutcome::Inconclusive,
        VerificationOutcome::Void,
    ] {
        let mut e = evidence();
        e.required = ChecksEvidence::new([CheckEvidence {
            check_id: "unit-tests".to_string(),
            outcome,
            tree_hash: TREE.to_string(),
        }]);
        let refusals = evaluate(&e).expect_err("{outcome:?} is not a pass");
        assert!(
            refusals
                .iter()
                .any(|r| r.criterion == Criterion::RequiredChecks),
            "{outcome:?} should refuse on criterion 1: {refusals:?}"
        );
    }
}

#[test]
fn a_check_that_passed_on_a_different_tree_does_not_count() {
    // §4.5 criterion 1 is "every required check `PASS` **at the current tree
    // hash**". A pass carried over from before the last edit is the exact thing
    // tree binding exists to refuse.
    let mut e = evidence();
    e.required = ChecksEvidence::new([CheckEvidence {
        check_id: "typecheck".to_string(),
        outcome: VerificationOutcome::Pass,
        tree_hash: "tree-from-two-edits-ago".to_string(),
    }]);
    let refusals = evaluate(&e).expect_err("a stale pass must not complete a task");
    assert!(
        refusals
            .iter()
            .any(|r| r.criterion == Criterion::RequiredChecks)
    );
    assert!(
        refusals[0].detail.contains("tree-from-two-edits-ago"),
        "the refusal must name the tree: {:?}",
        refusals[0]
    );
}

#[test]
fn a_triggered_conditional_check_that_failed_refuses_completion() {
    let mut e = evidence();
    e.conditional = ChecksEvidence::new([CheckEvidence {
        check_id: "migrate".to_string(),
        outcome: VerificationOutcome::Fail,
        tree_hash: TREE.to_string(),
    }]);
    let refusals = evaluate(&e).expect_err("criterion 2");
    assert!(
        refusals
            .iter()
            .any(|r| r.criterion == Criterion::ConditionalChecks)
    );
}

#[test]
fn a_failing_invariant_refuses_completion() {
    let mut e = evidence();
    e.invariants = ChecksEvidence::new([CheckEvidence {
        check_id: "no-secrets".to_string(),
        outcome: VerificationOutcome::Fail,
        tree_hash: TREE.to_string(),
    }]);
    let refusals = evaluate(&e).expect_err("criterion 3");
    assert!(
        refusals
            .iter()
            .any(|r| r.criterion == Criterion::InvariantChecks)
    );
}

#[test]
fn an_unresolved_finding_refuses_completion() {
    // §4.8: findings never auto-resolve. Criterion 4 is what gives that teeth.
    let mut e = evidence();
    e.findings = FindingsEvidence::unresolved(1);
    let refusals = evaluate(&e).expect_err("criterion 4");
    assert!(
        refusals
            .iter()
            .any(|r| r.criterion == Criterion::NoUnresolvedFindings)
    );
}

#[test]
fn a_reconciliation_verdict_outside_the_clean_pair_refuses_completion() {
    let mut e = evidence();
    e.reconciliation = ReconciliationEvidence::NotClean {
        verdict: "CONTRADICTED".to_string(),
    };
    let refusals = evaluate(&e).expect_err("criterion 6");
    assert!(
        refusals
            .iter()
            .any(|r| r.criterion == Criterion::ReconciliationVerdict)
    );
    assert!(refusals[0].detail.contains("CONTRADICTED"));
}

#[test]
fn every_failing_criterion_is_reported_not_just_the_first() {
    let mut e = evidence();
    e.required = ChecksEvidence::new([CheckEvidence {
        check_id: "t".to_string(),
        outcome: VerificationOutcome::Fail,
        tree_hash: TREE.to_string(),
    }]);
    e.findings = FindingsEvidence::unresolved(3);
    e.reconciliation = ReconciliationEvidence::NotClean {
        verdict: "OUT_OF_SCOPE".to_string(),
    };
    let refusals = evaluate(&e).expect_err("three criteria fail");
    assert_eq!(refusals.len(), 3, "{refusals:?}");
}

#[test]
fn a_run_with_no_checks_at_all_does_not_complete() {
    // Vacuous truth is the failure mode here: "every required check passed" is
    // trivially true of a profile that requires nothing, and a task that can
    // complete without a single check having run is exactly what §4.5's
    // "verification is authoritative" denies.
    let mut e = evidence();
    e.required = ChecksEvidence::new([]);
    e.invariants = ChecksEvidence::new([]);
    let refusals = evaluate(&e).expect_err("no evidence is not good evidence");
    assert!(
        refusals
            .iter()
            .any(|r| r.criterion == Criterion::RequiredChecks)
    );
}

// ---------------------------------------------------------------------------
// criterion 5 — every acceptance criterion binds to ≥1 *passing* check
// ---------------------------------------------------------------------------

/// One criterion, bound to the named checks, with the results the run observed.
fn criterion(id: &str, verified_by: &[&str], results: Vec<CheckEvidence>) -> CriterionEvidence {
    CriterionEvidence {
        id: id.to_string(),
        manual: false,
        verified_by: verified_by.iter().map(|s| s.to_string()).collect(),
        results,
    }
}

/// The whole evidence, with criterion 5 answered by `criteria`.
fn with_criteria(criteria: Vec<CriterionEvidence>) -> CompletionEvidence {
    CompletionEvidence {
        acceptance: AcceptanceEvidence::Evaluated { criteria },
        ..evidence()
    }
}

#[test]
fn a_criterion_bound_to_a_check_that_failed_refuses_completion() {
    // The load-bearing word in §4.5's criterion 5 is *passing*. A criterion
    // bound to a check that ran and failed is bound to nothing that proves it,
    // and a gate that read "is it bound?" rather than "did it pass?" would let
    // a task complete on the strength of a failing test being *mentioned*.
    let e = with_criteria(vec![criterion(
        "AC-1",
        &["unit-tests"],
        vec![CheckEvidence {
            check_id: "unit-tests".to_string(),
            outcome: VerificationOutcome::Fail,
            tree_hash: TREE.to_string(),
        }],
    )]);
    let refusals = evaluate(&e).expect_err("criterion 5");
    let refusal = refusals
        .iter()
        .find(|r| r.criterion == Criterion::AcceptanceBindings)
        .unwrap_or_else(|| panic!("{refusals:?}"));
    assert!(
        refusal.detail.contains("AC-1") && refusal.detail.contains("unit-tests"),
        "{refusal:?}"
    );
}

#[test]
fn a_manual_criterion_can_never_be_satisfied_mechanically() {
    // §3.7's escape hatch: "mark it `manual: true`, which forces a review
    // boundary." Bound here to a check that passed, because `manual` is not a
    // claim about how much evidence exists — it is a claim that mechanical
    // evidence is not what settles this question.
    let mut c = criterion("AC-2", &["unit-tests"], vec![passing("unit-tests")]);
    c.manual = true;
    let refusals = evaluate(&with_criteria(vec![c])).expect_err("criterion 5");
    let refusal = refusals
        .iter()
        .find(|r| r.criterion == Criterion::AcceptanceBindings)
        .unwrap_or_else(|| panic!("{refusals:?}"));
    assert!(refusal.detail.contains("AC-2"), "{refusal:?}");
}

#[test]
fn a_criterion_bound_to_a_passing_check_satisfies_criterion_five() {
    // **Positive control.** A criterion 5 that refused every task would satisfy
    // both tests above and complete nothing, so the satisfied case is asserted
    // in the same breath — including that the criterion leaves the deferred
    // list, which is what distinguishes "evaluated and held" from "skipped".
    let verified = evaluate(&with_criteria(vec![criterion(
        "AC-1",
        &["unit-tests"],
        vec![passing("unit-tests")],
    )]))
    .expect("a criterion bound to a passing check is satisfied");
    assert_eq!(verified.deferred(), &[Criterion::PolicyGrants]);
}

#[test]
fn one_passing_check_is_enough_even_when_a_sibling_check_failed() {
    // §4.5 says "≥1 passing check", not "all of them". The failing sibling is
    // still refused — by criterion 1, 2 or 3, wherever it belongs — so reading
    // criterion 5 as "all" would make it a duplicate of those rather than the
    // separate question it is.
    let e = with_criteria(vec![criterion(
        "AC-1",
        &["unit-tests", "smoke"],
        vec![
            CheckEvidence {
                check_id: "smoke".to_string(),
                outcome: VerificationOutcome::Fail,
                tree_hash: TREE.to_string(),
            },
            passing("unit-tests"),
        ],
    )]);
    let verified = evaluate(&e).expect("one bound check passed");
    assert_eq!(verified.deferred(), &[Criterion::PolicyGrants]);
}

#[test]
fn a_criterion_whose_only_pass_was_on_another_tree_does_not_count() {
    // The same rule criteria 1–3 obey — "PASS **at the current tree hash**" —
    // applied where it is easiest to forget, because criterion 5 goes looking
    // for a pass by name rather than iterating a group.
    let e = with_criteria(vec![criterion(
        "AC-1",
        &["unit-tests"],
        vec![CheckEvidence {
            check_id: "unit-tests".to_string(),
            outcome: VerificationOutcome::Pass,
            tree_hash: "tree-from-two-edits-ago".to_string(),
        }],
    )]);
    let refusals = evaluate(&e).expect_err("a stale pass proves nothing about this tree");
    assert!(
        refusals
            .iter()
            .any(|r| r.criterion == Criterion::AcceptanceBindings)
    );
}

#[test]
fn a_criterion_naming_a_check_that_never_ran_refuses_completion() {
    // A conditional check whose trigger did not match produces no result, and
    // "no result" is not a pass. Fail closed: the criterion names something
    // nobody measured.
    let e = with_criteria(vec![criterion("AC-1", &["migration-validate"], Vec::new())]);
    let refusals = evaluate(&e).expect_err("criterion 5");
    let refusal = refusals
        .iter()
        .find(|r| r.criterion == Criterion::AcceptanceBindings)
        .unwrap_or_else(|| panic!("{refusals:?}"));
    assert!(
        refusal.detail.contains("migration-validate"),
        "the refusal must name the check that produced nothing: {refusal:?}"
    );
}

#[test]
fn an_evaluation_that_names_no_criterion_refuses_rather_than_passing_vacuously() {
    // `Evaluated { criteria: [] }` is a caller saying "I evaluated the criteria"
    // and naming none, which asserts nothing at all. A plan that declares none
    // says so with `NoCriteria`; a task no plan was ever materialized for says
    // so with `NotEvaluated`. Leaving a third, vacuously-true spelling would
    // make the empty vector the easiest way to switch criterion 5 off.
    let refusals = evaluate(&with_criteria(Vec::new())).expect_err("criterion 5");
    assert!(
        refusals
            .iter()
            .any(|r| r.criterion == Criterion::AcceptanceBindings)
    );
}

#[test]
fn a_plan_that_declares_no_criteria_is_a_different_fact_from_no_plan_at_all() {
    // Schema v8's `NULL` versus `'[]'`, at the gate. Both let the task through
    // — a criterion-less task is still held to criteria 1–4 and 6–7, which is
    // what stops this being a hole — but only one of them has been *answered*.
    // Conflating them would let a pre-S11 row, which nobody has ever checked,
    // report itself as checked and found empty.
    let declared_none = CompletionEvidence {
        acceptance: AcceptanceEvidence::NoCriteria,
        ..evidence()
    };
    assert_eq!(
        evaluate(&declared_none)
            .expect("nothing to require")
            .deferred(),
        &[Criterion::PolicyGrants],
        "a plan that declares no criteria has answered criterion 5"
    );

    let never_materialized = evidence();
    assert_eq!(
        evaluate(&never_materialized).expect("gate").deferred(),
        &[Criterion::AcceptanceBindings, Criterion::PolicyGrants],
        "a task no plan was materialized for has not answered it"
    );
}

// ---------------------------------------------------------------------------
// the deferred criteria, and the trap that stops a later slice forgetting them
// ---------------------------------------------------------------------------

#[test]
fn the_deferred_criteria_are_exactly_the_two_s4_does_not_own() {
    // Pinned deliberately. When S7 or S11 lands and this list does not shrink,
    // this test is the second alarm — the first being that `evaluate` will not
    // compile once the owning slice adds a real variant to its evidence enum.
    let verified = evaluate(&evidence()).expect("gate");
    assert_eq!(
        verified.deferred(),
        &[Criterion::AcceptanceBindings, Criterion::PolicyGrants]
    );
}

#[test]
fn each_deferred_criterion_names_the_slice_that_owes_it() {
    assert_eq!(Criterion::AcceptanceBindings.owner(), Slice::S11PlanLedger);
    assert_eq!(Criterion::PolicyGrants.owner(), Slice::S7Policy);
    assert_eq!(Criterion::RequiredChecks.owner(), Slice::S4Verification);
}

#[test]
fn all_seven_criteria_from_4_5_are_represented() {
    assert_eq!(Criterion::ALL.len(), 7);
    let owners: Vec<Slice> = Criterion::ALL.iter().map(|c| c.owner()).collect();
    assert_eq!(
        owners
            .iter()
            .filter(|o| **o == Slice::S4Verification)
            .count(),
        5,
        "S4 owns criteria 1, 2, 3, 4 and 6"
    );
}

#[test]
fn a_deferred_criterion_cannot_be_asserted_as_satisfied_today() {
    // The mechanism, stated as a test so it is not merely a comment: the two
    // deferred evidence types have exactly one variant each, which says "not
    // evaluated". There is no way to write "policy is satisfied" until S7 adds
    // the variant — and adding it makes `evaluate`'s exhaustive match fail to
    // compile, so S7 cannot ship without deciding what to do here.
    let e = evidence();
    assert!(matches!(e.policy, PolicyEvidence::NotEvaluated { .. }));
    assert!(matches!(
        e.acceptance,
        AcceptanceEvidence::NotEvaluated { .. }
    ));
}

// ---------------------------------------------------------------------------
// the route
// ---------------------------------------------------------------------------

#[test]
fn no_other_route_out_of_reconciliation_is_terminal() {
    for route in [
        ReconciledRoute::Verifying,
        ReconciledRoute::AwaitingApproval,
        ReconciledRoute::AwaitingReview,
        ReconciledRoute::Blocked,
        ReconciledRoute::Repairing,
    ] {
        assert!(!route.state().is_terminal(), "{route:?}");
    }
}

#[test]
fn complete_is_the_only_terminal_route_and_it_carries_its_evidence() {
    let verified = evaluate(&evidence()).expect("gate");
    let route = ReconciledRoute::Complete(verified);
    assert!(route.state().is_terminal());
    assert!(!route.requires_human());
    match &route {
        ReconciledRoute::Complete(v) => assert_eq!(v.tree_hash(), TREE),
        other => panic!("{other:?}"),
    }
}
