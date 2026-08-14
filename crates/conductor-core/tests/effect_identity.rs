//! `operation_id = blake3(kind ‖ run_id ‖ attempt_ordinal ‖ tree_hash)` — §4.7.
//!
//! The identity is the whole idempotency mechanism: two workers that intend the
//! same effect must compute the same id, and two *different* effects must never
//! collide. Both halves are tested, because a hash function used as an identity
//! is only useful if the pre-image is unambiguous — concatenating fields without
//! a separator makes `("a","bc")` and `("ab","c")` the same operation.

use conductor_core::RunId;
use conductor_core::effect::{OperationId, Precondition, SideEffectKind, SideEffectState};

fn run(id: &str) -> RunId {
    RunId::new(id).expect("run id")
}

#[test]
fn the_same_effect_computes_the_same_id() {
    let a = OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-0041"), 1, "tree-abc");
    let b = OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-0041"), 1, "tree-abc");
    assert_eq!(a, b);
    assert!(a.as_str().starts_with("blake3:"));
}

#[test]
fn every_component_changes_the_id() {
    let base = OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-0041"), 1, "tree-abc");
    let others = [
        OperationId::compute(
            SideEffectKind::WorkspaceCreate,
            &run("r-0041"),
            1,
            "tree-abc",
        ),
        OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-0042"), 1, "tree-abc"),
        OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-0041"), 2, "tree-abc"),
        OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-0041"), 1, "tree-abd"),
    ];
    for other in &others {
        assert_ne!(&base, other, "a component was not part of the identity");
    }
}

#[test]
fn field_boundaries_are_unambiguous() {
    // Without a separator, ("r-0041", 1) and ("r-004", 11) would hash the same
    // bytes and two unrelated effects would share one ledger row.
    let a = OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-0041"), 1, "t");
    let b = OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-004"), 11, "t");
    assert_ne!(a, b);

    let c = OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-1"), 1, "ab");
    let d = OperationId::compute(SideEffectKind::ArtifactWrite, &run("r-1"), 1, "ab");
    assert_eq!(c, d);
}

#[test]
fn a_ledger_state_never_silently_becomes_confirmed() {
    // §4.7: an INTENDED row is resolved by re-checking the precondition against
    // the world, and an indeterminate answer is AMBIGUOUS — never a retry and
    // never a guess.
    assert_eq!(SideEffectState::Intended.as_str(), "INTENDED");
    assert_eq!(SideEffectState::Confirmed.as_str(), "CONFIRMED");
    assert_eq!(SideEffectState::Failed.as_str(), "FAILED");
    assert_eq!(SideEffectState::Ambiguous.as_str(), "AMBIGUOUS");
    assert_eq!(SideEffectState::ALL.len(), 4);

    // AMBIGUOUS halts the run; CONFIRMED and FAILED are decided.
    assert!(SideEffectState::Ambiguous.halts_the_run());
    assert!(!SideEffectState::Confirmed.halts_the_run());
    assert!(!SideEffectState::Failed.halts_the_run());
    assert!(!SideEffectState::Intended.halts_the_run());
}

#[test]
fn a_precondition_round_trips_so_a_restart_can_recheck_it() {
    let precondition = Precondition::FileWithHash {
        path: "/artifacts/r-0041/baseline.json".to_string(),
        content_hash: "blake3:deadbeef".to_string(),
    };
    let json = serde_json::to_string(&precondition).expect("serialize");
    let back: Precondition = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(precondition, back);
}

#[test]
fn every_side_effect_kind_names_a_checkable_precondition() {
    // §4.7's design constraint: "an effect Conductor cannot verify afterwards is
    // an effect Conductor may not own." A kind with no way to answer "did it
    // happen?" must not exist.
    for kind in SideEffectKind::ALL {
        assert!(
            !kind.did_it_happen_question().is_empty(),
            "{kind:?} has no post-hoc check"
        );
    }
}

// ---------------------------------------------------------------------------
// S5's two Conductor-owned git effects.
// ---------------------------------------------------------------------------

#[test]
fn the_two_git_effects_of_section_4_7_exist_and_ask_the_questions_it_states() {
    // §4.7's table, verbatim:
    //
    // | `git.commit.local`     | does a commit with this tree and message exist
    // |                        | on the run branch?
    // | `git.fetch_into_main`  | does the target ref point at the expected sha?
    //
    // S3 deliberately left both kinds out — "a kind whose intent/confirm path
    // does not exist is a lie the ledger would tell on restart". S5 supplies
    // both paths, so both kinds appear.
    assert_eq!(SideEffectKind::GitCommitLocal.as_str(), "git.commit.local");
    assert_eq!(
        SideEffectKind::GitFetchIntoMain.as_str(),
        "git.fetch_into_main"
    );
    assert!(
        SideEffectKind::GitCommitLocal
            .did_it_happen_question()
            .contains("run branch")
    );
    assert!(
        SideEffectKind::GitFetchIntoMain
            .did_it_happen_question()
            .contains("expected sha")
    );
}

#[test]
fn the_git_preconditions_round_trip_so_a_restart_can_recheck_them() {
    // The ledger row is the only thing a restarted Conductor has. If a
    // precondition cannot be read back, the effect is undecidable and the run
    // halts for a human — so the round trip is the load-bearing property, not a
    // serde formality.
    for precondition in [
        Precondition::CommitOnBranch {
            path: "/workspaces/r-0041".to_string(),
            branch: "conductor/T-0012/r-0041".to_string(),
            tree: "0f9c1a".to_string(),
            message_marker: "Conductor-Run: r-0041".to_string(),
        },
        Precondition::RefAtSha {
            path: "/repo".to_string(),
            reference: "refs/heads/conductor/T-0012/r-0041".to_string(),
            sha: "abc123".to_string(),
        },
    ] {
        let json = serde_json::to_string(&precondition).expect("serialize");
        let back: Precondition = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(precondition, back);
        assert!(!back.path().is_empty());
    }
}

#[test]
fn the_two_git_effects_have_distinct_operation_ids_for_the_same_run_and_tree() {
    // Both effects of one attempt share run, ordinal and tree. Only the `kind`
    // component separates them, so if it were dropped from the hash the fetch
    // would resolve the commit's ledger row — and a restart would conclude the
    // commit had already happened because the fetch had.
    let commit = OperationId::compute(SideEffectKind::GitCommitLocal, &run("r-0041"), 1, "tree-a");
    let fetch = OperationId::compute(
        SideEffectKind::GitFetchIntoMain,
        &run("r-0041"),
        1,
        "tree-a",
    );
    assert_ne!(commit, fetch);
}
