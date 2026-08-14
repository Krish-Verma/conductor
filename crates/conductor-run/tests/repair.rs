//! Bounded repair's pure half — master plan §4.6.
//!
//! Everything here is a function of its arguments: no store, no process, no
//! clock. That is deliberate. §4.6's three loop-breakers are the only thing
//! standing between a failing task and an unbounded spend, and a rule that can
//! only be exercised through a whole vertical is a rule nobody can test
//! exhaustively.
//!
//! **The normalization tests are the load-bearing ones.** If `normalize` fails
//! to strip one line number, two identical failures fingerprint differently,
//! `progressed()` returns true for a loop, and *every* loop-breaker silently
//! stops working — the system keeps running, the tests keep passing, and the
//! budget is the only thing left. So the inputs below are real compiler and test
//! output, not paraphrases of it.

use conductor_core::VerificationOutcome;
use conductor_run::repair::breaker::{
    AttemptResult, BudgetLimit, Decision, RepairHistory, StopReason, decide,
};
use conductor_run::repair::config::{
    RepairConfig, RetryKind, SessionPolicy, retry_kind, session_for_attempt, session_id_for,
};
use conductor_run::repair::failure::{Failure, progressed};
use conductor_run::repair::fingerprint::{Fingerprint, first_failing_assertion, normalize};
use conductor_run::verify::runner::{CheckKind, CheckResult, VerificationReport};

// ---------------------------------------------------------------------------
// D1 — normalization, against output real tools actually produce.
// ---------------------------------------------------------------------------

/// A `cargo test` failure, verbatim.
const CARGO_TEST_FAILURE: &str = "\
running 3 tests
test policy::tests::unknown_action_denies ... FAILED

failures:

---- policy::tests::unknown_action_denies stdout ----

thread 'policy::tests::unknown_action_denies' panicked at crates/conductor-run/src/policy/evaluate.rs:212:9:
assertion `left == right` failed
  left: Allow
 right: Deny
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

failures:
    policy::tests::unknown_action_denies

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
";

/// The same failure after the agent edited the file above it and the run moved
/// to a different workspace: fifteen lines further down, a different absolute
/// path, a different wall time. **The same failure.**
const CARGO_TEST_FAILURE_MOVED: &str = "\
running 3 tests
test policy::tests::unknown_action_denies ... FAILED

failures:

---- policy::tests::unknown_action_denies stdout ----

thread 'policy::tests::unknown_action_denies' panicked at /var/folders/8n/T/.tmpQ7x9/workspaces/r-0041/crates/conductor-run/src/policy/evaluate.rs:227:13:
assertion `left == right` failed
  left: Allow
 right: Deny
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

failures:
    policy::tests::unknown_action_denies

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.23s
";

/// A genuinely different failure: same test, different expectation.
const CARGO_TEST_FAILURE_DIFFERENT: &str = "\
thread 'policy::tests::unknown_action_denies' panicked at crates/conductor-run/src/policy/evaluate.rs:212:9:
assertion `left == right` failed
  left: Allow
 right: Warn
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
";

const RUSTC_ERROR: &str = "\
   Compiling conductor-run v0.1.0 (/var/folders/8n/T/.tmpQ7x9/workspaces/r-0041/crates/conductor-run)
error[E0308]: mismatched types
   --> crates/conductor-run/src/repair/driver.rs:42:20
    |
42  |     let bound: u32 = \"not a number\";
    |                ---   ^^^^^^^^^^^^^^ expected `u32`, found `&str`
    |                |
    |                expected due to this

error: aborting due to 1 previous error; 3 warnings emitted
For more information about this error, try `rustc --explain E0308`.
";

#[test]
fn normalization_strips_the_line_number_a_failure_moved_to() {
    // The single highest-value case in the slice. An agent that edits anything
    // above a failing assertion moves its line number; if that number survives
    // normalization the two failures look different and the loop runs on.
    let a = normalize("panicked at src/policy/evaluate.rs:212:9:");
    let b = normalize("panicked at src/policy/evaluate.rs:227:13:");
    assert_eq!(a, b, "a moved line number must not change the fingerprint");
}

#[test]
fn normalization_strips_the_workspace_path_a_run_happens_to_use() {
    // Every run gets its own clone (§4.1), so an absolute path contains the run
    // id. Left in, no two runs could ever share a fingerprint.
    let a = normalize("panicked at crates/conductor-run/src/policy/evaluate.rs:212:9:");
    let b = normalize(
        "panicked at /var/folders/8n/T/.tmpQ7x9/workspaces/r-0041/crates/conductor-run/src/policy/evaluate.rs:212:9:",
    );
    assert_eq!(a, b);
}

#[test]
fn normalization_strips_addresses_and_timings() {
    assert_eq!(
        normalize("signal 11 at 0x7f9c8a0b1234"),
        normalize("signal 11 at 0x600002a1c0f0"),
    );
    assert_eq!(
        normalize("test result: FAILED. 1 failed; finished in 0.04s"),
        normalize("test result: FAILED. 1 failed; finished in 12.71s"),
    );
    assert_eq!(normalize("took 15 ms"), normalize("took 3 ms"));
    assert_eq!(
        normalize("[2026-08-13T10:11:12.345Z] check failed"),
        normalize("[2026-08-14T23:59:00.001Z] check failed"),
    );
}

#[test]
fn normalization_keeps_the_values_that_distinguish_two_failures() {
    // The other half of the property, and the one an over-eager normalizer
    // destroys: strip *every* number and `left: 1 / right: 2` becomes
    // indistinguishable from `left: 3 / right: 4`, so two different bugs stop
    // the loop as if they were one.
    assert_ne!(
        normalize("assertion `left == right` failed\n  left: 1\n right: 2"),
        normalize("assertion `left == right` failed\n  left: 3\n right: 4"),
    );
    assert_ne!(
        normalize("error[E0308]: mismatched types"),
        normalize("error[E0433]: failed to resolve"),
    );
}

#[test]
fn normalization_is_idempotent() {
    // A normalizer that changes its own output has no fixed point, so equality
    // between two normalized strings would depend on how many times each had
    // been through it.
    for text in [CARGO_TEST_FAILURE, RUSTC_ERROR, "left: 1\nright: 2"] {
        let once = normalize(text);
        assert_eq!(normalize(&once), once, "not idempotent for {text:?}");
    }
}

#[test]
fn the_first_failing_assertion_is_the_assertion_and_not_the_whole_log() {
    let found = first_failing_assertion(CARGO_TEST_FAILURE).expect("cargo test failed loudly");

    // It starts at the panic, not at "running 3 tests".
    assert!(found.starts_with("thread 'policy"), "{found}");
    // It carries the values, because they are what makes two failures
    // different.
    assert!(found.contains("left: Allow"), "{found}");
    assert!(found.contains("right: Deny"), "{found}");
    // It stops: a "first failing assertion" that included the trailing summary
    // would carry `finished in 0.04s` into every fingerprint.
    assert!(!found.contains("test result:"), "{found}");
    assert!(!found.contains("RUST_BACKTRACE"), "{found}");
}

#[test]
fn the_first_failing_assertion_of_a_compiler_error_is_the_error_line() {
    let found = first_failing_assertion(RUSTC_ERROR).expect("rustc failed loudly");
    assert!(found.starts_with("error[E0308]"), "{found}");
}

#[test]
fn a_log_with_no_recognisable_marker_still_yields_something() {
    // `sh -c 'echo boom; exit 1'` has no marker. Returning None would mean the
    // fingerprint of every unrecognised failure was the same, which is the
    // conflation that stops the loop on failures that have nothing in common.
    let found = first_failing_assertion("boom\n").expect("a fallback is required");
    assert_eq!(found, "boom");
    assert_eq!(first_failing_assertion("   \n\n"), None, "nothing to say");
}

// ---------------------------------------------------------------------------
// D1 — the fingerprint itself.
// ---------------------------------------------------------------------------

#[test]
fn the_same_failure_at_a_different_line_has_the_same_fingerprint() {
    let a = Fingerprint::compute(
        ["unit-tests"],
        &first_failing_assertion(CARGO_TEST_FAILURE).expect("assertion"),
    );
    let b = Fingerprint::compute(
        ["unit-tests"],
        &first_failing_assertion(CARGO_TEST_FAILURE_MOVED).expect("assertion"),
    );
    assert_eq!(a, b, "normalization is what makes these equal");
}

#[test]
fn a_different_failure_has_a_different_fingerprint() {
    let a = Fingerprint::compute(
        ["unit-tests"],
        &first_failing_assertion(CARGO_TEST_FAILURE).expect("assertion"),
    );
    let b = Fingerprint::compute(
        ["unit-tests"],
        &first_failing_assertion(CARGO_TEST_FAILURE_DIFFERENT).expect("assertion"),
    );
    assert_ne!(a, b);
}

#[test]
fn the_failing_check_ids_are_sorted_before_hashing() {
    // §4.6 writes `sorted(failing_check_ids)`. Without the sort, the fingerprint
    // would depend on the order the schedule happened to run in.
    let a = Fingerprint::compute(["unit-tests", "typecheck"], "boom");
    let b = Fingerprint::compute(["typecheck", "unit-tests"], "boom");
    assert_eq!(a, b);
}

#[test]
fn which_checks_failed_is_part_of_the_fingerprint() {
    assert_ne!(
        Fingerprint::compute(["unit-tests"], "boom"),
        Fingerprint::compute(["unit-tests", "typecheck"], "boom"),
    );
}

#[test]
fn a_fingerprint_is_a_blake3_digest() {
    // ADR-0007: blake3 is the only digest, and every hash in the system is
    // written with its algorithm in front of it.
    let f = Fingerprint::compute(["unit-tests"], "boom");
    assert!(f.as_str().starts_with("blake3:"), "{f}");
}

// ---------------------------------------------------------------------------
// D2 — progressed(), exhaustively.
// ---------------------------------------------------------------------------

fn failure(checks: &[&str], assertion: &str, tree: &str) -> Failure {
    Failure::new(checks.iter().map(|c| c.to_string()), assertion, tree)
}

#[test]
fn strictly_fewer_failing_checks_is_progress() {
    let prev = failure(&["alpha", "beta"], "alpha: missing symbol", "tree-1");
    let next = failure(&["beta"], "beta: missing symbol", "tree-2");
    assert!(progressed(&prev, &next));
}

#[test]
fn strictly_fewer_failing_checks_is_progress_even_at_the_same_tree() {
    // §4.6's first disjunct has no tree clause. A check that started passing is
    // progress however it happened.
    let prev = failure(&["alpha", "beta"], "alpha: missing symbol", "tree-1");
    let next = failure(&["beta"], "beta: missing symbol", "tree-1");
    assert!(progressed(&prev, &next));
}

#[test]
fn the_same_failing_checks_are_not_a_strict_subset_of_themselves() {
    // ⊂ is proper containment. Reading it as ⊆ would make every repeated
    // failure look like progress, which disables the loop-breakers by
    // arithmetic rather than by bug.
    let prev = failure(&["alpha"], "alpha: missing symbol", "tree-1");
    let next = failure(&["alpha"], "alpha: missing symbol", "tree-2");
    assert!(!progressed(&prev, &next));
}

#[test]
fn more_failing_checks_is_not_progress() {
    let prev = failure(&["alpha"], "alpha: missing symbol", "tree-1");
    let next = failure(&["alpha", "beta"], "alpha: missing symbol", "tree-2");
    assert!(!progressed(&prev, &next));
}

#[test]
fn a_different_problem_after_a_real_edit_is_progress() {
    let prev = failure(&["alpha"], "alpha: missing symbol", "tree-1");
    let next = failure(&["alpha"], "alpha: type mismatch", "tree-2");
    assert!(progressed(&prev, &next));
}

#[test]
fn a_different_problem_without_an_edit_is_not_progress() {
    // Both clauses of the second disjunct are required. A check that reports
    // something new at an unchanged tree is flaky, not fixed — and spending an
    // attempt on it is exactly the waste §4.6 exists to bound.
    let prev = failure(&["alpha"], "alpha: missing symbol", "tree-1");
    let next = failure(&["alpha"], "alpha: type mismatch", "tree-1");
    assert!(!progressed(&prev, &next));
}

#[test]
fn the_same_problem_after_an_edit_is_not_progress() {
    let prev = failure(&["alpha"], "alpha: missing symbol", "tree-1");
    let next = failure(&["alpha"], "alpha: missing symbol", "tree-2");
    assert!(!progressed(&prev, &next));
}

#[test]
fn nothing_at_all_changed_is_not_progress() {
    let prev = failure(&["alpha"], "alpha: missing symbol", "tree-1");
    let next = failure(&["alpha"], "alpha: missing symbol", "tree-1");
    assert!(!progressed(&prev, &next));
}

// ---------------------------------------------------------------------------
// D3 — the three loop-breakers.
// ---------------------------------------------------------------------------

/// A history whose entries are `(assertion, tree)` failures of one check.
fn history(entries: &[(&str, &str)]) -> RepairHistory {
    let mut history = RepairHistory::new();
    for (assertion, tree) in entries {
        history.record(AttemptResult::Failed(failure(
            &["unit-tests"],
            assertion,
            tree,
        )));
    }
    history
}

/// Enough budget that no test below stops on the budget by accident.
fn roomy() -> RepairConfig {
    RepairConfig {
        max_attempts: 9,
        escalate_after: 9,
        ..RepairConfig::default()
    }
}

#[test]
fn breaker_1_an_identical_fingerprint_twice_stops_at_once() {
    let history = history(&[
        ("boom at src/a.rs:1:1", "tree-1"),
        ("boom at src/a.rs:9:1", "tree-2"),
    ]);
    assert_eq!(
        decide(&history, &roomy(), 99),
        Decision::Stop(StopReason::IdenticalFingerprint),
        "the two assertions differ only in a line number, which normalization strips"
    );
}

#[test]
fn breaker_1_can_be_turned_off_and_then_the_budget_is_all_that_is_left() {
    // The config key exists (§4.6: `stop_on_identical_fingerprint: true`), so
    // what it does when false has to be defined. It is also the control that
    // proves the breaker above is what stopped the loop.
    let config = RepairConfig {
        stop_on_identical_fingerprint: false,
        ..roomy()
    };
    let history = history(&[
        ("boom at src/a.rs:1:1", "tree-1"),
        ("boom at src/a.rs:9:1", "tree-2"),
    ]);
    assert_eq!(
        decide(&history, &config, 99),
        Decision::Stop(StopReason::NoProgress),
        "an identical failure is still not progress, it is just not reported as the same cause"
    );
}

#[test]
fn breaker_2_an_oscillation_stops_even_though_every_step_looked_like_progress() {
    // A→B→A. Each step is a different fingerprint at a changed tree, so
    // `progressed()` says yes every time and breaker 1 never fires. This is the
    // loop that only a window can see.
    let history = history(&[
        ("cannot resolve module 'alpha'", "tree-1"),
        ("type mismatch: expected u32", "tree-2"),
        ("cannot resolve module 'alpha'", "tree-3"),
    ]);
    assert!(
        progressed(
            &failure(&["unit-tests"], "type mismatch: expected u32", "tree-2"),
            &failure(&["unit-tests"], "cannot resolve module 'alpha'", "tree-3"),
        ),
        "the control: the last step passes progressed(), so only oscillation can stop it"
    );
    assert_eq!(
        decide(&history, &roomy(), 99),
        Decision::Stop(StopReason::Oscillation)
    );
}

#[test]
fn breaker_2_keeps_the_last_four_and_no_more() {
    // §4.6: "Detected by keeping the last 4." A repeat five back is outside the
    // window and must not stop a loop that is otherwise progressing — an
    // unbounded window would eventually stop every long-running repair.
    let history = history(&[
        ("alpha", "tree-1"),
        ("beta", "tree-2"),
        ("gamma", "tree-3"),
        ("delta", "tree-4"),
        ("alpha", "tree-5"),
    ]);
    assert_eq!(decide(&history, &roomy(), 99), Decision::Repair);
}

#[test]
fn breaker_3_a_repair_that_changed_nothing_stops() {
    let mut history = history(&[("boom", "tree-1")]);
    history.record(AttemptResult::NoChange);
    assert_eq!(
        decide(&history, &roomy(), 99),
        Decision::Stop(StopReason::EmptyEdit)
    );
}

#[test]
fn breaker_3_does_not_fire_on_the_first_attempt_of_all() {
    // Acceptance row 2: "Crash before edits … `NO_CHANGE` … new attempt, same
    // packet … `COMPLETE`". §4.6 breaks on "a **repair attempt** producing
    // NO_CHANGE"; the *first* attempt producing nothing is the row that is
    // supposed to get a retry.
    let mut history = RepairHistory::new();
    history.record(AttemptResult::NoChange);
    assert_eq!(decide(&history, &roomy(), 99), Decision::Repair);
}

// ---------------------------------------------------------------------------
// D4 — the budget.
// ---------------------------------------------------------------------------

#[test]
fn the_default_config_is_the_one_section_4_6_prints() {
    let config = RepairConfig::default();
    assert_eq!(config.max_attempts, 2);
    assert!(config.stop_on_identical_fingerprint);
    assert_eq!(config.escalate_after, 2);
    assert_eq!(config.new_session_on_attempt, 2);
}

#[test]
fn the_budget_counts_repairs_and_not_the_attempt_that_created_the_work() {
    // Two repairs are allowed after the initial attempt, and the third is
    // refused. Part 5.1's `attempt_budget` default of 3 is the same statement
    // from the other side: one initial attempt plus §4.6's two repairs.
    let config = RepairConfig::default();
    let mut history = history(&[("alpha", "tree-1")]);
    assert_eq!(decide(&history, &config, 3), Decision::Repair, "repair 1");

    history.record(AttemptResult::Failed(failure(
        &["unit-tests"],
        "beta",
        "tree-2",
    )));
    assert_eq!(decide(&history, &config, 3), Decision::Repair, "repair 2");

    history.record(AttemptResult::Failed(failure(
        &["unit-tests"],
        "gamma",
        "tree-3",
    )));
    assert_eq!(
        decide(&history, &config, 3),
        Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::RepairMaxAttempts
        }),
        "repair 3 is refused"
    );
}

#[test]
fn the_tasks_own_attempt_budget_binds_when_it_is_the_smaller_number() {
    // §4.7 bounds retries by `attempt_budget`; §4.6 bounds repairs by
    // `max_attempts`. Two limits over one thing, so the smaller must win —
    // otherwise a task whose durable budget is 2 quietly gets 3 attempts.
    let config = RepairConfig::default();
    let history = history(&[("alpha", "tree-1")]);
    assert_eq!(
        decide(&history, &config, 1),
        Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::TaskAttemptBudget
        })
    );
}

#[test]
fn escalate_after_binds_when_it_is_the_smaller_number() {
    let config = RepairConfig {
        max_attempts: 5,
        escalate_after: 1,
        ..RepairConfig::default()
    };
    let mut history = history(&[("alpha", "tree-1")]);
    assert_eq!(decide(&history, &config, 99), Decision::Repair);
    history.record(AttemptResult::Failed(failure(
        &["unit-tests"],
        "beta",
        "tree-2",
    )));
    assert_eq!(
        decide(&history, &config, 99),
        Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::EscalateAfter
        })
    );
}

// ---------------------------------------------------------------------------
// D5 — the two attempt outcomes the enum could not express before S6.
// ---------------------------------------------------------------------------

#[test]
fn a_crash_is_an_attempt_that_no_loop_breaker_may_read() {
    // Acceptance row 2: "Crash before edits … new attempt, same packet … yes …
    // `COMPLETE`". A crashed attempt produced no verification result at all, so
    // there is nothing for §4.6's breakers to compare — and treating the absence
    // as "the same failure again" or "an empty edit" would refuse the retry the
    // row requires.
    let mut history = history(&[("boom", "tree-1")]);
    history.record(AttemptResult::Crashed);
    assert_eq!(decide(&history, &roomy(), 99), Decision::Repair);

    let mut only_crashes = RepairHistory::new();
    only_crashes.record(AttemptResult::Crashed);
    only_crashes.record(AttemptResult::Crashed);
    assert_eq!(
        decide(&only_crashes, &roomy(), 99),
        Decision::Repair,
        "two crashes are two absences of evidence, not a loop"
    );
}

#[test]
fn a_crash_between_two_identical_failures_does_not_hide_the_loop() {
    // The control for the test above: a crash is skipped over, not treated as a
    // divider that resets the comparison. The same failure twice is the same
    // failure twice whatever happened in between.
    let mut history = history(&[("boom at src/a.rs:1:1", "tree-1")]);
    history.record(AttemptResult::Crashed);
    history.record(AttemptResult::Failed(failure(
        &["unit-tests"],
        "boom at src/a.rs:9:1",
        "tree-2",
    )));
    assert_eq!(
        decide(&history, &roomy(), 99),
        Decision::Stop(StopReason::IdenticalFingerprint)
    );
}

#[test]
fn a_crash_still_costs_the_work_budget() {
    // §4.7's exemption is for *infrastructure*, not for an agent that died. A
    // crashed agent consumed a spawn, a workspace and a wall-clock window; only
    // the loop-breakers are blind to it, never the budget.
    let config = RepairConfig::default();
    let mut history = RepairHistory::new();
    history.record(AttemptResult::Crashed);
    assert_eq!(decide(&history, &config, 99), Decision::Repair);
    history.record(AttemptResult::Crashed);
    assert_eq!(decide(&history, &config, 99), Decision::Repair);
    history.record(AttemptResult::Crashed);
    assert_eq!(
        decide(&history, &config, 99),
        Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::RepairMaxAttempts
        })
    );
}

#[test]
fn an_infrastructure_attempt_costs_no_work_budget_at_all() {
    // §4.7: an infrastructure retry "does **not** consume budget", and
    // "conflating them is how a broken API key silently exhausts a task's
    // budget". So a run whose adapter is broken must arrive at its own limit
    // with the whole work budget still in hand.
    let config = RepairConfig::default();
    let mut interrupted = history(&[("alpha", "tree-1")]);
    interrupted.record(AttemptResult::Infrastructure);
    assert_eq!(
        decide(&interrupted, &config, 2),
        Decision::Repair,
        "one work attempt and one broken adapter is not two work attempts"
    );

    // The control: replace the infrastructure entry with a work one and the
    // same task budget binds immediately.
    let mut spent = history(&[("alpha", "tree-1")]);
    spent.record(AttemptResult::Failed(failure(
        &["unit-tests"],
        "beta",
        "tree-2",
    )));
    assert_eq!(
        decide(&spent, &config, 2),
        Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::TaskAttemptBudget
        })
    );
}

#[test]
fn infrastructure_retries_have_their_own_bound() {
    // Acceptance row 8: "infra retry ×1, **no budget spent** … human after 2".
    // One initial attempt plus one retry is two invocations, and then a person.
    let config = RepairConfig::default();
    assert_eq!(config.max_infra_retries, 1);

    let mut history = RepairHistory::new();
    history.record(AttemptResult::Infrastructure);
    assert_eq!(decide(&history, &config, 99), Decision::Repair, "the ×1");

    history.record(AttemptResult::Infrastructure);
    assert_eq!(
        decide(&history, &config, 99),
        Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::InfrastructureRetries
        }),
        "and then a human"
    );
}

#[test]
fn the_infrastructure_bound_only_speaks_when_infrastructure_was_the_last_word() {
    // A report holding a `FAIL` "has real work to do" (§4.5), and reporting a
    // stale infrastructure count as the reason a human was called would send
    // that person looking at the adapter instead of at the failing check.
    let config = RepairConfig {
        max_infra_retries: 0,
        ..roomy()
    };
    let mut history = RepairHistory::new();
    history.record(AttemptResult::Infrastructure);
    history.record(AttemptResult::Failed(failure(
        &["unit-tests"],
        "alpha",
        "tree-1",
    )));
    assert_eq!(decide(&history, &config, 99), Decision::Repair);
}

// ---------------------------------------------------------------------------
// D4 — infrastructure retry is not a work retry (§4.7).
// ---------------------------------------------------------------------------

fn result(check_id: &str, outcome: VerificationOutcome) -> CheckResult {
    CheckResult {
        check_id: check_id.to_string(),
        kind: CheckKind::Required,
        outcome,
        tree_hash: "tree-1".to_string(),
        command_hash: "blake3:cmd".to_string(),
        from_cache: false,
        exit_code: Some(1),
        duration_ms: 1,
        log_path: None,
        log_digest: None,
        excerpt: None,
    }
}

fn report(results: Vec<CheckResult>) -> VerificationReport {
    VerificationReport {
        toolchain_fingerprint: "blake3:toolchain".to_string(),
        results,
        findings: Vec::new(),
    }
}

#[test]
fn a_failing_check_is_a_work_retry_and_costs_budget() {
    assert_eq!(
        retry_kind(&report(vec![result(
            "unit-tests",
            VerificationOutcome::Fail
        )])),
        RetryKind::Work
    );
}

#[test]
fn an_inconclusive_check_is_infrastructure_and_costs_no_budget() {
    // §4.5: "`FAIL` → repair; `INCONCLUSIVE` → bounded infra retry, then human."
    // §4.7: conflating the two "is how a broken API key silently exhausts a
    // task's budget".
    assert_eq!(
        retry_kind(&report(vec![result(
            "unit-tests",
            VerificationOutcome::Inconclusive
        )])),
        RetryKind::Infrastructure
    );
}

#[test]
fn a_void_result_is_infrastructure_because_nothing_was_observed() {
    // Row 26: "re-run at the new tree" — a verification action, not an agent
    // one. `VOID` says the tree moved under the check, which is a statement
    // about the run, never about the code.
    assert_eq!(
        retry_kind(&report(vec![result(
            "unit-tests",
            VerificationOutcome::Void
        )])),
        RetryKind::Infrastructure
    );
}

#[test]
fn a_failure_outranks_an_inconclusive_in_the_same_report() {
    // A report holding both has real work to do; retrying the infrastructure
    // would leave the failing check failing.
    assert_eq!(
        retry_kind(&report(vec![
            result("typecheck", VerificationOutcome::Inconclusive),
            result("unit-tests", VerificationOutcome::Fail),
        ])),
        RetryKind::Work
    );
}

#[test]
fn an_all_green_report_needs_no_retry_of_either_kind() {
    assert_eq!(
        retry_kind(&report(vec![result(
            "unit-tests",
            VerificationOutcome::Pass
        )])),
        RetryKind::None
    );
}

// ---------------------------------------------------------------------------
// D7 — a stuck agent's context is the problem.
// ---------------------------------------------------------------------------

#[test]
fn attempt_two_gets_a_fresh_session() {
    let config = RepairConfig::default();
    assert_eq!(session_for_attempt(1, &config), SessionPolicy::Resume);
    assert_eq!(session_for_attempt(2, &config), SessionPolicy::Fresh);
    assert_eq!(session_for_attempt(3, &config), SessionPolicy::Fresh);
}

#[test]
fn a_fresh_session_carries_no_session_id_even_when_one_exists() {
    // The whole point: the previous session is *available* and is deliberately
    // not used. §4.6: "a stuck agent's context is the problem, and resuming
    // re-imports the stuckness."
    assert_eq!(
        session_id_for(SessionPolicy::Fresh, true, Some("session-from-attempt-1")),
        None
    );
}

#[test]
fn resuming_uses_the_previous_session_when_the_adapter_can() {
    assert_eq!(
        session_id_for(SessionPolicy::Resume, true, Some("session-from-attempt-1")),
        Some("session-from-attempt-1".to_string())
    );
}

#[test]
fn an_adapter_that_cannot_resume_never_receives_a_session_to_resume() {
    // §6.1's capability table is not decoration: handing a session id to an
    // adapter that declares `session_resume: false` would be Conductor asking
    // for something it has been told is not there.
    assert_eq!(
        session_id_for(SessionPolicy::Resume, false, Some("session-from-attempt-1")),
        None
    );
    assert_eq!(session_id_for(SessionPolicy::Resume, true, None), None);
}
