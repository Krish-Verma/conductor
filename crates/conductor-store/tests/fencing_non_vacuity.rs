//! Proof that `tests/fencing.rs` can fail.
//!
//! Two slices running, a safety test passed for the wrong reason (S1's crash
//! test held nothing at risk; S2's isolation test could not have detected the
//! damage it claimed to rule out — ADR-0006). A fencing test that passes because
//! nothing ever tried to write concurrently is the same failure.
//!
//! This file runs the **identical** sequence — claim, expire, successor claims,
//! stale worker wakes and writes — with the fencing predicate removed from the
//! statement, and asserts that the stale worker **destroys the successor's
//! work**. If fencing were a no-op, this file's assertions would be exactly what
//! `fencing.rs` observes.

mod common;

use conductor_core::Fence;
use conductor_store::{StoreError, with_immediate};
use rusqlite::params;

const LEASE_MS: i64 = 60_000;
const NOW: i64 = 1_770_000_000_000;

/// The same write as [`conductor_store::Store::renew_lease`] with the one clause
/// that makes it safe deleted. Not exported by the crate — it exists here only
/// so the safe path has something to be measured against.
fn unfenced_renew(store: &mut conductor_store::Store, fence: &Fence, now_ms: i64) -> usize {
    with_immediate(store.conn_mut(), |tx| {
        let n = tx.execute(
            "UPDATE run SET lease_owner='worker-a', lease_expires_at=?2 WHERE id=?1",
            params![fence.run_id().as_str(), now_ms + LEASE_MS],
        )?;
        Ok(n)
    })
    .expect("unfenced update")
}

#[test]
fn without_fencing_the_stale_worker_overwrites_its_successor() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    let a = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    let stale = Fence::new(a.run_id.clone(), a.lease_epoch);

    let woken = NOW + LEASE_MS + 1;
    store.expire_leases(woken).expect("expire");
    let b = store
        .claim_next_run("worker-b", woken, LEASE_MS)
        .expect("claim")
        .expect("successor claims");
    assert_eq!(b.lease_owner, "worker-b");

    // The fenced path refuses…
    let err = store
        .renew_lease(&stale, woken + 1, LEASE_MS)
        .expect_err("the real path must reject this");
    assert!(matches!(err, StoreError::FencedOut { .. }));

    // …and the unfenced path does exactly the damage the fence prevents.
    let rows = unfenced_renew(&mut store, &stale, woken + 1);
    assert_eq!(rows, 1, "the unfenced statement wrote");

    let owner: String = store
        .conn()
        .query_row("SELECT lease_owner FROM run WHERE id='r-0001'", [], |r| {
            r.get(0)
        })
        .expect("owner");
    assert_eq!(
        owner, "worker-a",
        "without fencing the stale worker has taken the run back from its successor — \
         this is the outcome tests/fencing.rs asserts cannot happen, so that test is live"
    );
}

#[test]
fn the_fence_rejects_precisely_because_the_epoch_moved() {
    // Same call, same worker, same run — the only difference is whether the
    // epoch in hand is current. Anything else passing or failing here would mean
    // the rejection came from somewhere other than the fencing token.
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    let a = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");

    let current = Fence::new(a.run_id.clone(), a.lease_epoch);
    store
        .renew_lease(&current, NOW + 1, LEASE_MS)
        .expect("the current epoch is accepted");

    let ahead = Fence::new(a.run_id.clone(), a.lease_epoch + 1);
    let err = store
        .renew_lease(&ahead, NOW + 2, LEASE_MS)
        .expect_err("an epoch that does not match is rejected");
    match err {
        StoreError::FencedOut { expected, actual } => {
            assert_eq!(expected, a.lease_epoch + 1);
            assert_eq!(actual, Some(a.lease_epoch));
        }
        other => panic!("expected FencedOut, got {other:?}"),
    }
}
