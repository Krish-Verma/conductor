//! The side-effect ledger — §4.7, acceptance row 22.
//!
//! > | 22 | Duplicate side effect | kill between effect and confirm |
//! > `side_effect` `INTENDED` | re-check precondition; do not re-run | no |
//! > only if ambiguous | **exactly one commit** |
//!
//! The store's job here is narrow: make the intent durable before the effect,
//! make the receipt durable after it, and make an `INTENDED` row findable on
//! restart. Deciding whether the effect happened is the caller's, because only
//! the caller can look at the world.

mod common;

use conductor_core::Fence;
use conductor_core::effect::{OperationId, Precondition, SideEffectKind, SideEffectState};
use conductor_store::StoreError;

const LEASE_MS: i64 = 60_000;
const NOW: i64 = 1_770_000_000_000;

fn claimed(store: &mut conductor_store::Store) -> Fence {
    common::seed_ready_runs(store, 1).expect("seed");
    let run = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    Fence::new(run.run_id, run.lease_epoch)
}

fn precondition() -> Precondition {
    Precondition::FileWithHash {
        path: "/artifacts/r-0001/baseline.json".to_string(),
        content_hash: "blake3:abc".to_string(),
    }
}

fn operation(fence: &Fence) -> OperationId {
    OperationId::compute(SideEffectKind::ArtifactWrite, fence.run_id(), 1, "tree-1")
}

#[test]
fn an_intent_is_durable_before_the_effect_runs() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let op = operation(&fence);

    store
        .intend_effect(
            &fence,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW,
        )
        .expect("intend");

    let row = store.side_effect(&op).expect("query").expect("a row");
    assert_eq!(row.state, SideEffectState::Intended);
    assert_eq!(row.kind, SideEffectKind::ArtifactWrite);
    assert_eq!(row.precondition, precondition());
    assert_eq!(row.receipt, None);
    assert_eq!(row.resolved_at, None);
}

#[test]
fn confirming_records_the_receipt() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let op = operation(&fence);
    store
        .intend_effect(
            &fence,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW,
        )
        .expect("intend");

    store
        .confirm_effect(&fence, &op, "{\"bytes\":128}", NOW + 5)
        .expect("confirm");

    let row = store.side_effect(&op).expect("query").expect("a row");
    assert_eq!(row.state, SideEffectState::Confirmed);
    assert_eq!(row.receipt.as_deref(), Some("{\"bytes\":128}"));
    assert_eq!(row.resolved_at, Some(NOW + 5));
}

#[test]
fn intending_the_same_operation_twice_is_a_no_op_not_a_second_row() {
    // The identity exists so that a restart which re-derives the same operation
    // finds the existing row instead of creating a parallel one.
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let op = operation(&fence);

    let first = store
        .intend_effect(
            &fence,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW,
        )
        .expect("intend");
    let second = store
        .intend_effect(
            &fence,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW + 1,
        )
        .expect("intend again");

    assert_eq!(first, SideEffectState::Intended);
    assert_eq!(
        second,
        SideEffectState::Intended,
        "the pre-existing row is reported, not replaced"
    );
    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM side_effect"), 1);
    let intended_at: i64 = store
        .conn()
        .query_row("SELECT intended_at FROM side_effect", [], |r| r.get(0))
        .expect("intended_at");
    assert_eq!(intended_at, NOW, "the original intent is not overwritten");
}

#[test]
fn re_intending_a_confirmed_effect_reports_it_as_already_done() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let op = operation(&fence);
    store
        .intend_effect(
            &fence,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW,
        )
        .expect("intend");
    store
        .confirm_effect(&fence, &op, "receipt", NOW + 1)
        .expect("confirm");

    let again = store
        .intend_effect(
            &fence,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW + 2,
        )
        .expect("intend again");
    assert_eq!(
        again,
        SideEffectState::Confirmed,
        "the caller must be told the effect already happened, not allowed to redo it"
    );
    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM side_effect"), 1);
}

#[test]
fn an_indeterminate_precondition_becomes_ambiguous_and_halts() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let op = operation(&fence);
    store
        .intend_effect(
            &fence,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW,
        )
        .expect("intend");

    store
        .mark_effect_ambiguous(&fence, &op, "the file exists but its hash differs", NOW + 9)
        .expect("ambiguous");

    let row = store.side_effect(&op).expect("query").expect("a row");
    assert_eq!(row.state, SideEffectState::Ambiguous);
    assert!(row.state.halts_the_run());
    assert!(
        row.receipt
            .as_deref()
            .unwrap_or_default()
            .contains("hash differs"),
        "the reason a human is being asked must be recorded"
    );
}

#[test]
fn unresolved_intents_are_what_a_restart_finds() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);

    let a = OperationId::compute(SideEffectKind::ArtifactWrite, fence.run_id(), 1, "tree-1");
    let b = OperationId::compute(SideEffectKind::WorkspaceCreate, fence.run_id(), 1, "tree-1");
    store
        .intend_effect(
            &fence,
            &a,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW,
        )
        .expect("intend a");
    store
        .intend_effect(
            &fence,
            &b,
            SideEffectKind::WorkspaceCreate,
            &Precondition::WorkspaceAtHead {
                path: "/ws/r-0001".to_string(),
                head: "abc123".to_string(),
            },
            NOW,
        )
        .expect("intend b");
    store
        .confirm_effect(&fence, &a, "done", NOW + 1)
        .expect("confirm a");

    let unresolved = store.unresolved_effects().expect("query");
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].operation_id, b);
    assert_eq!(unresolved[0].kind, SideEffectKind::WorkspaceCreate);
}

#[test]
fn every_ledger_write_is_fenced() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let stale = Fence::new(fence.run_id().clone(), fence.lease_epoch() - 1);
    let op = operation(&fence);

    let err = store
        .intend_effect(
            &stale,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW,
        )
        .expect_err("a stale worker must not record an intent");
    assert!(matches!(err, StoreError::FencedOut { .. }));
    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM side_effect"), 0);

    store
        .intend_effect(
            &fence,
            &op,
            SideEffectKind::ArtifactWrite,
            &precondition(),
            NOW,
        )
        .expect("intend");
    let err = store
        .confirm_effect(&stale, &op, "receipt", NOW)
        .expect_err("a stale worker must not confirm an effect");
    assert!(matches!(err, StoreError::FencedOut { .. }));
    let row = store.side_effect(&op).expect("query").expect("a row");
    assert_eq!(row.state, SideEffectState::Intended);
}
