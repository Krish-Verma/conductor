//! Acceptance row 27 — "Stale worker wakes late … **all writes rejected** …
//! successor unaffected".
//!
//! §4.7: "`lease_epoch` **is the fencing token.** Every subsequent write by that
//! worker carries its epoch and is rejected if the epoch moved. Without fencing,
//! a process that stalls past its lease and then wakes will happily write over
//! its successor's work."
//!
//! The test is written so that it *can* fail: `tests/fencing_non_vacuity.rs`
//! performs the identical sequence through an unfenced statement and asserts the
//! successor's work is destroyed. If fencing were a no-op, this file would pass
//! and that one would fail — which is how a safety test is shown to be live
//! rather than lucky (ADR-0006).

mod common;

use common::SeedRun;
use conductor_core::attempt::Attempt;
use conductor_core::{AttemptId, Fence, RunState, TerminalAttempt};
use conductor_store::StoreError;

const LEASE_MS: i64 = 60_000;
const NOW: i64 = 1_770_000_000_000;

fn terminal_evidence(attempt_id: &str, run_id: &str) -> TerminalAttempt {
    Attempt::create(
        AttemptId::new(attempt_id).expect("attempt id"),
        conductor_core::RunId::new(run_id).expect("run id"),
        1,
    )
    .starting()
    .active(1, Some(1))
    .exited(0)
    .evidence()
}

#[test]
fn a_stale_worker_that_wakes_after_lease_expiry_cannot_write() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    // Worker A claims and then stalls past its lease.
    let a = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    let stale_fence = Fence::new(a.run_id.clone(), a.lease_epoch);

    // The lease tick sweeps the run: §5.2's restart rule forces it to
    // RECONCILING, and the epoch moves so that A is fenced out the moment its
    // lease lapses — not merely once a successor happens to claim.
    let woken = NOW + LEASE_MS + 1;
    let expired = store.expire_leases(woken).expect("expire");
    assert_eq!(expired.len(), 1);

    // Worker B takes the run.
    let b = store
        .claim_next_run("worker-b", woken, LEASE_MS)
        .expect("claim")
        .expect("the successor claims");
    let live_fence = Fence::new(b.run_id.clone(), b.lease_epoch);
    assert!(b.lease_epoch > a.lease_epoch);

    // Now A wakes up. Every write it can make must be rejected.
    let evidence = terminal_evidence("a-0001", "r-0001");

    let err = store
        .advance_to_reconciling(&stale_fence, &evidence, woken + 1)
        .expect_err("a stale epoch must not be able to move the run");
    assert!(
        matches!(err, StoreError::FencedOut { .. }),
        "expected FencedOut, got {err:?}"
    );

    let err = store
        .renew_lease(&stale_fence, woken + 1, LEASE_MS)
        .expect_err("a stale worker must not be able to extend a lease it lost");
    assert!(matches!(err, StoreError::FencedOut { .. }));

    let err = store
        .record_event(
            &stale_fence,
            conductor_core::EventKind::AttemptFinished,
            "{}",
            woken + 1,
        )
        .expect_err("a stale worker must not be able to append evidence");
    assert!(matches!(err, StoreError::FencedOut { .. }));

    // The successor is unaffected: its own write still lands.
    store
        .advance_to_reconciling(&live_fence, &evidence, woken + 2)
        .expect("the live owner writes normally");

    let state: String = store
        .conn()
        .query_row("SELECT state FROM run WHERE id='r-0001'", [], |row| {
            row.get(0)
        })
        .expect("state");
    assert_eq!(state, RunState::Reconciling.as_str());

    let owner: String = store
        .conn()
        .query_row("SELECT lease_owner FROM run WHERE id='r-0001'", [], |row| {
            row.get(0)
        })
        .expect("owner");
    assert_eq!(
        owner, "worker-b",
        "the stale worker never took ownership back"
    );
}

#[test]
fn the_epoch_moves_at_lease_expiry_even_before_a_successor_claims() {
    // The narrower hazard: between the lease lapsing and anybody re-claiming,
    // the stale worker's epoch is still the current one. If expiry did not move
    // the token, A could write into that window unopposed.
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    let a = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    let stale_fence = Fence::new(a.run_id.clone(), a.lease_epoch);

    let woken = NOW + LEASE_MS + 1;
    store.expire_leases(woken).expect("expire");

    let err = store
        .renew_lease(&stale_fence, woken, LEASE_MS)
        .expect_err("expiry alone must fence the old owner out");
    assert!(matches!(err, StoreError::FencedOut { .. }));
}

#[test]
fn a_fenced_out_write_changes_nothing_at_all() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    let a = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    let stale = Fence::new(a.run_id.clone(), a.lease_epoch);
    let woken = NOW + LEASE_MS + 1;
    store.expire_leases(woken).expect("expire");

    let events_before = common::count(&store, "SELECT COUNT(*) FROM event");
    let runs_before: String = store
        .conn()
        .query_row("SELECT state FROM run WHERE id='r-0001'", [], |r| r.get(0))
        .expect("state");

    let _ = store.record_event(
        &stale,
        conductor_core::EventKind::AttemptFinished,
        "{\"marker\":\"must-not-land\"}",
        woken,
    );
    let _ = store.renew_lease(&stale, woken, LEASE_MS);

    assert_eq!(
        common::count(&store, "SELECT COUNT(*) FROM event"),
        events_before,
        "a rejected write must not leave an event behind"
    );
    assert_eq!(
        common::count(
            &store,
            "SELECT COUNT(*) FROM event WHERE payload LIKE '%must-not-land%'"
        ),
        0
    );
    let runs_after: String = store
        .conn()
        .query_row("SELECT state FROM run WHERE id='r-0001'", [], |r| r.get(0))
        .expect("state");
    assert_eq!(runs_before, runs_after);
}

#[test]
fn a_live_lease_is_renewable_by_its_owner() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    let a = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    let fence = Fence::new(a.run_id.clone(), a.lease_epoch);

    store
        .renew_lease(&fence, NOW + 15_000, LEASE_MS)
        .expect("the owner renews");

    let expires: i64 = store
        .conn()
        .query_row(
            "SELECT lease_expires_at FROM run WHERE id='r-0001'",
            [],
            |r| r.get(0),
        )
        .expect("expiry");
    assert_eq!(expires, NOW + 15_000 + LEASE_MS);

    // Renewal does not move the epoch: it is the same ownership, extended.
    let epoch: i64 = store
        .conn()
        .query_row("SELECT lease_epoch FROM run WHERE id='r-0001'", [], |r| {
            r.get(0)
        })
        .expect("epoch");
    assert_eq!(epoch, a.lease_epoch);
}

#[test]
fn expiry_only_touches_runs_whose_lease_has_actually_lapsed() {
    let (_dir, mut store) = common::temp_store();
    common::seed_runs(
        &mut store,
        &[
            SeedRun::ready("r-live", 100, 1)
                .with_state("RUNNING")
                .leased("worker-live", NOW + 30_000),
            SeedRun::ready("r-dead", 100, 2)
                .with_state("RUNNING")
                .leased("worker-dead", NOW - 1),
            SeedRun::ready("r-ready", 100, 3),
        ],
    )
    .expect("seed");

    let expired = store.expire_leases(NOW).expect("expire");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].run_id.as_str(), "r-dead");

    let state_of = |id: &str| -> String {
        store
            .conn()
            .query_row("SELECT state FROM run WHERE id=?1", [id], |r| r.get(0))
            .expect("state")
    };
    assert_eq!(state_of("r-live"), "RUNNING", "a live lease is untouched");
    assert_eq!(
        state_of("r-dead"),
        "RECONCILING",
        "§5.2: an expired run is forced to RECONCILING"
    );
    assert_eq!(state_of("r-ready"), "READY", "an unleased run is untouched");
}

#[test]
fn expiry_sweeps_every_lease_bearing_state() {
    // §4.7 step 2: "Find runs in RUNNING/RECONCILING/VERIFYING with expired
    // leases." All three, not just RUNNING.
    let (_dir, mut store) = common::temp_store();
    common::seed_runs(
        &mut store,
        &[
            SeedRun::ready("r-running", 100, 1)
                .with_state("RUNNING")
                .leased("w", NOW - 1),
            SeedRun::ready("r-reconciling", 100, 2)
                .with_state("RECONCILING")
                .leased("w", NOW - 1),
            SeedRun::ready("r-verifying", 100, 3)
                .with_state("VERIFYING")
                .leased("w", NOW - 1),
        ],
    )
    .expect("seed");

    let expired = store.expire_leases(NOW).expect("expire");
    let mut ids: Vec<String> = expired
        .iter()
        .map(|e| e.run_id.as_str().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["r-reconciling", "r-running", "r-verifying"]);
    assert_eq!(
        common::count(
            &store,
            "SELECT COUNT(*) FROM run WHERE state='RECONCILING' AND lease_owner IS NULL"
        ),
        3
    );
}

#[test]
fn expiry_is_idempotent() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");
    store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");

    let later = NOW + LEASE_MS + 1;
    assert_eq!(store.expire_leases(later).expect("expire").len(), 1);
    assert_eq!(
        store.expire_leases(later).expect("expire again").len(),
        0,
        "a second sweep must not re-expire an already swept run"
    );
}
