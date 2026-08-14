//! The fake agent's scenario catalogue.
//!
//! The S3 slice names fifteen scenarios. They are data, not code, because the
//! S10 risk note says the fake agent stays "the primary CI harness **forever**"
//! — a harness whose behaviours are compiled in can only be extended by
//! recompiling Conductor, and every later slice needs to add cases.
//!
//! This file asserts the catalogue is complete and that every scenario in it
//! parses. Whether each one *behaves* as named is asserted in `conductor-run`,
//! where there is a supervisor to run them under.

use conductor_agent::scenario::{Scenario, Step, catalogue, scenario_by_id};

#[test]
fn every_scenario_the_slice_names_is_present() {
    let required = [
        "success",
        "success-with-report",
        "partial-edits",
        "crash-before-edits",
        "crash-after-edits",
        "stall",
        "timeout",
        "malformed-report",
        "missing-report",
        "verification-failure",
        "same-failure-repeatedly",
        "unexpected-dependency-change",
        "forbidden-git-change",
        "duplicate-attempt",
        "control-socket",
    ];
    let catalogue = catalogue();
    let have: Vec<&str> = catalogue.iter().map(|s| s.id.as_str()).collect();
    for id in required {
        assert!(
            have.contains(&id),
            "scenario {id} is missing; have {have:?}"
        );
    }
}

#[test]
fn every_catalogued_scenario_round_trips_through_json() {
    for scenario in catalogue() {
        let json = serde_json::to_string(&scenario).expect("serialize");
        let back: Scenario = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, scenario.id);
        assert_eq!(back.steps.len(), scenario.steps.len());
    }
}

#[test]
fn an_unknown_field_in_a_scenario_file_is_tolerated() {
    // A scenario file is written by a human or a later slice, and the fake agent
    // must not become the reason a test cannot be extended.
    let json = r#"{
        "id": "hand-written",
        "description": "…",
        "note": "a field this binary has never heard of",
        "steps": [{"step":"exit","code":0,"comment":"also unknown"}]
    }"#;
    let scenario: Scenario = serde_json::from_str(json).expect("must tolerate unknown fields");
    assert_eq!(scenario.id, "hand-written");
    assert_eq!(scenario.steps.len(), 1);
}

#[test]
fn scenarios_that_must_end_in_a_crash_actually_do() {
    for id in ["crash-before-edits", "crash-after-edits"] {
        let scenario = scenario_by_id(id).expect("scenario");
        let last = scenario.steps.last().expect("at least one step");
        assert!(
            matches!(last, Step::KillSelf { .. } | Step::Abort),
            "{id} must end by dying, not by exiting: {last:?}"
        );
    }
}

#[test]
fn the_crash_after_edits_scenario_writes_before_it_dies() {
    // Otherwise it is the crash-*before*-edits scenario wearing a different
    // name, and acceptance rows 2 and 3 would be testing the same thing.
    let scenario = scenario_by_id("crash-after-edits").expect("scenario");
    let write_index = scenario
        .steps
        .iter()
        .position(|s| matches!(s, Step::WriteFile { .. }))
        .expect("crash-after-edits must write a file");
    let die_index = scenario
        .steps
        .iter()
        .position(|s| matches!(s, Step::KillSelf { .. } | Step::Abort))
        .expect("crash-after-edits must die");
    assert!(write_index < die_index);

    let before = scenario_by_id("crash-before-edits").expect("scenario");
    assert!(
        !before
            .steps
            .iter()
            .any(|s| matches!(s, Step::WriteFile { .. })),
        "crash-before-edits must not touch the tree"
    );
}

#[test]
fn the_stall_and_timeout_scenarios_fail_in_different_ways() {
    // A stall is silence; a timeout is work that never ends. Conductor
    // distinguishes them (§6.4: `reason=stall` versus the wall-clock budget), so
    // the fixtures must too — otherwise only one of the two timers is tested.
    let stall = scenario_by_id("stall").expect("scenario");
    assert!(stall.steps.iter().any(|s| matches!(s, Step::Stall)));
    assert!(
        !stall.steps.iter().any(|s| matches!(s, Step::Spin { .. })),
        "a stalling agent must emit nothing, or it is not stalling"
    );

    let timeout = scenario_by_id("timeout").expect("scenario");
    assert!(timeout.steps.iter().any(|s| matches!(s, Step::Spin { .. })));
    assert!(
        !timeout.steps.iter().any(|s| matches!(s, Step::Stall)),
        "the wall-clock scenario must keep talking, or it would trip the idle timer first"
    );
}

#[test]
fn the_repeated_failure_scenario_is_deterministic() {
    // Row 9 depends on two attempts producing the *same* fingerprint. A
    // scenario with a timestamp, a random value or a pid in its output could
    // not.
    let scenario = scenario_by_id("same-failure-repeatedly").expect("scenario");
    let rendered = serde_json::to_string(&scenario).expect("serialize");
    for forbidden in ["$RANDOM", "{{pid}}", "{{now}}", "{{uuid}}"] {
        assert!(
            !rendered.contains(forbidden),
            "the repeated-failure scenario must not vary between attempts"
        );
    }
}

#[test]
fn the_forbidden_git_scenario_touches_repository_structure_not_files() {
    // Acceptance row 14 sets a remote inside the clone, which leaves the tree
    // identical to baseline. A scenario that also edited a file would let the
    // test pass on the file change and never exercise the config diff.
    let scenario = scenario_by_id("forbidden-git-change").expect("scenario");
    assert!(scenario.steps.iter().any(|s| matches!(s, Step::Git { .. })));
    assert!(
        !scenario
            .steps
            .iter()
            .any(|s| matches!(s, Step::WriteFile { .. })),
        "row 14 is about a tree-identical change; writing a file would mask it"
    );
}

#[test]
fn the_control_socket_scenario_names_the_socket_by_environment() {
    // Acceptance row 28. The path comes from the environment because the test
    // creates the listener; a hardcoded path would make the scenario depend on
    // a socket nobody in the test controls.
    let scenario = scenario_by_id("control-socket").expect("scenario");
    assert!(
        scenario
            .steps
            .iter()
            .any(|s| matches!(s, Step::ConnectUnix { .. })),
        "the scenario must actually attempt the connection"
    );
}

#[test]
fn scenario_ids_are_unique() {
    let mut ids: Vec<String> = catalogue().iter().map(|s| s.id.clone()).collect();
    let before = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), before, "two scenarios share an id");
}
