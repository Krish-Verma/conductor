//! The exit from `AWAITING_REVIEW` — the edge §5.2 drew and nothing could take
//! (ADR-0019).
//!
//! Before S13, `lease.rs` had a writer for every route *into* `AWAITING_REVIEW`
//! and none out of it: `route_reconciled` requires `RECONCILING`,
//! `route_verified` requires `VERIFYING`, `reopen_for_repair` and
//! `escalate_from_repairing` require `REPAIRING`, and `resume_after_grant`
//! requires `AWAITING_APPROVAL`. `advance_state` — the one function that could
//! have taken any other edge — is private. So a run that reached
//! `AWAITING_REVIEW` stayed there for the rest of time.
//!
//! These tests are about the *writer*. What a human is allowed to decide, and
//! what acceptance does not excuse, is `conductor-core`'s completion gate and is
//! tested there.

mod common;

use conductor_core::{ReviewOutcome, RunId, RunState};
use conductor_store::StoreError;

/// A run seeded straight into `AWAITING_REVIEW`, which is where every verdict
/// that needs a person leaves it.
fn awaiting_review(store: &mut conductor_store::Store, id: &str) -> RunId {
    common::seed_parents(store).expect("parents");
    common::seed_runs(
        store,
        &[common::SeedRun::ready(id, 0, 1_000).with_state("AWAITING_REVIEW")],
    )
    .expect("seed");
    RunId::new(id.to_string()).expect("run id")
}

#[test]
fn a_reviewed_run_can_be_sent_to_repair() {
    let (_dir, mut store) = common::temp_store();
    let run = awaiting_review(&mut store, "R-review-1");

    let state = store
        .apply_review_decision(
            &run,
            ReviewOutcome::Repairing,
            "a human asked for a repair",
            2_000,
        )
        .expect("AWAITING_REVIEW → REPAIRING is an edge §5.2 draws");

    assert_eq!(state, RunState::Repairing);
    assert_eq!(
        common::count(
            &store,
            "SELECT COUNT(*) FROM run WHERE id = 'R-review-1' AND state = 'REPAIRING'"
        ),
        1
    );
}

#[test]
fn a_reviewed_run_can_be_superseded_or_cancelled() {
    for (outcome, expected, id) in [
        (
            ReviewOutcome::Superseded,
            RunState::Superseded,
            "R-review-2",
        ),
        (ReviewOutcome::Cancelled, RunState::Cancelled, "R-review-3"),
    ] {
        let (_dir, mut store) = common::temp_store();
        let run = awaiting_review(&mut store, id);
        let state = store
            .apply_review_decision(&run, outcome, "a human decided", 2_000)
            .expect("an edge §5.2 draws");
        assert_eq!(state, expected);
    }
}

#[test]
fn a_run_that_is_not_awaiting_review_is_refused() {
    // The guard that matters. This writer is **unfenced** — a run in
    // `AWAITING_REVIEW` has released its lease and is waiting for a person, so
    // there is no lease-holder to fence against, exactly as `resume_after_grant`
    // argues. `WHERE state = 'AWAITING_REVIEW'` is therefore the *only* thing
    // standing between a review decision and a run that is busy running an agent.
    //
    // Remove that clause and this test fails while every other test in this file
    // passes — which is what makes it the one worth having.
    let (_dir, mut store) = common::temp_store();
    common::seed_parents(&mut store).expect("parents");
    common::seed_runs(
        &mut store,
        &[common::SeedRun::ready("R-running", 0, 1_000).with_state("RUNNING")],
    )
    .expect("seed");
    let run = RunId::new("R-running".to_string()).expect("run id");

    let refused = store
        .apply_review_decision(&run, ReviewOutcome::Cancelled, "a human decided", 2_000)
        .expect_err("a RUNNING run must not be moved by a review decision");

    match refused {
        StoreError::NotInState { required, .. } => {
            assert_eq!(required, RunState::AwaitingReview)
        }
        other => panic!("the refusal must name the state it required: {other:?}"),
    }
    assert_eq!(
        common::count(
            &store,
            "SELECT COUNT(*) FROM run WHERE id = 'R-running' AND state = 'RUNNING'"
        ),
        1,
        "the run must be untouched"
    );
}

#[test]
fn two_decisions_racing_the_same_review_produce_one_winner() {
    // Two operators answering the same review at the same moment. The second
    // finds the state already moved and is refused rather than overwriting the
    // first decision — the same property `resume_after_grant` relies on, and the
    // reason the guard is a `WHERE` clause rather than a prior `SELECT`.
    let (_dir, mut store) = common::temp_store();
    let run = awaiting_review(&mut store, "R-review-4");

    store
        .apply_review_decision(&run, ReviewOutcome::Repairing, "first", 2_000)
        .expect("the first decision wins");
    let second = store
        .apply_review_decision(&run, ReviewOutcome::Cancelled, "second", 2_001)
        .expect_err("the second must be refused, not applied");

    assert!(matches!(second, StoreError::NotInState { .. }));
    assert_eq!(
        common::count(
            &store,
            "SELECT COUNT(*) FROM run WHERE id = 'R-review-4' AND state = 'REPAIRING'"
        ),
        1,
        "the first decision must survive the second attempt"
    );
}

#[test]
fn the_transition_is_recorded_as_an_event() {
    // §3.5 keeps the event journal out of what survives a lost store, but within
    // one store's life "who moved this run and why" is the difference between an
    // audit trail and a state column. Every other state writer in `lease.rs`
    // appends one; this one is not allowed to be the exception.
    let (_dir, mut store) = common::temp_store();
    let run = awaiting_review(&mut store, "R-review-5");

    store
        .apply_review_decision(
            &run,
            ReviewOutcome::Cancelled,
            "a human stopped the task",
            2_000,
        )
        .expect("decision applied");

    assert_eq!(
        common::count(
            &store,
            "SELECT COUNT(*) FROM event WHERE run_id = 'R-review-5' \
             AND kind = 'RUN_STATE_CHANGED'"
        ),
        1
    );
    let payload: String = store
        .conn()
        .query_row(
            "SELECT payload FROM event WHERE run_id = 'R-review-5' \
             AND kind = 'RUN_STATE_CHANGED'",
            [],
            |row| row.get(0),
        )
        .expect("payload");
    assert!(
        payload.contains("a human stopped the task"),
        "the reason a human gave must reach the journal: {payload}"
    );
}
