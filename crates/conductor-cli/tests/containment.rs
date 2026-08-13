//! `conductor doctor --containment` — master plan §7.1.
//!
//! The verification criterion for slice S2.5 is that this command *reproduces
//! Part 4.2's table*. These tests need the real `codex` binary for the measured
//! rows; where it is absent they assert the fail-closed rows instead.

use std::path::Path;
use std::process::{Command, Output};

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

fn run(args: &[&str]) -> Output {
    Command::new(CONDUCTOR)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not json ({e}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn store_arg(path: &Path) -> String {
    path.to_str().expect("utf8 path").to_string()
}

fn subject<'a>(
    report: &'a serde_json::Value,
    adapter: &str,
    launcher: &str,
) -> &'a serde_json::Value {
    report["containment"]["subjects"]
        .as_array()
        .expect("subjects is an array")
        .iter()
        .find(|s| s["adapter"] == adapter && s["launcher"] == launcher)
        .unwrap_or_else(|| panic!("no subject {adapter} x {launcher} in {report}"))
}

fn codex_present() -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join("codex").exists())
}

#[test]
fn containment_is_only_probed_when_it_is_asked_for() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");

    let out = run(&[
        "doctor",
        "--json",
        "--init-store",
        "--store",
        &store_arg(&db),
    ]);

    let report = json(&out);
    assert!(
        report["containment"].is_null(),
        "probing spawns real subprocesses; it must not happen unasked"
    );
}

#[test]
fn the_json_report_reproduces_the_capability_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");

    let out = run(&[
        "doctor",
        "--containment",
        "--json",
        "--init-store",
        "--store",
        &store_arg(&db),
    ]);
    let report = json(&out);

    // §4.2 records FakeAgent as n/a: Conductor's own code is not an adversary.
    let fake = subject(&report, "fake", "none");
    assert_eq!(fake["status"], "not_applicable");

    let codex = subject(&report, "codex", "codex-sandbox");
    if !codex_present() {
        assert_eq!(codex["status"], "adapter_absent");
        assert_eq!(codex["capabilities"]["filesystem_write"], "NONE");
        assert_eq!(codex["capabilities"]["network_egress"], "NONE");
        assert_eq!(codex["capabilities"]["control_surface"], "NONE");
        assert_eq!(codex["capabilities"]["credential_read"], "NONE");
        return;
    }

    assert_eq!(codex["status"], "measured");
    assert_eq!(codex["capabilities"]["filesystem_write"], "RESTRICTED");
    assert_eq!(codex["capabilities"]["network_egress"], "HARD");
    assert_eq!(codex["capabilities"]["control_surface"], "HARD");
    assert_eq!(codex["capabilities"]["credential_read"], "NONE");

    // Informational, and reported as unmeasured by this harness because
    // observing a hook requires a live agent session.
    assert_eq!(codex["capabilities"]["tool_interception"], "NONE");

    // The exception set behind Restricted is enumerated (M7).
    let exceptions = codex["capabilities"]["exceptions"]
        .as_array()
        .expect("exceptions is an array");
    assert!(
        exceptions
            .iter()
            .any(|path| path.as_str().unwrap_or_default() == "/tmp"),
        "{exceptions:?}"
    );

    // The AF_UNIX denial carries its positive control, in the report a human
    // reads — not only in the test suite.
    let unix = codex["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["id"] == "unix_socket_connect")
        .expect("the control-surface case");
    assert_eq!(unix["verdict"]["state"], "denied");
    assert_eq!(unix["control"]["kind"], "permission_flag");
    assert_eq!(unix["control"]["observation"]["state"], "allowed");
}

#[test]
fn a_measured_subject_is_cached_under_its_version_triple() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");

    let out = run(&[
        "doctor",
        "--containment",
        "--json",
        "--init-store",
        "--store",
        &store_arg(&db),
    ]);
    let report = json(&out);

    let cached = report["containment"]["cached_subjects"]
        .as_u64()
        .expect("cached_subjects is a number");
    if codex_present() {
        assert!(cached >= 1, "a measured subject must be cached: {report}");
        let codex = subject(&report, "codex", "codex-sandbox");
        assert!(
            codex["key"]["adapter_version"]
                .as_str()
                .unwrap_or_default()
                .starts_with("codex-cli"),
            "the cache key carries the adapter version verbatim: {}",
            codex["key"]
        );
        assert!(
            !codex["key"]["os_version"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
    }
}

#[test]
fn reporting_containment_never_creates_a_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("nested").join("conductor.db");

    let out = run(&[
        "doctor",
        "--containment",
        "--json",
        "--store",
        &store_arg(&db),
    ]);

    let report = json(&out);
    assert!(
        !db.exists(),
        "a capability probe must not bring a database into existence"
    );
    assert_eq!(report["containment"]["cached_subjects"], 0);
    assert!(
        report["containment"]["cache_note"]
            .as_str()
            .unwrap_or_default()
            .contains("store"),
        "the report must say why nothing was cached: {report}"
    );
    // The measurement itself still happened.
    assert!(
        report["containment"]["subjects"]
            .as_array()
            .expect("subjects")
            .len()
            >= 4
    );
}

#[test]
fn a_host_without_the_adapters_reports_them_absent_and_enforces_nothing() {
    // Failure injection, end to end: neither the adapter nor the sandbox
    // launcher is on PATH. Every dimension must fail closed, and the command
    // must still produce a report.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");

    let out = Command::new(CONDUCTOR)
        .args([
            "doctor",
            "--containment",
            "--json",
            "--init-store",
            "--store",
            &store_arg(&db),
        ])
        .env("PATH", "/nonexistent")
        .output()
        .expect("spawn");

    let report = json(&out);
    for (adapter, launcher) in [("codex", "codex-sandbox"), ("claude", "none")] {
        let subject = subject(&report, adapter, launcher);
        assert_eq!(
            subject["status"], "adapter_absent",
            "{adapter} x {launcher}: {subject}"
        );
        for dimension in [
            "filesystem_write",
            "network_egress",
            "control_surface",
            "credential_read",
        ] {
            assert_eq!(
                subject["capabilities"][dimension], "NONE",
                "{adapter} x {launcher} / {dimension}"
            );
        }
    }
    assert_eq!(report["containment"]["cached_subjects"], 0);
}

#[test]
fn the_human_rendering_is_the_capability_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");

    let out = run(&[
        "doctor",
        "--containment",
        "--init-store",
        "--store",
        &store_arg(&db),
    ]);
    let text = String::from_utf8_lossy(&out.stdout);

    for expected in [
        "containment",
        "filesystem_write",
        "network_egress",
        "control_surface",
        "credential_read",
        "tool_interception",
        "codex x codex-sandbox",
        "fake x none",
    ] {
        assert!(
            text.contains(expected),
            "the rendering must show {expected:?}:\n{text}"
        );
    }
    assert!(
        text.contains("informational"),
        "tool_interception must be marked as non-gating:\n{text}"
    );
}
