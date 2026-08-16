//! The attempt state machine — master plan §5.2, "Attempt (8 states)".
//!
//! ```text
//! CREATED → STARTING → ACTIVE ─┬─► EXITED     ─┐
//!                              ├─► CRASHED    ─┤
//!                              ├─► TIMED_OUT  ─┼─► RECONCILED (terminal)
//!                              └─► STALE      ─┘
//! ```
//!
//! Two invariants this file exists to pin:
//!
//! 1. **Every path ends at `RECONCILED`** — and the only way to get there is from
//!    a terminal outcome, never straight from `ACTIVE`. §5.2: "`RECONCILING` is
//!    mandatory and unskippable — enforced in the type system, not by
//!    convention." The illegal edges are proven absent by `compile_fail`
//!    doctests on the types themselves; the tests here pin the legal ones.
//! 2. **`STALE` ≠ `CRASHED`.** `CRASHED` is an observed nonzero exit; `STALE` is
//!    "we do not know". Nothing may turn the second into the first.

use conductor_core::attempt::{Attempt, AttemptState, TerminalAttempt};
use conductor_core::{AttemptId, AttemptOutcome, RunId, RunState};

fn ids() -> (AttemptId, RunId) {
    (
        AttemptId::new("a-0001").expect("attempt id"),
        RunId::new("r-0041").expect("run id"),
    )
}

fn created() -> Attempt<conductor_core::attempt::phase::Created> {
    let (attempt_id, run_id) = ids();
    Attempt::create(attempt_id, run_id, 1)
}

#[test]
fn the_machine_has_exactly_eight_states() {
    assert_eq!(AttemptState::ALL.len(), 8);
    let names: Vec<&str> = AttemptState::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "CREATED",
            "STARTING",
            "ACTIVE",
            "EXITED",
            "CRASHED",
            "TIMED_OUT",
            "STALE",
            "RECONCILED",
        ]
    );
}

#[test]
fn the_happy_path_walks_created_starting_active_exited_reconciled() {
    let a = created();
    assert_eq!(a.state(), AttemptState::Created);

    let a = a.starting();
    assert_eq!(a.state(), AttemptState::Starting);

    let a = a.active(4242, Some(1_786_604_521_844_678));
    assert_eq!(a.state(), AttemptState::Active);
    assert_eq!(a.pid(), 4242);
    assert_eq!(a.pid_start_time(), Some(1_786_604_521_844_678));

    let a = a.exited(0);
    assert_eq!(a.state(), AttemptState::Exited);
    assert_eq!(a.outcome(), AttemptOutcome::Exited);

    let a = a.reconciled();
    assert_eq!(a.state(), AttemptState::Reconciled);
    assert!(a.state().is_terminal());
}

#[test]
fn a_nonzero_exit_is_crashed_and_a_zero_exit_is_not() {
    let a = created().starting().active(1, Some(1)).exited(0);
    assert_eq!(a.state(), AttemptState::Exited);

    let a = created().starting().active(1, Some(1)).exited(7);
    assert_eq!(
        a.state(),
        AttemptState::Crashed,
        "§6.4: exit != 0 is CRASHED"
    );
    assert_eq!(a.exit_code(), Some(7));
}

#[test]
fn a_signal_death_is_crashed_and_records_the_signal() {
    let a = created().starting().active(1, Some(1)).signalled(9);
    assert_eq!(a.state(), AttemptState::Crashed);
    assert_eq!(a.signal(), Some(9));
    assert_eq!(a.exit_code(), None);
}

#[test]
fn stale_is_not_crashed_because_unknown_is_not_known() {
    let stale = created().starting().active(1, Some(1)).stale();
    assert_eq!(stale.state(), AttemptState::Stale);
    assert_eq!(stale.outcome(), AttemptOutcome::Stale);
    // The distinction §5.2 insists on: no exit was observed, so neither an exit
    // code nor a signal may be recorded.
    assert_eq!(stale.exit_code(), None);
    assert_eq!(stale.signal(), None);
    assert_ne!(stale.outcome(), AttemptOutcome::Crashed);
}

#[test]
fn every_terminal_outcome_reaches_reconciled() {
    let outcomes = [
        created()
            .starting()
            .active(1, Some(1))
            .exited(0)
            .reconciled(),
        created()
            .starting()
            .active(1, Some(1))
            .exited(3)
            .reconciled(),
        created()
            .starting()
            .active(1, Some(1))
            .timed_out_wall()
            .reconciled(),
        created()
            .starting()
            .active(1, Some(1))
            .timed_out_idle()
            .reconciled(),
        created().starting().active(1, Some(1)).stale().reconciled(),
    ];
    for a in outcomes {
        assert_eq!(a.state(), AttemptState::Reconciled);
    }
}

#[test]
fn a_timeout_is_timed_out_and_carries_its_reason() {
    let wall = created().starting().active(1, Some(1)).timed_out_wall();
    assert_eq!(wall.state(), AttemptState::TimedOut);
    assert_eq!(wall.outcome(), AttemptOutcome::TimedOut);
    assert_eq!(wall.timeout_reason(), Some("wall_clock"));

    let idle = created().starting().active(1, Some(1)).timed_out_idle();
    assert_eq!(idle.state(), AttemptState::TimedOut);
    // §6.4: "no output for idle_timeout → TIMED_OUT, reason=stall".
    assert_eq!(idle.timeout_reason(), Some("stall"));
}

#[test]
fn spawning_can_fail_before_the_process_exists_and_that_is_not_a_crash() {
    // An attempt that never became a process has no pid to classify. §4.7 calls
    // this an infrastructure failure: it must not look like the agent died.
    let a = created().starting().spawn_failed("no such file");
    assert_eq!(a.state(), AttemptState::Stale);
    assert_eq!(a.exit_code(), None);
    assert_eq!(a.signal(), None);
}

#[test]
fn a_run_can_only_leave_running_by_going_to_reconciling() {
    let terminal: TerminalAttempt = created().starting().active(1, Some(1)).exited(0).evidence();
    assert_eq!(RunState::leave_running(&terminal), RunState::Reconciling);

    // …and that holds for every terminal outcome, including the ones a naive
    // implementation would be tempted to route straight to COMPLETE.
    for t in [
        created().starting().active(1, Some(1)).exited(0).evidence(),
        created().starting().active(1, Some(1)).exited(9).evidence(),
        created().starting().active(1, Some(1)).stale().evidence(),
        created()
            .starting()
            .active(1, Some(1))
            .timed_out_wall()
            .evidence(),
    ] {
        assert_eq!(RunState::leave_running(&t), RunState::Reconciling);
    }
}

#[test]
fn terminal_evidence_names_the_attempt_it_came_from() {
    let a = created().starting().active(77, Some(5)).exited(0);
    let evidence = a.evidence();
    assert_eq!(evidence.attempt_id().as_str(), "a-0001");
    assert_eq!(evidence.outcome(), AttemptOutcome::Exited);
}

#[test]
fn attempt_state_round_trips_through_text() {
    for s in AttemptState::ALL {
        assert_eq!(&s.as_str().parse::<AttemptState>().expect("parse"), s);
    }
    assert!("RECOVERING".parse::<AttemptState>().is_err());
}
