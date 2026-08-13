//! Turning case outcomes into an `Enforcement`, without running anything.
//!
//! These tests exist because of ADR-0002's methodology record. S0's first
//! containment round failed **twice in the permissive direction**: its "outside"
//! directory was inside the permitted region, and its AF_UNIX test failed on
//! `sun_path` length rather than on the sandbox. Both produced confident
//! statements about a boundary that had not been tested.
//!
//! So the classifier's contract is: **a case that cannot distinguish "denied"
//! from "broken" contributes nothing, and a dimension with no trustworthy basis
//! is not measured — which means `None`.**

use conductor_core::containment::{Enforcement, GatingDimension};
use conductor_run::containment::probe::{
    CaseId, CaseReport, CaseVerdict, ControlKind, ControlReport, Observation, classify_dimension,
};

fn blocked() -> Observation {
    Observation::Blocked {
        detail: "PermissionDenied".to_string(),
    }
}

fn control(observation: Observation) -> Option<ControlReport> {
    Some(ControlReport {
        kind: ControlKind::Unlaunched,
        observation,
    })
}

/// A case that was denied under the launcher and succeeded under its control.
fn denied(id: CaseId, target: &str) -> CaseReport {
    CaseReport::new(id, target, blocked(), control(Observation::Allowed))
}

/// A case that was permitted under the launcher.
fn allowed(id: CaseId, target: &str) -> CaseReport {
    CaseReport::new(
        id,
        target,
        Observation::Allowed,
        control(Observation::Allowed),
    )
}

fn fs_cases() -> Vec<CaseReport> {
    vec![
        allowed(CaseId::FsWriteInsideWorkspace, "/ws/probe.txt"),
        denied(CaseId::FsWriteOutsideSibling, "/outside/probe.txt"),
        denied(CaseId::FsWriteHomeRoot, "/home/.probe"),
        denied(CaseId::FsWriteUsersShared, "/Users/Shared/probe"),
        denied(CaseId::FsWriteOutsideNestedShell, "/outside/nested.txt"),
    ]
}

#[test]
fn a_denial_requires_a_control_that_succeeded() {
    let case = CaseReport::new(
        CaseId::FsWriteOutsideSibling,
        "/outside/probe.txt",
        blocked(),
        control(Observation::Allowed),
    );
    assert_eq!(case.verdict, CaseVerdict::Denied);
}

#[test]
fn a_blocked_case_whose_control_also_failed_is_broken_never_denied() {
    // This is exactly S0 round 1's AF_UNIX result: the operation failed, and the
    // reason had nothing to do with the sandbox.
    let case = CaseReport::new(
        CaseId::UnixSocketConnect,
        "/probe/c.sock",
        blocked(),
        control(Observation::Broken {
            reason: "listener never bound".to_string(),
        }),
    );
    assert!(
        matches!(case.verdict, CaseVerdict::Broken { .. }),
        "got {:?}",
        case.verdict
    );

    let report = classify_dimension(GatingDimension::ControlSurface, &[case]);
    assert!(!report.measured, "a broken case is not a measurement");
    assert_eq!(
        report.enforcement,
        Enforcement::None,
        "an unmeasured dimension fails closed"
    );
}

#[test]
fn every_outside_write_denied_is_hard() {
    let report = classify_dimension(GatingDimension::FilesystemWrite, &fs_cases());

    assert!(report.measured);
    assert_eq!(report.enforcement, Enforcement::Hard);
    assert!(report.exceptions.is_empty());
}

#[test]
fn a_permitted_region_makes_it_restricted_and_is_enumerated() {
    let mut cases = fs_cases();
    cases.push(allowed(CaseId::FsWriteTmp, "/tmp/probe"));
    cases.push(allowed(CaseId::FsWriteTmpdir, "/var/folders/x/T/probe"));

    let report = classify_dimension(GatingDimension::FilesystemWrite, &cases);

    assert!(report.measured);
    assert_eq!(report.enforcement, Enforcement::Restricted);
    assert_eq!(
        report.exceptions,
        vec![
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/var/folders/x/T")
        ],
        "Restricted is only meaningful with the exception set enumerated (§4.2), and the \
         exception is the writable region — not the probe's own file inside it, which a \
         policy could never match against"
    );
}

#[test]
fn two_permitted_writes_in_one_region_are_one_exception() {
    let mut cases = fs_cases();
    cases.push(allowed(CaseId::FsWriteTmp, "/tmp/probe-a"));
    cases.push(allowed(CaseId::FsWriteTmpdir, "/tmp/probe-b"));

    let report = classify_dimension(GatingDimension::FilesystemWrite, &cases);

    assert_eq!(report.exceptions, vec![std::path::PathBuf::from("/tmp")]);
}

#[test]
fn nothing_denied_is_none() {
    let cases = vec![
        allowed(CaseId::FsWriteInsideWorkspace, "/ws/probe.txt"),
        allowed(CaseId::FsWriteOutsideSibling, "/outside/probe.txt"),
        allowed(CaseId::FsWriteHomeRoot, "/home/.probe"),
        allowed(CaseId::FsWriteOutsideNestedShell, "/outside/nested.txt"),
    ];

    let report = classify_dimension(GatingDimension::FilesystemWrite, &cases);

    assert!(
        report.measured,
        "observing that nothing is enforced is a measurement"
    );
    assert_eq!(report.enforcement, Enforcement::None);
}

#[test]
fn a_write_inside_the_workspace_that_fails_means_the_probe_is_broken() {
    // The positive control for the whole filesystem dimension. If the agent
    // cannot write where it is supposed to be able to write, every "denial"
    // below is meaningless.
    let mut cases = fs_cases();
    cases[0] = CaseReport::new(
        CaseId::FsWriteInsideWorkspace,
        "/ws/probe.txt",
        blocked(),
        control(Observation::Allowed),
    );

    let report = classify_dimension(GatingDimension::FilesystemWrite, &cases);

    assert!(!report.measured);
    assert_eq!(report.enforcement, Enforcement::None);
    assert!(
        report
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("inside the workspace"),
        "the reason must name the failed control: {:?}",
        report.reason
    );
}

#[test]
fn a_child_process_that_escapes_collapses_the_dimension_to_none() {
    // M8 measured that the restriction is inherited. If it stops being
    // inherited, the direct denials are worthless — and calling the nested path
    // an "exception" would be a lie, because every path is then reachable.
    let mut cases = fs_cases();
    cases[4] = allowed(CaseId::FsWriteOutsideNestedShell, "/outside/nested.txt");

    let report = classify_dimension(GatingDimension::FilesystemWrite, &cases);

    assert_eq!(report.enforcement, Enforcement::None);
    assert!(!report.measured);
    assert!(
        report
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("child"),
        "the reason must name child-process inheritance: {:?}",
        report.reason
    );
}

#[test]
fn a_single_informative_case_is_too_thin_a_basis_for_filesystem_write() {
    // Only the sibling case survived; everything else was broken. One data point
    // about one path is not "writes outside the workspace are denied".
    let cases = vec![
        allowed(CaseId::FsWriteInsideWorkspace, "/ws/probe.txt"),
        denied(CaseId::FsWriteOutsideSibling, "/outside/probe.txt"),
        CaseReport::new(
            CaseId::FsWriteHomeRoot,
            "/home/.probe",
            blocked(),
            control(Observation::Broken {
                reason: "control could not write either".to_string(),
            }),
        ),
    ];

    let report = classify_dimension(GatingDimension::FilesystemWrite, &cases);

    assert!(!report.measured, "got {report:?}");
    assert_eq!(report.enforcement, Enforcement::None);
}

#[test]
fn losing_the_sibling_case_is_fatal_even_with_other_denials() {
    // The sibling directory is the canonical "outside the workspace, not in a
    // permitted region" location. Round 1 got exactly this wrong.
    let cases = vec![
        allowed(CaseId::FsWriteInsideWorkspace, "/ws/probe.txt"),
        denied(CaseId::FsWriteHomeRoot, "/home/.probe"),
        denied(CaseId::FsWriteUsersShared, "/Users/Shared/probe"),
        denied(CaseId::FsWriteOutsideNestedShell, "/outside/nested.txt"),
    ];

    let report = classify_dimension(GatingDimension::FilesystemWrite, &cases);

    assert!(!report.measured, "got {report:?}");
    assert_eq!(report.enforcement, Enforcement::None);
}

#[test]
fn network_and_control_surface_are_hard_only_when_every_case_was_denied() {
    for dimension in [
        GatingDimension::NetworkEgress,
        GatingDimension::ControlSurface,
        GatingDimension::CredentialRead,
    ] {
        let ids: Vec<CaseId> = match dimension {
            GatingDimension::NetworkEgress => vec![CaseId::NetTcpConnect, CaseId::NetDnsResolve],
            GatingDimension::ControlSurface => vec![CaseId::UnixSocketConnect],
            _ => vec![CaseId::ReadPlantedSecret],
        };

        let all_denied: Vec<CaseReport> = ids.iter().map(|id| denied(*id, "target")).collect();
        let report = classify_dimension(dimension, &all_denied);
        assert!(report.measured, "{dimension}: {report:?}");
        assert_eq!(report.enforcement, Enforcement::Hard, "{dimension}");

        let any_allowed: Vec<CaseReport> = ids.iter().map(|id| allowed(*id, "target")).collect();
        let report = classify_dimension(dimension, &any_allowed);
        assert!(report.measured, "{dimension}: {report:?}");
        assert_eq!(report.enforcement, Enforcement::None, "{dimension}");
    }
}

#[test]
fn a_partly_denied_network_fails_closed_to_none() {
    // TCP denied but DNS permitted is not "Hard". There is no exception
    // vocabulary for network in §4.2's model, so the weaker verdict wins.
    let cases = vec![
        denied(CaseId::NetTcpConnect, "1.1.1.1:443"),
        allowed(CaseId::NetDnsResolve, "example.com"),
    ];

    let report = classify_dimension(GatingDimension::NetworkEgress, &cases);

    assert_eq!(report.enforcement, Enforcement::None);
}

#[test]
fn a_dimension_with_no_cases_at_all_is_not_measured() {
    for dimension in GatingDimension::ALL {
        let report = classify_dimension(*dimension, &[]);
        assert!(!report.measured, "{dimension}");
        assert_eq!(report.enforcement, Enforcement::None, "{dimension}");
    }
}
