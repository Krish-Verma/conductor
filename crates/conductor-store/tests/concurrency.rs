//! Concurrent-writer contention over the real claim, and the self-test that
//! gives the invariant checkers teeth.
//!
//! ADR-0004 decision 4 is binding: "Every future concurrency harness carries a
//! self-test. An invariant checker that has never been shown to fail is not
//! evidence." So this file asserts both directions — the checkers pass on a
//! genuine concurrent run, and they fail on deliberately corrupted state.

use std::process::Command;

const BENCH: &str = env!("CARGO_BIN_EXE_conductor-claim-bench");

fn run_bench(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(BENCH)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn {BENCH}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_invariant_checkers_fire_on_corrupted_state() {
    let (ok, stdout, stderr) = run_bench(&["--self-test"]);
    assert!(ok, "self-test failed\nstdout:\n{stdout}\nstderr:\n{stderr}");
    for expected in [
        "[PASS] I1 detects duplicate ownership",
        "[PASS] I2 detects partial transition",
        "[PASS] I3 detects epoch != 1",
        "[PASS] I4 detects a corrupted database file",
        "[PASS] I4 reports ok on a structurally sound db",
        "[PASS] all_pass is false when any checker fails",
    ] {
        assert!(
            stdout.contains(expected),
            "missing {expected:?} in self-test output:\n{stdout}"
        );
    }
}

#[test]
fn concurrent_writer_processes_claim_every_run_exactly_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("result.json");
    let workdir = dir.path().join("work");

    let (ok, stdout, stderr) = run_bench(&[
        "--writers",
        "4",
        "--rows",
        "120",
        "--repeat",
        "1",
        "--think-ms",
        "1",
        "--label",
        "test-contended",
        "--work-dir",
        workdir.to_str().expect("utf8"),
        "--out",
        out.to_str().expect("utf8"),
    ]);
    assert!(ok, "bench failed\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).expect("read result json"))
            .expect("parse result json");

    let summary = json["summary"]
        .as_array()
        .expect("summary array")
        .first()
        .expect("one writer count");

    assert_eq!(summary["writers"], 4);
    assert_eq!(
        summary["claims"], 120,
        "every seeded run must be claimed exactly once: {summary}"
    );
    assert_eq!(summary["duplicate_claims"], 0, "duplicate claim: {summary}");
    assert_eq!(summary["busy_errors"], 0, "busy errors: {summary}");
    assert_eq!(summary["other_error_count"], 0, "errors: {summary}");
    assert_eq!(
        summary["invariants_all_pass"], true,
        "invariants failed: {summary}"
    );
    assert!(
        summary["active_writers_mean"].as_f64().expect("f64") > 1.0,
        "writers never actually overlapped, so this measured nothing: {summary}"
    );

    // The instrument must record the stack it measured, or the numbers are not
    // attributable to anything.
    let meta = &json["meta"];
    assert!(
        meta["sqlite_version"]
            .as_str()
            .expect("sqlite_version")
            .starts_with('3'),
        "meta: {meta}"
    );
    assert_eq!(meta["pragmas"]["journal_mode"], "wal");
    assert_eq!(meta["pragmas"]["fullfsync"], "1");
}
