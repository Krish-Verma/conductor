//! Failure injection for the containment harness.
//!
//! Slice S2.5 names four: adapter binary absent · adapter version bumped →
//! cache invalidated · probe times out · sandbox launcher missing. Each must
//! produce a defined, fail-closed outcome rather than an error, a crash, or —
//! worst of all — a confident `Hard`.
//!
//! The two setup defects that invalidated S0's first round are injected here
//! too: a scratch root inside a permitted region, and a socket path over the
//! `sun_path` limit.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use conductor_core::containment::Enforcement;
use conductor_run::containment::ContainmentError;
use conductor_run::containment::cache::{self, CacheLookup};
use conductor_run::containment::probe::{
    Adapter, CaseId, CaseVerdict, Host, Launcher, ProbeConfig, ProbeStatus, Subject, ToolInfo,
    probe_subject,
};
use conductor_store::Store;

/// A host with named tools and nothing else, so "not installed" can be modelled
/// without touching this machine's `PATH`.
fn host_with(tools: &[(&str, &str)]) -> Host {
    let mut map = BTreeMap::new();
    for (name, version) in tools {
        map.insert(
            (*name).to_string(),
            ToolInfo {
                path: PathBuf::from(format!("/nonexistent/{name}")),
                version: (*version).to_string(),
            },
        );
    }
    Host::new("macOS 26.6 (25G72) arm64", map)
}

#[test]
fn an_absent_adapter_is_a_reported_fact_with_fail_closed_capabilities() {
    let report = probe_subject(
        Subject::new(Adapter::Codex, Launcher::CodexSandbox),
        &host_with(&[]),
        &common::config(),
    )
    .expect("an uninstalled adapter is not an error");

    assert_eq!(report.status, ProbeStatus::AdapterAbsent);
    assert!(report.capabilities.is_fail_closed());
    assert!(
        report.key.is_none(),
        "there is no version to key a cache on"
    );
    assert!(
        report.notes.iter().any(|note| note.contains("codex")),
        "the report must name what is missing: {:?}",
        report.notes
    );
    assert!(report.cases.is_empty(), "nothing may be run");
}

#[test]
fn an_absent_launcher_is_a_reported_fact_with_fail_closed_capabilities() {
    // The adapter is installed; the thing that would have contained it is not.
    let report = probe_subject(
        Subject::new(Adapter::Claude, Launcher::CodexSandbox),
        &host_with(&[("claude", "2.1.228 (Claude Code)")]),
        &common::config(),
    )
    .expect("an uninstalled launcher is not an error");

    assert_eq!(report.status, ProbeStatus::LauncherAbsent);
    assert!(report.capabilities.is_fail_closed());
    assert!(report.cases.is_empty());
}

#[test]
fn a_deadline_no_case_can_meet_yields_broken_never_hard() {
    // A timeout is the most dangerous failure in this harness: every case
    // "fails", and a naive classifier would read a wall of failures as a very
    // strong sandbox.
    let config = ProbeConfig::new(
        common::root("timeout"),
        PathBuf::from(common::PAYLOAD),
        Duration::from_millis(0),
    )
    .expect("valid root");

    let report = probe_subject(
        Subject::new(Adapter::Claude, Launcher::None),
        &host_with(&[("claude", "2.1.228 (Claude Code)")]),
        &config,
    )
    .expect("a timeout is not an error");

    assert_eq!(report.status, ProbeStatus::Broken);
    assert!(report.capabilities.is_fail_closed());
    let liveness = report
        .cases
        .iter()
        .find(|case| case.id == CaseId::LauncherLiveness)
        .expect("liveness runs first");
    match &liveness.verdict {
        CaseVerdict::Broken { reason } => assert!(
            reason.contains("timed out"),
            "the reason must say so: {reason}"
        ),
        other => panic!("a timed-out case must be Broken, got {other:?}"),
    }
}

#[test]
fn a_probe_root_inside_a_permitted_region_is_refused_before_anything_runs() {
    // S0 round 1's first defect: the "outside the workspace" directory was
    // itself under /tmp, which `workspace-write` permits (M7). Every escape it
    // reported had not happened.
    let tmpdir = std::env::temp_dir();
    for root in [
        tmpdir.join("conductor-probe-invalid"),
        PathBuf::from("/tmp/conductor-probe-invalid"),
    ] {
        let err = ProbeConfig::new(
            root.clone(),
            PathBuf::from(common::PAYLOAD),
            Duration::from_secs(5),
        )
        .expect_err("a root inside a permitted region must be refused");

        match err {
            ContainmentError::Untrustworthy(reason) => {
                assert!(
                    reason.contains("permits"),
                    "the refusal must explain itself: {reason}"
                );
            }
            other => panic!("expected Untrustworthy, got {other:?}"),
        }
    }
}

#[test]
fn a_socket_path_over_the_sun_path_limit_is_refused_before_anything_connects() {
    // S0 round 1's second defect: the AF_UNIX test failed on `sun_path` length
    // and the failure was read as a sandbox denial.
    let home = std::env::var_os("HOME").expect("HOME");
    let root = PathBuf::from(home).join(".conductor").join("x".repeat(120));

    let err = ProbeConfig::new(root, PathBuf::from(common::PAYLOAD), Duration::from_secs(5))
        .expect_err("an unusable socket path must be refused");

    match err {
        ContainmentError::Untrustworthy(reason) => assert!(
            reason.contains("sun_path"),
            "the refusal must name the limit: {reason}"
        ),
        other => panic!("expected Untrustworthy, got {other:?}"),
    }
}

#[test]
fn a_probe_root_that_already_exists_is_refused_so_cleanup_cannot_destroy_it() {
    // The probe removes its own root when it finishes. Pointed at a directory
    // that holds anything else — `$HOME`, a repository — that cleanup would be
    // destructive, so the root must be one the probe creates itself.
    let home = std::env::var_os("HOME").expect("HOME");

    let err = ProbeConfig::new(
        PathBuf::from(home),
        PathBuf::from(common::PAYLOAD),
        Duration::from_secs(5),
    )
    .expect_err("an existing directory must be refused as a probe root");

    match err {
        ContainmentError::Untrustworthy(reason) => assert!(
            reason.contains("already exists"),
            "the refusal must say why: {reason}"
        ),
        other => panic!("expected Untrustworthy, got {other:?}"),
    }
}

#[test]
fn an_absent_payload_is_refused_before_anything_runs() {
    // Without this check every case would fail to start, and a failure to start
    // is exactly what must never be mistaken for a denial.
    let err = ProbeConfig::new(
        common::root("no-payload"),
        PathBuf::from("/nonexistent/conductor-probe-action"),
        Duration::from_secs(5),
    )
    .expect_err("an absent payload must be refused");

    assert!(matches!(err, ContainmentError::Untrustworthy(_)), "{err:?}");
}

#[test]
fn an_adapter_upgrade_invalidates_the_measurement_it_invalidated() {
    // The end-to-end version-bump path: measure, cache, upgrade the adapter,
    // and find that the host is once again unmeasured.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open_or_create(dir.path().join("conductor.db")).expect("store");

    let host = host_with(&[("claude", "2.1.228 (Claude Code)")]);
    let report = probe_subject(
        Subject::new(Adapter::Claude, Launcher::None),
        &host,
        &common::config(),
    )
    .expect("probe");
    assert_eq!(report.status, ProbeStatus::Measured);
    let key = report.key.clone().expect("a measured subject has a key");
    cache::upsert(store.conn_mut(), &key, &report.capabilities, 1).expect("cache");
    assert!(cache::lookup(store.conn(), &key).expect("query").is_hit());

    // codex-cli 0.142.0 → 0.143.0, or Claude Code 2.1.228 → 2.2.0: the cached
    // row describes a host that no longer exists.
    let upgraded = host_with(&[("claude", "2.2.0 (Claude Code)")]);
    let after = probe_subject(
        Subject::new(Adapter::Claude, Launcher::None),
        &upgraded,
        &common::config(),
    )
    .expect("probe");
    let new_key = after.key.clone().expect("key");
    assert_ne!(new_key, key);

    let lookup = cache::lookup(store.conn(), &new_key).expect("query");
    assert!(matches!(lookup, CacheLookup::Miss));
    assert!(
        lookup.capabilities().is_fail_closed(),
        "after an upgrade the host is unmeasured until it is re-probed"
    );
}

#[test]
fn nothing_the_harness_reports_can_exceed_none_without_a_measurement() {
    // The invariant behind every case above, stated once: any status other than
    // `measured` means every gating dimension is `None`.
    let configs = [
        (
            Subject::new(Adapter::Fake, Launcher::None),
            host_with(&[]),
            ProbeStatus::NotApplicable,
        ),
        (
            Subject::new(Adapter::Codex, Launcher::CodexSandbox),
            host_with(&[]),
            ProbeStatus::AdapterAbsent,
        ),
        (
            Subject::new(Adapter::Claude, Launcher::CodexSandbox),
            host_with(&[("claude", "2.1.228")]),
            ProbeStatus::LauncherAbsent,
        ),
    ];

    for (subject, host, expected) in configs {
        let report = probe_subject(subject, &host, &common::config()).expect("probe");
        assert_eq!(report.status, expected);
        for (dimension, enforcement) in report.capabilities.gating_dimensions() {
            assert_eq!(
                enforcement,
                Enforcement::None,
                "{dimension} on an unmeasured subject"
            );
        }
    }
}
