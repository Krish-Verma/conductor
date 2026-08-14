//! The `AgentAdapter` contract — master plan §6.1.
//!
//! > Conductor owns spawning, killing, timeouts and streaming. Adapters are
//! > **pure translation** — build argv+env, parse lines, classify exits. This
//! > makes every adapter testable against recorded JSONL fixtures with no
//! > process at all, which is what you want because agent output is the least
//! > stable thing in the system.
//!
//! **Not one test in this file starts a process.** That is the property being
//! asserted: if any of it needed a subprocess, the adapter would be doing
//! something §6.1 says Conductor does.

use std::path::PathBuf;

use conductor_agent::fake::FakeAgent;
use conductor_agent::{AgentAdapter, AgentEvent, RunOutputs, StartInput};
use conductor_core::{AttemptOutcome, ReportClaim, RunId, TaskId};

fn adapter() -> FakeAgent {
    FakeAgent::new(
        PathBuf::from("/nonexistent/conductor-fake-agent"),
        PathBuf::from("/nonexistent/scenario.json"),
    )
}

fn start_input() -> StartInput {
    StartInput {
        run_id: RunId::new("r-0041").expect("run id"),
        task_id: TaskId::new("T-0012").expect("task id"),
        attempt_ordinal: 1,
        workspace: PathBuf::from("/ws/r-0041"),
        report_path: PathBuf::from("/artifacts/r-0041/1/report.json"),
        session_id: None,
        env: Default::default(),
    }
}

#[test]
fn command_builds_argv_and_does_not_spawn() {
    let command = adapter().command(&start_input()).expect("command");

    assert_eq!(
        command.program,
        PathBuf::from("/nonexistent/conductor-fake-agent")
    );
    assert!(command.args.contains(&"--scenario".to_string()));
    assert!(command.args.contains(&"--report".to_string()));
    assert_eq!(command.cwd, PathBuf::from("/ws/r-0041"));
    // The program does not exist. If `command` spawned anything, this test
    // could not pass.
}

#[test]
fn the_environment_is_an_allowlist_not_an_addition() {
    // §4.9: "An **allowlisted** environment (`PATH`, redirected `HOME`, `LANG`,
    // `TERM`, the adapter's own auth variable, nothing else). Not a denylist —
    // a denylist misses the next variable name."
    let mut input = start_input();
    input.env.insert("PATH".to_string(), "/usr/bin".to_string());
    input
        .env
        .insert("HOME".to_string(), "/ws/r-0041/home".to_string());

    let command = adapter().command(&input).expect("command");

    assert_eq!(
        command.env.get("PATH").map(String::as_str),
        Some("/usr/bin")
    );
    assert_eq!(
        command.env.get("HOME").map(String::as_str),
        Some("/ws/r-0041/home")
    );
    // Nothing crept in that the caller did not put there, except the adapter's
    // own variables, which are named.
    for key in command.env.keys() {
        assert!(
            key == "PATH" || key == "HOME" || key.starts_with("CONDUCTOR_"),
            "unexpected variable {key} in the agent environment"
        );
    }
    // And the caller's allowlist is what decides: nothing from this test
    // process's own environment is inherited.
    assert!(!command.env.contains_key("CARGO"));
    assert!(!command.env.contains_key("SSH_AUTH_SOCK"));
}

#[test]
fn parse_event_reads_recorded_jsonl_with_no_process() {
    let adapter = adapter();

    let started = adapter
        .parse_event(r#"{"kind":"agent.started","scenario":"success"}"#)
        .expect("parse")
        .expect("an event");
    assert!(matches!(started, AgentEvent::Started { .. }));

    let file = adapter
        .parse_event(r#"{"kind":"file.written","path":"src/a.rs"}"#)
        .expect("parse")
        .expect("an event");
    match file {
        AgentEvent::FileWritten { path } => assert_eq!(path, "src/a.rs"),
        other => panic!("expected FileWritten, got {other:?}"),
    }

    let checkpoint = adapter
        .parse_event(r#"{"kind":"checkpoint","name":"after-edits"}"#)
        .expect("parse")
        .expect("an event");
    match checkpoint {
        AgentEvent::Checkpoint { name } => assert_eq!(name, "after-edits"),
        other => panic!("expected Checkpoint, got {other:?}"),
    }
}

#[test]
fn an_unknown_field_never_makes_a_line_unparseable() {
    // §2.2: "permissive serde on all agent input … **never** `deny_unknown_fields`
    // on anything an agent produced." Agent CLIs add fields between patch
    // releases; refusing the line would turn a cosmetic upstream change into a
    // failed run.
    let event = adapter()
        .parse_event(
            r#"{"kind":"file.written","path":"src/a.rs","tokens":42,"nested":{"x":[1,2]}}"#,
        )
        .expect("an unknown field must not be an error")
        .expect("an event");
    assert!(matches!(event, AgentEvent::FileWritten { .. }));
}

#[test]
fn an_unknown_event_kind_is_ignored_not_an_error() {
    // A kind Conductor has never heard of is the same situation as an unknown
    // field: newer agent, older Conductor.
    assert_eq!(
        adapter()
            .parse_event(r#"{"kind":"thread.reasoning.delta","text":"…"}"#)
            .expect("must not error"),
        None
    );
}

#[test]
fn a_malformed_line_is_reported_but_does_not_stop_the_stream() {
    // Malformed JSONL is one of the injected failures. The adapter reports it as
    // an error for the supervisor to record as a finding; it is the supervisor's
    // job to keep reading, and `tests/scenarios.rs` proves it does.
    let err = adapter()
        .parse_event("{not json at all")
        .expect_err("a malformed line is an error");
    assert!(err.to_string().contains("could not be parsed"));

    // A blank line is not malformed, just empty.
    assert_eq!(adapter().parse_event("").expect("blank"), None);
    assert_eq!(adapter().parse_event("   ").expect("blank"), None);
}

#[test]
fn extract_report_prefers_the_report_file_and_tolerates_its_absence() {
    let adapter = adapter();

    let outputs = RunOutputs {
        stdout_lines: Vec::new(),
        report_json: Some(
            r#"{"claim":"COMPLETE","files_touched":["src/a.rs"],"summary":"did the thing"}"#
                .to_string(),
        ),
    };
    let report = adapter
        .extract_report(&outputs)
        .expect("extract")
        .expect("a report");
    assert_eq!(report.claim, ReportClaim::Complete);
    assert_eq!(report.files_touched, vec!["src/a.rs".to_string()]);

    // Row 4: exit 0 with no report is not an error — reconciliation is
    // authoritative and the report is optional.
    let empty = RunOutputs {
        stdout_lines: Vec::new(),
        report_json: None,
    };
    assert_eq!(adapter.extract_report(&empty).expect("extract"), None);
}

#[test]
fn a_malformed_report_is_an_error_and_names_itself() {
    // Row 5: finding `REPORT_UNPARSEABLE`. The adapter's job is to say the
    // report could not be read; deciding what that means is reconciliation's.
    let outputs = RunOutputs {
        stdout_lines: Vec::new(),
        report_json: Some("{\"claim\": ".to_string()),
    };
    let err = adapter()
        .extract_report(&outputs)
        .expect_err("a malformed report is an error");
    assert!(err.to_string().to_lowercase().contains("report"));
}

#[test]
fn a_report_with_extra_fields_still_parses() {
    let outputs = RunOutputs {
        stdout_lines: Vec::new(),
        report_json: Some(
            r#"{"claim":"PARTIAL","files_touched":[],"summary":"","cost_usd":0.12,"model":"x"}"#
                .to_string(),
        ),
    };
    let report = adapter()
        .extract_report(&outputs)
        .expect("extract")
        .expect("a report");
    assert_eq!(report.claim, ReportClaim::Partial);
}

#[test]
fn a_report_delivered_on_stdout_is_found_when_there_is_no_file() {
    // Both real adapters have two report channels (a final message and a file).
    // The fake has both so that the two paths are exercised from S3 rather than
    // discovered at S10.
    let outputs = RunOutputs {
        stdout_lines: vec![
            r#"{"kind":"file.written","path":"a"}"#.to_string(),
            r#"{"kind":"agent.report","report":{"claim":"COMPLETE","files_touched":["a"],"summary":"s"}}"#
                .to_string(),
        ],
        report_json: None,
    };
    let report = adapter()
        .extract_report(&outputs)
        .expect("extract")
        .expect("a report");
    assert_eq!(report.claim, ReportClaim::Complete);
}

#[test]
fn classify_exit_follows_section_6_4() {
    let adapter = adapter();
    assert_eq!(adapter.classify_exit(Some(0), None), AttemptOutcome::Exited);
    assert_eq!(
        adapter.classify_exit(Some(1), None),
        AttemptOutcome::Crashed
    );
    assert_eq!(
        adapter.classify_exit(Some(42), None),
        AttemptOutcome::Crashed
    );
    assert_eq!(
        adapter.classify_exit(None, Some(9)),
        AttemptOutcome::Crashed
    );
    assert_eq!(
        adapter.classify_exit(None, Some(11)),
        AttemptOutcome::Crashed
    );
    // Neither a code nor a signal: nothing was observed, and unknown must not be
    // recorded as known.
    assert_eq!(adapter.classify_exit(None, None), AttemptOutcome::Stale);
}

#[test]
fn capabilities_are_functional_only() {
    // §6.1: security capabilities live in ExecutionCapabilities (§4.2);
    // conflating them hides the distinction that matters.
    let caps = adapter().capabilities();
    assert!(caps.streaming_events);
    assert!(caps.schema_enforced_final_output);
    assert!(caps.conductor_assigned_session_id);
    assert!(caps.hermetic_config);
    assert!(!caps.spend_cap);
}

#[test]
fn resume_command_is_optional_and_the_fake_declines() {
    // §6.1 returns Option: an adapter that cannot resume says so rather than
    // producing a command that silently starts a fresh session.
    let input = conductor_agent::ResumeInput {
        start: start_input(),
        session_id: "s-1".to_string(),
    };
    assert!(adapter().resume_command(&input).is_none());
}

#[test]
fn the_adapter_id_is_stable() {
    assert_eq!(adapter().id(), "fake");
}
