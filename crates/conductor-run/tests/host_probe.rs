//! The probe against the real host.
//!
//! Master plan S2.5: *"Probe results match the M6–M12 matrix on this host"* and
//! *"the AF_UNIX probe includes its positive control and fails if the control
//! does not connect"*.
//!
//! **These tests need the real `codex` binary.** Where it is absent they assert
//! the fail-closed path instead of failing spuriously — which is itself the
//! behaviour §4.2 requires of an unmeasured host.
//!
//! No model is invoked. `codex sandbox` runs a command under the seatbelt policy
//! and makes no API call (M13).

mod common;

use conductor_core::containment::{Enforcement, GatingDimension};
use conductor_run::containment::probe::{
    Adapter, CaseId, CaseVerdict, ControlKind, Host, Launcher, Observation, ProbeStatus, Subject,
    SubjectReport, probe_all, probe_subject,
};

fn case(report: &SubjectReport, id: CaseId) -> &conductor_run::containment::probe::CaseReport {
    report
        .cases
        .iter()
        .find(|case| case.id == id)
        .unwrap_or_else(|| panic!("case {id} did not run; cases: {:?}", report.cases))
}

#[test]
fn codex_sandbox_reproduces_the_measured_matrix() {
    let host = Host::detect();
    let config = common::config();
    let subject = Subject::new(Adapter::Codex, Launcher::CodexSandbox);

    let report = probe_subject(subject, &host, &config).expect("probing must not error");

    if host.tool("codex").is_none() {
        assert_eq!(report.status, ProbeStatus::AdapterAbsent);
        assert!(report.capabilities.is_fail_closed());
        return;
    }

    assert_eq!(
        report.status,
        ProbeStatus::Measured,
        "notes: {:?}",
        report.notes
    );

    // §4.2's Codex column: Restricted / Hard / Hard / None.
    assert_eq!(
        report.capabilities.filesystem_write,
        Enforcement::Restricted,
        "M6-M8: writes outside the workspace are denied, with exceptions. {:?}",
        report.dimensions
    );
    // M9. On a machine with no network the positive control cannot succeed, so
    // the dimension is *unmeasured* — and asserting `Hard` there would be the
    // exact error this harness exists to prevent: reading "the operation failed"
    // as "the sandbox stopped it".
    let network_control_worked = case(&report, CaseId::NetTcpConnect)
        .control
        .as_ref()
        .is_some_and(|control| control.observation == Observation::Allowed);
    if network_control_worked {
        assert_eq!(
            report.capabilities.network_egress,
            Enforcement::Hard,
            "M9. {:?}",
            report.dimensions
        );
    } else {
        assert_eq!(
            report.capabilities.network_egress,
            Enforcement::None,
            "this host has no outbound network, so egress containment cannot be \
             distinguished from an offline machine and must fail closed. {:?}",
            report.dimensions
        );
    }
    assert_eq!(
        report.capabilities.control_surface,
        Enforcement::Hard,
        "M10/M11. {:?}",
        report.dimensions
    );
    assert_eq!(
        report.capabilities.credential_read,
        Enforcement::None,
        "M12: reads are not restricted. {:?}",
        report.dimensions
    );

    // M7: the exception set is enumerated, not merely implied by "Restricted".
    let exceptions: Vec<String> = report
        .capabilities
        .exceptions
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    assert!(
        exceptions.contains(&"/tmp".to_string()),
        "M7: /tmp must appear in the exception set: {exceptions:?}"
    );
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        let tmpdir = tmpdir.to_string_lossy().trim_end_matches('/').to_string();
        assert!(
            exceptions.contains(&tmpdir),
            "M7: $TMPDIR ({tmpdir}) must appear in the exception set: {exceptions:?}"
        );
    }
    assert_eq!(
        exceptions.len(),
        if std::env::var_os("TMPDIR").is_some() {
            2
        } else {
            1
        },
        "M6/M7: /tmp and $TMPDIR are the *only* exceptions; anything else here is a \
         write outside the workspace that Conductor did not know about: {exceptions:?}"
    );

    // M8: the restriction is inherited by child processes.
    assert_eq!(
        case(&report, CaseId::FsWriteOutsideNestedShell).verdict,
        CaseVerdict::Denied
    );
    // M15, and the precondition for reading anything above as containment.
    assert_eq!(
        case(&report, CaseId::LauncherLiveness).verdict,
        CaseVerdict::Allowed
    );
}

#[test]
fn the_af_unix_denial_is_backed_by_its_positive_control() {
    // The slice's named acceptance test. A control that does not connect means
    // the probe is broken, and the probe must say so rather than report a
    // denial — this is the failure that invalidated S0 round 1 (ADR-0002).
    let host = Host::detect();
    let config = common::config();
    let subject = Subject::new(Adapter::Codex, Launcher::CodexSandbox);

    let report = probe_subject(subject, &host, &config).expect("probing must not error");
    if host.tool("codex").is_none() {
        assert!(report.capabilities.is_fail_closed());
        return;
    }

    let unix = case(&report, CaseId::UnixSocketConnect);
    let control = unix
        .control
        .as_ref()
        .expect("a denied AF_UNIX connect must carry a positive control");

    assert_eq!(
        control.kind,
        ControlKind::PermissionFlag,
        "the control must hold the launcher constant and vary only the permission (M11)"
    );
    assert_eq!(
        control.observation,
        Observation::Allowed,
        "the identical connect must succeed under --allow-unix-socket, or the denial is \
         indistinguishable from a broken test"
    );
    assert_eq!(unix.verdict, CaseVerdict::Denied);
    assert_eq!(report.capabilities.control_surface, Enforcement::Hard);
}

#[test]
fn the_bare_launcher_measures_that_nothing_is_enforced() {
    // §4.2's "Claude Code (bare)" column is `None` across the board. That has to
    // be *observed* — a declared None is exactly what this slice exists to stop.
    let host = Host::detect();
    let config = common::config();
    let subject = Subject::new(Adapter::Claude, Launcher::None);

    let report = probe_subject(subject, &host, &config).expect("probing must not error");

    if host.tool("claude").is_none() {
        assert_eq!(report.status, ProbeStatus::AdapterAbsent);
        assert!(report.capabilities.is_fail_closed());
        return;
    }

    assert_eq!(report.status, ProbeStatus::Measured, "{:?}", report.notes);
    assert!(
        report.capabilities.is_fail_closed(),
        "an unlaunched agent enforces nothing: {:?}",
        report.capabilities
    );
    // …and it is a measurement, not a default: the writes actually landed.
    assert_eq!(
        case(&report, CaseId::FsWriteOutsideSibling).verdict,
        CaseVerdict::Allowed,
        "with no launcher, a write outside the workspace must succeed"
    );
    assert_eq!(
        case(&report, CaseId::ReadPlantedSecret).verdict,
        CaseVerdict::Allowed
    );
    for dimension in GatingDimension::ALL {
        let report = report
            .dimensions
            .iter()
            .find(|d| d.dimension == *dimension)
            .expect("every dimension is reported");
        assert!(
            report.measured,
            "{dimension} must be measured, not assumed: {report:?}"
        );
    }
}

#[test]
fn the_launcher_is_what_changes_the_outcome() {
    // A permanent non-vacuity guard, in the spirit of ADR-0006's negative
    // control. If the probe were not really applying the launcher — wrong
    // argument order, a workspace that happens to be writable everywhere, a
    // payload that never ran — the sandboxed and unlaunched runs would agree.
    // They must not.
    let host = Host::detect();
    if host.tool("codex").is_none() {
        return;
    }

    let sandboxed = probe_subject(
        Subject::new(Adapter::Codex, Launcher::CodexSandbox),
        &host,
        &common::config(),
    )
    .expect("probe");
    let unlaunched = probe_subject(
        Subject::new(Adapter::Codex, Launcher::None),
        &host,
        &common::config(),
    )
    .expect("probe");

    assert_eq!(
        case(&unlaunched, CaseId::FsWriteOutsideSibling).verdict,
        CaseVerdict::Allowed,
        "the identical write must succeed with no launcher"
    );
    assert_eq!(
        case(&sandboxed, CaseId::FsWriteOutsideSibling).verdict,
        CaseVerdict::Denied,
        "…and be denied under the launcher"
    );
    assert_ne!(
        sandboxed.capabilities, unlaunched.capabilities,
        "the launcher must be what makes the difference"
    );
}

#[test]
fn the_fake_agent_is_not_probed_at_all() {
    // §4.2: "FakeAgent is Conductor's own code and not an adversary; recording
    // it as Hard would be a category error."
    let host = Host::detect();
    let config = common::config();

    let report = probe_subject(Subject::new(Adapter::Fake, Launcher::None), &host, &config)
        .expect("probing must not error");

    assert_eq!(report.status, ProbeStatus::NotApplicable);
    assert!(report.capabilities.is_fail_closed());
    assert!(report.cases.is_empty(), "nothing should have been run");
    assert!(report.key.is_none(), "nothing to cache");
}

#[test]
fn the_registry_covers_every_column_of_the_capability_table() {
    let host = Host::detect();
    let config = common::config();

    let reports = probe_all(&host, &config).expect("probing must not error");

    let pairs: Vec<(String, String)> = reports
        .iter()
        .map(|report| (report.adapter.clone(), report.launcher.clone()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("fake".to_string(), "none".to_string()),
            ("codex".to_string(), "codex-sandbox".to_string()),
            ("claude".to_string(), "none".to_string()),
            ("claude".to_string(), "codex-sandbox".to_string()),
        ]
    );
    for report in &reports {
        if report.status != ProbeStatus::Measured {
            assert!(
                report.capabilities.is_fail_closed(),
                "{} x {} is not measured, so it must enforce nothing: {:?}",
                report.adapter,
                report.launcher,
                report.capabilities
            );
        }
    }
}
