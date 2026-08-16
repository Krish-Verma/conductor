//! Persisting the attempt machine — §5.2 and schema v2.
//!
//! Schema v1 stored only `attempt.outcome`, so `CREATED`, `STARTING` and
//! `ACTIVE` had nowhere to live and a supervisor could not record that an
//! attempt was in flight. Startup recovery reads exactly that. Migration 2 adds
//! `attempt.state`; these tests are what that column exists for.

mod common;

use conductor_core::attempt::{Attempt, AttemptState};
use conductor_core::{AttemptId, AttemptOutcome, Fence, RunId};
use conductor_store::StoreError;
use conductor_store::attempt::NewAttempt;

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

fn new_attempt(ordinal: i64) -> NewAttempt {
    NewAttempt {
        id: AttemptId::new(format!("a-{ordinal:04}")).expect("attempt id"),
        ordinal,
        kind: "IMPLEMENT".to_string(),
        adapter: "fake".to_string(),
        launcher: "none".to_string(),
        caps_snapshot: "{}".to_string(),
        agent_session_id: None,
    }
}

fn state_of(store: &conductor_store::Store, id: &str) -> String {
    store
        .conn()
        .query_row("SELECT state FROM attempt WHERE id=?1", [id], |r| r.get(0))
        .expect("attempt state")
}

#[test]
fn a_created_attempt_is_persisted_as_in_flight() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);

    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create attempt");

    assert_eq!(attempt.state(), AttemptState::Created);
    assert_eq!(state_of(&store, "a-0001"), "CREATED");
    // The whole point of the column: recovery can see the attempt exists.
    assert!(AttemptState::Created.is_in_flight());
}

#[test]
fn the_supervisor_records_starting_then_active_with_the_process_identity() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);

    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create");
    let attempt = attempt.starting();
    store
        .record_attempt_starting(&fence, &attempt, NOW + 1)
        .expect("starting");
    assert_eq!(state_of(&store, "a-0001"), "STARTING");

    let attempt = attempt.active(4242, Some(1_786_604_521_844_678));
    store
        .record_attempt_active(&fence, &attempt, NOW + 2)
        .expect("active");
    assert_eq!(state_of(&store, "a-0001"), "ACTIVE");

    let (pid, start, started_at): (i64, i64, i64) = store
        .conn()
        .query_row(
            "SELECT pid, pid_start_time, started_at FROM attempt WHERE id='a-0001'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("row");
    assert_eq!(pid, 4242);
    assert_eq!(
        start, 1_786_604_521_844_678,
        "a pid without its start time cannot survive pid reuse"
    );
    assert_eq!(started_at, NOW + 2);
}

#[test]
fn a_spawn_whose_start_time_could_not_be_read_persists_null_and_never_a_zero() {
    // §4.7 step 3 probes "alive **and** start-time matches?". A start time the
    // supervisor could not read is an unanswerable second half, and the column
    // has to say so. `0` would not: it is a value, it compares, and it is the
    // one value [`conductor_run::supervise::probe`] used to read as "do not
    // check" — so persisting it turns "we could not identify the child" into
    // "adopt whatever holds this pid".
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);

    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create")
        .starting()
        .active(4242, None);
    store
        .record_attempt_active(&fence, &attempt, NOW + 1)
        .expect("active");

    let rows = store.attempts_for_run(fence.run_id()).expect("query");
    let row = rows.first().expect("a row");
    assert_eq!(
        row.pid,
        Some(4242),
        "the pid is still evidence; only the identity is missing"
    );
    assert_eq!(row.pid_start_time, None);
}

#[test]
fn a_terminal_attempt_records_state_and_outcome_separately() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create")
        .starting()
        .active(1, Some(1));

    let crashed = attempt.signalled(9);
    store
        .record_attempt_terminal(&fence, &crashed, NOW + 5)
        .expect("terminal");

    let (state, outcome, code, signal, ended): (
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    ) = store
        .conn()
        .query_row(
            "SELECT state, outcome, exit_code, signal, ended_at FROM attempt WHERE id='a-0001'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("row");
    assert_eq!(state, "CRASHED");
    assert_eq!(outcome, "CRASHED");
    assert_eq!(code, None);
    assert_eq!(signal, Some(9));
    assert_eq!(ended, Some(NOW + 5));
}

#[test]
fn a_stale_attempt_never_acquires_an_exit_code() {
    // §5.2: "`STALE` means we do not know, and unknown must not be recorded as
    // known." The database must not contain an exit code for one.
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create")
        .starting()
        .active(1, Some(1))
        .stale();

    store
        .record_attempt_terminal(&fence, &attempt, NOW + 5)
        .expect("terminal");

    let (state, outcome, code, signal): (String, String, Option<i64>, Option<i64>) = store
        .conn()
        .query_row(
            "SELECT state, outcome, exit_code, signal FROM attempt WHERE id='a-0001'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("row");
    assert_eq!(state, "STALE");
    assert_eq!(outcome, "STALE");
    assert_eq!(code, None);
    assert_eq!(signal, None);
}

#[test]
fn reconciled_keeps_the_outcome_the_attempt_actually_had() {
    // `attempt.outcome` in schema v1 had to carry RECONCILED, which destroyed
    // the classification. With `state` present, the two facts stay separate.
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create")
        .starting()
        .active(1, Some(1))
        .timed_out_idle();
    store
        .record_attempt_terminal(&fence, &attempt, NOW + 5)
        .expect("terminal");

    let reconciled = attempt.reconciled();
    store
        .record_attempt_reconciled(&fence, &reconciled, NOW + 6)
        .expect("reconciled");

    let (state, outcome): (String, String) = store
        .conn()
        .query_row(
            "SELECT state, outcome FROM attempt WHERE id='a-0001'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row");
    assert_eq!(state, "RECONCILED");
    assert_eq!(
        outcome, "TIMED_OUT",
        "the terminal classification survives reconciliation"
    );
}

#[test]
fn a_duplicate_attempt_ordinal_is_refused_by_the_schema() {
    // Acceptance: "duplicate attempt". Two supervisors that both believe they
    // own the run must not both be able to open attempt #1.
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);

    store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("first attempt");

    let mut duplicate = new_attempt(1);
    duplicate.id = AttemptId::new("a-other").expect("id");
    let err = store
        .create_attempt(&fence, duplicate, NOW)
        .expect_err("a second attempt at ordinal 1 must be refused");
    assert!(
        matches!(err, StoreError::Sqlite(_)),
        "expected the UNIQUE(run_id, ordinal) index to fire, got {err:?}"
    );
    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM attempt"), 1);
}

#[test]
fn every_attempt_write_is_fenced() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let stale = Fence::new(fence.run_id().clone(), fence.lease_epoch() - 1);

    let err = store
        .create_attempt(&stale, new_attempt(1), NOW)
        .expect_err("a stale worker must not open an attempt");
    assert!(matches!(err, StoreError::FencedOut { .. }));
    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM attempt"), 0);

    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create")
        .starting();
    let err = store
        .record_attempt_starting(&stale, &attempt, NOW)
        .expect_err("a stale worker must not advance an attempt");
    assert!(matches!(err, StoreError::FencedOut { .. }));
    assert_eq!(state_of(&store, "a-0001"), "CREATED");
}

#[test]
fn in_flight_attempts_are_what_recovery_reads() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create")
        .starting()
        .active(9911, Some(12345));
    store
        .record_attempt_active(&fence, &attempt, NOW)
        .expect("active");

    let in_flight = store.in_flight_attempts().expect("query");
    assert_eq!(in_flight.len(), 1);
    let row = &in_flight[0];
    assert_eq!(row.id.as_str(), "a-0001");
    assert_eq!(row.run_id.as_str(), "r-0001");
    assert_eq!(row.state, AttemptState::Active);
    assert_eq!(row.pid, Some(9911));
    assert_eq!(row.pid_start_time, Some(12345));
    assert_eq!(row.ordinal, 1);

    // Once terminal, it is no longer in flight.
    let terminal = attempt.exited(0);
    store
        .record_attempt_terminal(&fence, &terminal, NOW + 1)
        .expect("terminal");
    assert!(store.in_flight_attempts().expect("query").is_empty());
    assert_eq!(terminal.outcome(), AttemptOutcome::Exited);
}

#[test]
fn a_spawn_that_never_produced_a_process_is_stale_not_crashed() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    let attempt = store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create")
        .starting()
        .spawn_failed("No such file or directory");
    store
        .record_attempt_terminal(&fence, &attempt, NOW + 1)
        .expect("terminal");

    let (state, pid): (String, Option<i64>) = store
        .conn()
        .query_row(
            "SELECT state, pid FROM attempt WHERE id='a-0001'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("row");
    assert_eq!(state, "STALE");
    assert_eq!(pid, None, "there was never a process");
}

#[test]
fn attempt_ids_and_run_ids_round_trip_from_the_row() {
    let (_dir, mut store) = common::temp_store();
    let fence = claimed(&mut store);
    store
        .create_attempt(&fence, new_attempt(1), NOW)
        .expect("create");

    let rows = store
        .attempts_for_run(&RunId::new("r-0001").expect("run id"))
        .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, AttemptId::new("a-0001").expect("id"));
    assert_eq!(rows[0].adapter, "fake");
    assert_eq!(rows[0].launcher, "none");
}

#[test]
fn the_typestate_and_the_column_agree_on_every_state() {
    // Two encodings of one machine: the Rust phase and the persisted string.
    // They are asserted equal rather than assumed, because a drift here makes
    // recovery read a state the supervisor never meant.
    let names: Vec<&str> = AttemptState::ALL.iter().map(|s| s.as_str()).collect();
    let a = Attempt::create(
        AttemptId::new("a-x").expect("id"),
        RunId::new("r-x").expect("id"),
        1,
    );
    assert_eq!(a.state().as_str(), names[0]);
    let a = a.starting();
    assert_eq!(a.state().as_str(), names[1]);
    let a = a.active(1, Some(1));
    assert_eq!(a.state().as_str(), names[2]);
}
