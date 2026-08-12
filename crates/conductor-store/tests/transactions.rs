//! `BEGIN IMMEDIATE` discipline: commit on success, roll back on error, leave
//! nothing behind either way, and take the write lock at BEGIN rather than at
//! first write.

mod common;

use std::time::{Duration, Instant};

use conductor_store::{Store, StoreError, with_immediate};
use rusqlite::TransactionBehavior;

#[test]
fn commit_path_persists_every_write() {
    let (_dir, mut store) = common::temp_store();
    common::seed_parents(&mut store).expect("seed");

    let out = with_immediate(store.conn_mut(), |tx| {
        for i in 0..3 {
            tx.execute(
                "INSERT INTO event (run_id, seq, kind, payload, at)
                 VALUES (NULL, ?1, 'RUN_CLAIMED', '{}', 0)",
                [i],
            )?;
        }
        Ok("done")
    })
    .expect("transaction");

    assert_eq!(out, "done");
    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM event"), 3);
}

#[test]
fn rollback_leaves_no_partial_rows() {
    let (_dir, mut store) = common::temp_store();
    common::seed_parents(&mut store).expect("seed");

    let err = with_immediate(store.conn_mut(), |tx| {
        for i in 0..3 {
            tx.execute(
                "INSERT INTO event (run_id, seq, kind, payload, at)
                 VALUES (NULL, ?1, 'RUN_CLAIMED', '{}', 0)",
                [i],
            )?;
        }
        // Fail *after* writing: the writes so far must not survive.
        Err::<(), StoreError>(StoreError::Domain("deliberate".to_string()))
    })
    .expect_err("must propagate the closure's error");

    assert!(matches!(err, StoreError::Domain(_)));
    assert_eq!(
        common::count(&store, "SELECT COUNT(*) FROM event"),
        0,
        "a rolled back transaction must leave no partial rows"
    );
}

#[test]
fn rollback_releases_the_write_lock() {
    // A helper that rolls back but forgets to end the transaction would poison
    // every later write. Assert the next transaction still works.
    let (_dir, mut store) = common::temp_store();
    common::seed_parents(&mut store).expect("seed");

    let _ = with_immediate(store.conn_mut(), |_tx| {
        Err::<(), StoreError>(StoreError::Domain("deliberate".to_string()))
    });

    with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT INTO event (run_id, seq, kind, payload, at)
             VALUES (NULL, 1, 'RUN_CLAIMED', '{}', 0)",
            [],
        )?;
        Ok(())
    })
    .expect("a later transaction must still be able to write");

    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM event"), 1);
}

#[test]
fn a_panic_inside_the_closure_does_not_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    let mut store = Store::open_or_create(&path).expect("open");
    common::seed_parents(&mut store).expect("seed");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: conductor_store::StoreResult<()> = with_immediate(store.conn_mut(), |tx| {
            tx.execute(
                "INSERT INTO event (run_id, seq, kind, payload, at)
                 VALUES (NULL, 1, 'RUN_CLAIMED', '{}', 0)",
                [],
            )?;
            panic!("deliberate panic mid-transaction");
        });
    }));
    assert!(result.is_err(), "the panic must propagate");

    drop(store);
    let store = Store::open_existing(&path).expect("reopen");
    assert_eq!(
        common::count(&store, "SELECT COUNT(*) FROM event"),
        0,
        "an unwound transaction must not have committed"
    );
}

#[test]
fn immediate_takes_the_write_lock_at_begin() {
    // This is the property the design depends on (Part 5.1): the lock is taken
    // up front, so two writers serialise at BEGIN instead of racing to upgrade
    // a read snapshot.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    let mut a = Store::open_or_create(&path).expect("open a");
    let mut b = Store::open_existing(&path).expect("open b");

    // Keep the second connection from waiting the full 5 s busy_timeout.
    b.conn()
        .execute_batch("PRAGMA busy_timeout = 50;")
        .expect("shorten busy_timeout");

    let tx_a = a
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("A begins immediate");

    let started = Instant::now();
    let err = b
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect_err("B must not get the write lock while A holds it");
    let waited = started.elapsed();

    assert!(
        err.to_string().to_lowercase().contains("lock")
            || err.to_string().to_lowercase().contains("busy"),
        "expected a busy/locked error, got: {err}"
    );
    assert!(
        waited < Duration::from_secs(3),
        "should have failed after the shortened busy_timeout, waited {waited:?}"
    );

    tx_a.rollback().expect("release");

    // Once A is done, B proceeds.
    let tx_b = b
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("B gets the lock after A releases");
    tx_b.rollback().expect("release");
}
