//! §4.7's intent → precondition → act → receipt cycle, for the two git effects.
//!
//! The property under test is the one row 22 states: an effect happens **at most
//! once**, and "did it happen?" is answered by asking the world, never by
//! retrying and never by guessing.
//!
//! The crash injection lives in `integration_kill_matrix.rs`, which kills a real
//! process between the writes. This file pins the pieces that matrix depends on
//! — because a matrix over a mechanism whose semantics are wrong is a matrix
//! that proves the wrong thing precisely.

mod common;

use conductor_core::effect::{OperationId, Precondition, SideEffectKind};
use conductor_core::{Fence, RunId, SideEffectState};
use conductor_git::Trailers;
use conductor_run::effects::{
    Integration, IntegrationConfig, PreconditionAnswer, check_precondition, integrate,
};
use conductor_store::Store;

use common::vertical::{RUN, RUN_BRANCH, TARGET_BRANCH, World, commits_above, ref_updates};

/// Clone the run workspace and claim the run, returning the fence.
fn prepare(world: &World) -> (Store, Fence) {
    let mut store = world.store();
    let claimed = store
        .claim_next_run("worker-1", 0, conductor_store::LEASE_MS)
        .expect("claim")
        .expect("something to claim");
    let fence = claimed.fence();

    conductor_git::create_workspace(&conductor_git::WorkspaceRequest {
        source: world.source.clone(),
        workspace: world.workspace(),
        run_id: RunId::new(RUN).expect("id"),
        task_id: conductor_core::TaskId::new("T-0012").expect("id"),
        base_commit: world.base_commit.clone(),
        policy_hash: conductor_core::PolicyHash::new(common::agent::POLICY_HASH).expect("hash"),
    })
    .expect("workspace");

    (store, fence)
}

fn config(world: &World, tree_hash: &str) -> IntegrationConfig {
    IntegrationConfig {
        source_repo: world.source.clone(),
        workspace: world.workspace(),
        run_branch: RUN_BRANCH.to_string(),
        target_branch: TARGET_BRANCH.to_string(),
        base_commit: world.base_commit.clone(),
        attempt_ordinal: 1,
        tree_hash: tree_hash.to_string(),
        subject: "conductor: T-0012".to_string(),
        trailers: Trailers::new([conductor_git::Trailer::new("Conductor-Run", RUN)]),
    }
}

fn agent_edits(world: &World) {
    std::fs::write(
        world.workspace().join("src/added.rs"),
        "pub fn added() -> u32 { 1 }\n",
    )
    .expect("write");
}

// ---------------------------------------------------------------------------
// The three-valued precondition check.
// ---------------------------------------------------------------------------

#[test]
fn a_commit_that_was_never_made_is_a_decisive_no_not_an_ambiguity() {
    // The distinction is the whole of row 22's "no human unless ambiguous". If a
    // missing commit read as indeterminate, every crash between intent and
    // effect would stop for a person — which is the opposite of §4.7's
    // "converges with no human input".
    let world = World::new();
    let (_store, _fence) = prepare(&world);

    let answer = check_precondition(&Precondition::CommitOnBranch {
        path: world.workspace().display().to_string(),
        branch: RUN_BRANCH.to_string(),
        tree: "0000000000000000000000000000000000000000".to_string(),
        message_marker: "Conductor-Run: r-0041".to_string(),
    });
    assert_eq!(answer, PreconditionAnswer::NotHeld);
}

#[test]
fn a_commit_that_was_made_is_found_by_its_tree_and_marker() {
    let world = World::new();
    let (mut store, fence) = prepare(&world);
    agent_edits(&world);

    let outcome = integrate(&mut store, &fence, &config(&world, "tree-1"), &mut ()).expect("run");
    let Integration::Integrated { commit, .. } = outcome else {
        panic!("expected integration, got {outcome:?}");
    };

    assert_eq!(
        check_precondition(&Precondition::CommitOnBranch {
            path: world.workspace().display().to_string(),
            branch: RUN_BRANCH.to_string(),
            tree: commit.tree.clone(),
            message_marker: format!("Conductor-Run: {RUN}"),
        }),
        PreconditionAnswer::Held
    );
}

#[test]
fn a_repository_that_cannot_be_read_is_indeterminate_never_a_no() {
    // §4.7: "Ambiguous (precondition indeterminate) → mark `AMBIGUOUS`, halt the
    // run, raise a finding, require a human. **Never guess.**" A workspace that
    // is gone cannot say whether the commit happened, and answering "no" would
    // send the effect round again against a repository nobody can see.
    let world = World::new();
    let answer = check_precondition(&Precondition::CommitOnBranch {
        path: world.root().join("no-such-workspace").display().to_string(),
        branch: RUN_BRANCH.to_string(),
        tree: "abc".to_string(),
        message_marker: "Conductor-Run: r-0041".to_string(),
    });
    assert!(
        matches!(answer, PreconditionAnswer::Indeterminate(_)),
        "expected indeterminate, got {answer:?}"
    );
}

#[test]
fn a_ref_at_the_expected_sha_holds_and_a_missing_ref_does_not() {
    let world = World::new();
    let (mut store, fence) = prepare(&world);
    agent_edits(&world);

    let reference = format!("refs/heads/{RUN_BRANCH}");
    assert_eq!(
        check_precondition(&Precondition::RefAtSha {
            path: world.source.display().to_string(),
            reference: reference.clone(),
            sha: "abc".to_string(),
        }),
        PreconditionAnswer::NotHeld,
        "a ref that does not exist has decisively not been fetched"
    );

    let outcome = integrate(&mut store, &fence, &config(&world, "tree-1"), &mut ()).expect("run");
    let Integration::Integrated { fetched, .. } = &outcome else {
        panic!("expected integration, got {outcome:?}");
    };
    assert_eq!(
        check_precondition(&Precondition::RefAtSha {
            path: world.source.display().to_string(),
            reference,
            sha: fetched.sha.clone(),
        }),
        PreconditionAnswer::Held
    );
}

#[test]
fn a_ref_someone_else_moved_is_indeterminate() {
    // The ref exists but points somewhere Conductor did not put it. That is not
    // "the fetch has not happened" — it is a fact Conductor cannot account for,
    // and §4.7 says stop.
    let world = World::new();
    let (mut store, fence) = prepare(&world);
    agent_edits(&world);
    integrate(&mut store, &fence, &config(&world, "tree-1"), &mut ()).expect("run");

    let reference = format!("refs/heads/{RUN_BRANCH}");
    let answer = check_precondition(&Precondition::RefAtSha {
        path: world.source.display().to_string(),
        reference,
        sha: "0000000000000000000000000000000000000000".to_string(),
    });
    assert!(
        matches!(answer, PreconditionAnswer::Indeterminate(_)),
        "expected indeterminate, got {answer:?}"
    );
}

// ---------------------------------------------------------------------------
// The ledger.
// ---------------------------------------------------------------------------

#[test]
fn integration_records_both_effects_as_confirmed() {
    let world = World::new();
    let (mut store, fence) = prepare(&world);
    agent_edits(&world);

    integrate(&mut store, &fence, &config(&world, "tree-1"), &mut ()).expect("run");

    for kind in [
        SideEffectKind::GitCommitLocal,
        SideEffectKind::GitFetchIntoMain,
    ] {
        let id = OperationId::compute(kind, &RunId::new(RUN).expect("id"), 1, "tree-1");
        let row = store
            .side_effect(&id)
            .expect("read")
            .unwrap_or_else(|| panic!("{kind} has no ledger row"));
        assert_eq!(row.state, SideEffectState::Confirmed, "{kind}");
        assert!(row.receipt.is_some(), "{kind} has no receipt");
    }
    assert!(store.unresolved_effects().expect("effects").is_empty());
}

#[test]
fn integrating_twice_produces_one_commit_and_one_ref_update() {
    // Row 22's assertion, without a crash: the second call finds both ledger
    // rows `CONFIRMED` and must do nothing at all. If it instead re-ran the
    // effects, there would be two commits and two ref updates — which is exactly
    // what the counting below would show.
    let world = World::new();
    let (mut store, fence) = prepare(&world);
    agent_edits(&world);

    let first = integrate(&mut store, &fence, &config(&world, "tree-1"), &mut ()).expect("first");
    let second = integrate(&mut store, &fence, &config(&world, "tree-1"), &mut ()).expect("second");

    let (Integration::Integrated { commit: a, .. }, Integration::Integrated { commit: b, .. }) =
        (&first, &second)
    else {
        panic!("expected two integrations: {first:?} / {second:?}");
    };
    assert_eq!(a.sha, b.sha, "the second call must report the same commit");

    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        1,
        "exactly one commit"
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        1,
        "exactly one ref update"
    );
}

#[test]
fn an_attempt_that_changed_nothing_performs_no_effects_at_all() {
    // §4.8's `NO_CHANGE`. An empty Conductor-owned commit would move the run
    // branch while recording nothing, and would then satisfy a precondition
    // about a commit that meant nothing.
    let world = World::new();
    let (mut store, fence) = prepare(&world);

    let outcome = integrate(&mut store, &fence, &config(&world, "tree-1"), &mut ()).expect("run");
    assert!(
        matches!(outcome, Integration::NothingToCommit),
        "got {outcome:?}"
    );
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        0
    );
    // And no ledger row was opened for an effect that never happened.
    assert!(
        store
            .side_effect(&OperationId::compute(
                SideEffectKind::GitCommitLocal,
                &RunId::new(RUN).expect("id"),
                1,
                "tree-1",
            ))
            .expect("read")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Acceptance row 16.
// ---------------------------------------------------------------------------

#[test]
fn a_moved_target_branch_stops_integration_before_any_effect() {
    // Row 16: "no rebase, no merge". And nothing else either — the divergence is
    // detected before the first intent is written, so a human deciding what to
    // do is looking at a repository Conductor has not touched.
    let world = World::new();
    let (mut store, fence) = prepare(&world);
    agent_edits(&world);
    let moved = world.user_commits_to_main();

    let outcome = integrate(&mut store, &fence, &config(&world, "tree-1"), &mut ()).expect("run");
    let Integration::TargetMoved(divergence) = &outcome else {
        panic!("expected a divergence, got {outcome:?}");
    };
    assert_eq!(divergence.target_branch, TARGET_BRANCH);
    assert_eq!(divergence.expected, world.base_commit);
    assert_eq!(divergence.actual, moved);

    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0,
        "nothing may be committed once the target has moved"
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        0,
        "nothing may be fetched once the target has moved"
    );
    assert!(
        store.unresolved_effects().expect("effects").is_empty(),
        "no intent may be recorded for an effect that will not be attempted"
    );
    // The user's own branch is exactly where they left it.
    assert_eq!(common::agent::head(&world.source), moved);
}
