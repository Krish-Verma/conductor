//! The Codex adapter — master plan §6.2, slice S10.
//!
//! > Adapter parses recorded JSONL fixtures with **no process spawned**.
//!
//! Every fixture under `tests/fixtures/codex-jsonl/` is either a recording of a
//! real `codex exec --json` run (`success.jsonl`, `success-last-message.json`,
//! recorded against codex-cli 0.142.0 on 2026-08-15) or a hand-written stream
//! for a failure this slice must survive. **Nothing in this file starts a
//! process**, least of all Codex: the whole point of §6.1's pure-translation
//! adapter is that the least stable component in the system can be tested
//! against bytes.
//!
//! The four tests that matter most, because each encodes a trap that a
//! plausible adapter falls into:
//!
//! 1. [`the_report_is_the_last_agent_message_not_the_first_schema_shaped_one`] —
//!    `--output-schema` shapes *every* `agent_message`, not just the final one.
//! 2. [`absolute_paths_in_files_touched_are_made_workspace_relative`] — Codex
//!    reports absolute paths; reconciliation (§4.8) speaks workspace-relative.
//! 3. [`a_truncated_final_line_does_not_consume_the_lines_before_it`] — a killed
//!    agent leaves half a line, and the half-line must not erase the run.
//! 4. [`the_sandbox_is_workspace_write_never_danger_full_access`] — the sandbox
//!    is the only reason Codex was chosen first (§6.2, M6/M9/M10).

use std::path::PathBuf;

use conductor_agent::codex::{CodexAgent, REPORT_SCHEMA_JSON};
use conductor_agent::{AgentAdapter, AgentEvent, ResumeInput, RunOutputs, StartInput};
use conductor_core::{AttemptOutcome, ReportClaim, RunId, TaskId};

/// The workspace root every fixture was recorded against.
const WORKSPACE: &str = "/workspace";

fn adapter() -> CodexAgent {
    CodexAgent::new(
        PathBuf::from("/nonexistent/codex"),
        PathBuf::from(WORKSPACE),
        PathBuf::from("/artifacts/r-0041/1/report-schema.json"),
    )
    .with_prompt("Add a public `double` function to lib.rs and change nothing else.")
}

fn start_input() -> StartInput {
    StartInput {
        run_id: RunId::new("r-0041").expect("run id"),
        task_id: TaskId::new("T-0012").expect("task id"),
        attempt_ordinal: 1,
        workspace: PathBuf::from(WORKSPACE),
        report_path: PathBuf::from("/artifacts/r-0041/1/report.json"),
        session_id: None,
        env: Default::default(),
    }
}

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex-jsonl")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("fixture {}: {e}", path.display()))
}

fn fixture_lines(name: &str) -> Vec<String> {
    fixture(name).lines().map(str::to_string).collect()
}

/// The value that follows `flag` in an argv, if any.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

// ---------------------------------------------------------------------------
// command()
// ---------------------------------------------------------------------------

#[test]
fn command_builds_a_codex_exec_invocation_and_spawns_nothing() {
    let command = adapter().command(&start_input()).expect("command");

    assert_eq!(command.program, PathBuf::from("/nonexistent/codex"));
    assert_eq!(command.args.first().map(String::as_str), Some("exec"));
    assert!(command.args.iter().any(|a| a == "--json"));
    assert_eq!(command.cwd, PathBuf::from(WORKSPACE));
    // The program does not exist. If `command` spawned anything, this test
    // could not pass.
}

#[test]
fn the_sandbox_is_workspace_write_never_danger_full_access() {
    // §6.2: Codex is the first adapter *because* `--sandbox workspace-write` is
    // the only measured mechanism on this host that denies writes outside the
    // workspace (M6), denies egress (M9) and denies the control socket (M10).
    // An adapter that ships `danger-full-access` throws away the entire reason
    // this agent was chosen, and §4.9's prevented column with it.
    let command = adapter().command(&start_input()).expect("command");

    assert_eq!(
        arg_value(&command.args, "--sandbox"),
        Some("workspace-write")
    );
    assert!(
        !command.args.iter().any(|a| a == "danger-full-access"),
        "danger-full-access must never appear in an unattended argv: {:?}",
        command.args
    );
    assert!(!command.args.iter().any(|a| a == "read-only"));
}

#[test]
fn the_command_is_hermetic_against_ambient_user_configuration() {
    // §6.2 hermeticity. Without these, `~/.codex/config.toml` and any
    // AGENTS.md-style rules on the host silently change what the agent does,
    // and the run stops being a function of the plan.
    let command = adapter().command(&start_input()).expect("command");

    assert!(command.args.iter().any(|a| a == "--ignore-user-config"));
    assert!(command.args.iter().any(|a| a == "--ignore-rules"));
    // `--ephemeral` is deliberately absent: it would discard the session that
    // `resume` (S10 scope) needs.
    assert!(!command.args.iter().any(|a| a == "--ephemeral"));
}

#[test]
fn the_report_channel_is_two_files_the_adapter_never_opens() {
    // §6.2: `--output-schema <FILE>` makes the report contract enforced by the
    // agent runtime rather than validated afterwards; `--output-last-message
    // <FILE>` is where the final report lands. Both are paths the adapter is
    // handed, because an adapter that touches the filesystem is no longer pure
    // translation (§6.1).
    let command = adapter().command(&start_input()).expect("command");

    assert_eq!(
        arg_value(&command.args, "--output-schema"),
        Some("/artifacts/r-0041/1/report-schema.json")
    );
    assert_eq!(
        arg_value(&command.args, "--output-last-message"),
        Some("/artifacts/r-0041/1/report.json")
    );
    // Neither file exists. Building the command still succeeded.
}

#[test]
fn the_working_root_is_the_run_workspace_on_both_channels() {
    let command = adapter().command(&start_input()).expect("command");

    assert_eq!(command.cwd, PathBuf::from(WORKSPACE));
    assert_eq!(arg_value(&command.args, "--cd"), Some(WORKSPACE));
}

#[test]
fn the_prompt_is_the_final_argument_and_an_empty_one_is_refused() {
    // Finding from S10 measurement: `codex exec` with no prompt argument blocks
    // forever on "Reading additional input from stdin...". `supervise::spawn`
    // gives the child `Stdio::null()`, so an empty prompt would hang the attempt
    // until the wall-clock budget killed it. Refusing is cheaper and truthful.
    let command = adapter().command(&start_input()).expect("command");
    assert_eq!(
        command.args.last().map(String::as_str),
        Some("Add a public `double` function to lib.rs and change nothing else.")
    );

    let empty = CodexAgent::new(
        PathBuf::from("/nonexistent/codex"),
        PathBuf::from(WORKSPACE),
        PathBuf::from("/artifacts/schema.json"),
    );
    let err = empty
        .command(&start_input())
        .expect_err("an empty prompt must be refused");
    assert!(err.to_string().contains("stdin"), "{err}");
}

#[test]
fn the_environment_is_exactly_the_allowlist_the_caller_supplied() {
    // §4.9: the environment is an allowlist, not an addition. The Codex adapter
    // has no variables of its own — auth reaches it through whatever the caller
    // allowlisted — so anything extra here would be the adapter inventing
    // ambient state.
    let mut input = start_input();
    input.env.insert("PATH".to_string(), "/usr/bin".to_string());
    input
        .env
        .insert("HOME".to_string(), "/ws/r-0041/home".to_string());

    let command = adapter().command(&input).expect("command");

    assert_eq!(command.env, input.env);
    assert!(!command.env.contains_key("CARGO"));
    assert!(!command.env.contains_key("SSH_AUTH_SOCK"));
}

#[test]
fn a_start_input_for_another_workspace_is_refused_not_silently_accepted() {
    // The adapter holds the workspace root because `extract_report` — which has
    // no `StartInput` — must normalise absolute paths against it. Two sources of
    // one truth is a bug waiting to happen, so the disagreement is an error
    // rather than a coin flip about which root the report gets measured against.
    let mut input = start_input();
    input.workspace = PathBuf::from("/some/other/workspace");

    let err = adapter()
        .command(&input)
        .expect_err("a workspace disagreement must be refused");
    assert!(err.to_string().contains("workspace"), "{err}");
}

// ---------------------------------------------------------------------------
// parse_event()
// ---------------------------------------------------------------------------

#[test]
fn thread_started_carries_the_session_id_codex_assigned_itself() {
    // §6.2: "Session identity arrives in `thread.started`, so it cannot be
    // pre-assigned." There is no `session_id` field anywhere in the stream —
    // the id is `thread_id`, and an adapter looking for `session_id` finds
    // nothing and loses `resume`.
    let event = adapter()
        .parse_event(&fixture_lines("success.jsonl")[0])
        .expect("parse")
        .into_iter()
        .next()
        .expect("an event");

    match event {
        AgentEvent::Started { session_id, .. } => assert_eq!(
            session_id.as_deref(),
            Some("01a007b9-325f-78e0-9f17-d8b2561bf576")
        ),
        other => panic!("expected Started, got {other:?}"),
    }
}

#[test]
fn a_started_command_execution_item_becomes_one_command_run_event() {
    let adapter = adapter();
    let lines = fixture_lines("success.jsonl");

    let event = adapter
        .parse_event(&lines[3])
        .expect("parse")
        .into_iter()
        .next()
        .expect("event");
    match event {
        AgentEvent::CommandRun { command } => {
            assert_eq!(command, "/bin/zsh -lc 'git status --short'")
        }
        other => panic!("expected CommandRun, got {other:?}"),
    }

    // The matching `item.completed` says nothing new: the item was reported the
    // moment it started, which is the only report a killed run ever gets.
    assert_eq!(adapter.parse_event(&lines[4]).expect("parse"), Vec::new());
}

#[test]
fn a_file_change_item_becomes_a_write_normalised_to_the_workspace() {
    // Codex reports `/workspace/lib.rs`; §4.8 compares against `git status`,
    // which says `lib.rs`.
    let event = adapter()
        .parse_event(&fixture_lines("success.jsonl")[11])
        .expect("parse")
        .into_iter()
        .next()
        .expect("event");

    match event {
        AgentEvent::FileWritten { path } => assert_eq!(path, "lib.rs"),
        other => panic!("expected FileWritten, got {other:?}"),
    }
}

#[test]
fn a_deleted_file_change_is_a_deletion_not_a_write() {
    let line = r#"{"type":"item.started","item":{"id":"i","type":"file_change","changes":[{"path":"/workspace/src/gone.rs","kind":"delete"}],"status":"in_progress"}}"#;
    let event = adapter()
        .parse_event(line)
        .expect("parse")
        .into_iter()
        .next()
        .expect("event");

    match event {
        AgentEvent::FileDeleted { path } => assert_eq!(path, "src/gone.rs"),
        other => panic!("expected FileDeleted, got {other:?}"),
    }
}

#[test]
fn a_file_change_item_reports_every_change_it_carries() {
    // The test that justifies widening `parse_event` to `Vec<AgentEvent>` at
    // S10. Codex puts an **array** of changes in one `file_change` item, and one
    // edit touching several files is the ordinary case, not the exotic one.
    //
    // The previous signature returned at most one event, so this line reported
    // `a.rs` and silently dropped `b.rs` and `gone.rs`. Nothing failed: §4.8
    // reconciles against git, which sees all three — which is exactly why it
    // would never have been caught by a failing test, and exactly why S10 says
    // an adapter-shaped workaround is "a design smell to fix in the interface".
    let line = r#"{"type":"item.started","item":{"id":"i","type":"file_change","changes":[{"path":"/workspace/a.rs","kind":"update"},{"path":"/workspace/b.rs","kind":"add"},{"path":"/workspace/gone.rs","kind":"delete"}],"status":"in_progress"}}"#;
    let events = adapter().parse_event(line).expect("parse");

    assert_eq!(
        events,
        vec![
            AgentEvent::FileWritten {
                path: "a.rs".to_string()
            },
            AgentEvent::FileWritten {
                path: "b.rs".to_string()
            },
            AgentEvent::FileDeleted {
                path: "gone.rs".to_string()
            },
        ],
        "every change in the item must reach the event stream, in order, with \
         its kind preserved"
    );
}

#[test]
fn a_multi_change_edit_in_a_recorded_stream_reports_all_of_it() {
    // The same claim through a fixture file rather than a literal, so the
    // parsing path is the one a real stream takes.
    let adapter = adapter();
    let mut events = Vec::new();
    for line in fixture_lines("multi-file-change.jsonl") {
        events.extend(adapter.parse_event(&line).expect("parse"));
    }
    let written: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::FileWritten { .. } | AgentEvent::FileDeleted { .. }
            )
        })
        .collect();
    assert_eq!(
        written.len(),
        3,
        "a three-file edit reported {} change(s): {events:?}",
        written.len()
    );
}

#[test]
fn every_line_of_the_recorded_success_run_parses_without_error() {
    // The end-to-end shape of a real run: no line errors, the session id is
    // found, and the writes Codex claimed are workspace-relative.
    let adapter = adapter();
    let mut events = Vec::new();
    for line in fixture_lines("success.jsonl") {
        events.extend(
            adapter
                .parse_event(&line)
                .unwrap_or_else(|e| panic!("recorded line failed to parse: {e}\n{line}")),
        );
    }

    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Started {
            session_id: Some(_),
            ..
        }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::CommandRun { .. }))
            .count(),
        4,
        "the recorded run ran four shell commands"
    );
    let writes: Vec<_> = events.iter().filter(|e| e.claims_a_write()).collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0],
        &AgentEvent::FileWritten {
            path: "lib.rs".to_string()
        }
    );
}

#[test]
fn an_unknown_future_event_type_is_ignored_not_rejected() {
    // §2.2: never `deny_unknown_fields` on anything an agent produced. A Codex
    // upgrade that adds an event kind, an item kind or a field must not break a
    // run — and the known event in the same stream must still come through.
    let adapter = adapter();
    let mut known = 0usize;
    for line in fixture_lines("unknown-events.jsonl") {
        match adapter.parse_event(&line) {
            Ok(events) if !events.is_empty() => known += 1,
            Ok(_) => {}
            Err(e) => panic!("an unmodelled event must not be an error: {e}\n{line}"),
        }
    }
    // `thread.started` and the one `command_execution`; everything else in that
    // fixture is a kind this binary has never heard of.
    assert_eq!(known, 2);
}

#[test]
fn a_truncated_final_line_does_not_consume_the_lines_before_it() {
    // S10 failure injection: kill Codex mid-run. The stream ends mid-object.
    // The half-line is an error the supervisor records as a finding; every line
    // before it is still evidence and must survive.
    let adapter = adapter();
    let lines = fixture_lines("truncated.jsonl");

    let mut parsed = 0usize;
    let mut malformed = 0usize;
    for line in &lines {
        match adapter.parse_event(line) {
            Ok(events) if !events.is_empty() => parsed += 1,
            Ok(_) => {}
            Err(_) => malformed += 1,
        }
    }
    assert_eq!(malformed, 1, "exactly the truncated line is malformed");
    assert_eq!(parsed, 2, "thread.started and the started command survive");

    let err = adapter
        .parse_event(lines.last().expect("a last line"))
        .expect_err("the truncated line is an error");
    assert!(err.to_string().contains("could not be parsed"), "{err}");
}

#[test]
fn a_blank_line_is_empty_not_malformed() {
    assert_eq!(adapter().parse_event("").expect("blank"), Vec::new());
    assert_eq!(adapter().parse_event("   ").expect("blank"), Vec::new());
}

#[test]
fn an_error_event_naming_authentication_is_classified_as_infrastructure() {
    // §6.4: "auth/rate-limit error event → CRASHED, kind=infrastructure → infra
    // retry, no budget consumed". Charging an attempt budget for an expired
    // token would burn a task's whole allowance on a credential problem.
    let line = fixture_lines("auth-error.jsonl")
        .pop()
        .expect("the error line");
    let event = adapter()
        .parse_event(&line)
        .expect("parse")
        .into_iter()
        .next()
        .expect("event");

    match event {
        AgentEvent::Error {
            message,
            infrastructure,
        } => {
            assert!(
                infrastructure,
                "401 is infrastructure, not the task's fault"
            );
            assert!(message.contains("401"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn an_ordinary_error_is_not_flattered_into_an_infrastructure_one() {
    // The mirror of the rule above: calling everything infrastructure would make
    // every failure a free retry and defeat §4.6's bounded repair.
    let line = r#"{"type":"error","message":"the model produced an invalid patch and gave up"}"#;
    let event = adapter()
        .parse_event(line)
        .expect("parse")
        .into_iter()
        .next()
        .expect("event");

    match event {
        AgentEvent::Error { infrastructure, .. } => assert!(!infrastructure),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn a_failed_turn_is_an_error_however_it_nests_its_message() {
    // Only the success stream was recorded, so the failure shapes are handled
    // permissively: the message is looked for in more than one place rather
    // than assumed.
    let line =
        r#"{"type":"turn.failed","error":{"message":"usage limit reached; try again later"}}"#;
    let event = adapter()
        .parse_event(line)
        .expect("parse")
        .into_iter()
        .next()
        .expect("event");

    match event {
        AgentEvent::Error {
            message,
            infrastructure,
        } => {
            assert!(message.contains("usage limit"), "{message}");
            assert!(infrastructure, "a usage limit is a rate limit (§6.4)");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// extract_report()
// ---------------------------------------------------------------------------

#[test]
fn the_report_is_the_last_agent_message_not_the_first_schema_shaped_one() {
    // Measured at S10: `--output-schema` shapes **every** `agent_message`, not
    // only the final one. The recorded run emitted five; the first four claim
    // PARTIAL and only the last claims COMPLETE. An adapter that takes the first
    // schema-shaped message reports PARTIAL for a run that finished, and the
    // task is repaired forever.
    let adapter = adapter();
    let lines = fixture_lines("success.jsonl");

    let claims: Vec<ReportClaim> = lines
        .iter()
        .filter_map(|line| match adapter.parse_event(line) {
            Ok(events) => events.into_iter().find_map(|e| match e {
                AgentEvent::Report { report } => Some(report.claim),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(
        claims,
        vec![
            ReportClaim::Partial,
            ReportClaim::Partial,
            ReportClaim::Partial,
            ReportClaim::Partial,
            ReportClaim::Complete,
        ],
        "the recorded run really does claim PARTIAL four times before COMPLETE"
    );

    let report = adapter
        .extract_report(&RunOutputs {
            stdout_lines: lines,
            report_json: None,
        })
        .expect("extract")
        .into_iter()
        .next()
        .expect("a report");
    assert_eq!(report.claim, ReportClaim::Complete);
    assert!(report.summary.starts_with("Added"), "{}", report.summary);
}

#[test]
fn the_output_last_message_file_wins_over_the_stream() {
    // `-o/--output-last-message` is the channel `--output-schema` enforces. When
    // the two disagree, the enforced one is the report.
    let report = adapter()
        .extract_report(&RunOutputs {
            stdout_lines: fixture_lines("success.jsonl"),
            report_json: Some(
                r#"{"claim":"FAILED","files_touched":[],"summary":"the file wins"}"#.to_string(),
            ),
        })
        .expect("extract")
        .into_iter()
        .next()
        .expect("a report");

    assert_eq!(report.claim, ReportClaim::Failed);
    assert_eq!(report.summary, "the file wins");
}

#[test]
fn absolute_paths_in_files_touched_are_made_workspace_relative() {
    // Measured at S10: Codex reports absolute paths. §4.8 reconciles against
    // `git status`, which is workspace-relative. Without normalisation every
    // successful run reconciles as CONTRADICTED — a false "the agent lied" on
    // every single run.
    let report = adapter()
        .extract_report(&RunOutputs {
            stdout_lines: Vec::new(),
            report_json: Some(fixture("success-last-message.json")),
        })
        .expect("extract")
        .into_iter()
        .next()
        .expect("a report");

    assert_eq!(report.files_touched, vec!["lib.rs".to_string()]);
}

#[test]
fn a_path_genuinely_outside_the_workspace_is_left_alone_because_it_is_evidence() {
    // Rewriting an escape into something workspace-relative would delete the
    // one piece of evidence §4.9's detection column depends on. A sibling
    // directory sharing a prefix (`/workspace-other`) is not inside the
    // workspace either, however much string matching would like it to be.
    let report = adapter()
        .extract_report(&RunOutputs {
            stdout_lines: Vec::new(),
            report_json: Some(fixture("escaped-workspace-last-message.json")),
        })
        .expect("extract")
        .into_iter()
        .next()
        .expect("a report");

    assert_eq!(
        report.files_touched,
        vec![
            "lib.rs".to_string(),
            "src/deep/mod.rs".to_string(),
            "/workspace-other/lib.rs".to_string(),
            "/Users/kv/.ssh/config".to_string(),
            "already/relative.rs".to_string(),
        ]
    );
}

#[test]
fn a_schema_violating_report_is_an_error_that_names_the_report() {
    // S10 failure injection: schema violation. `--output-schema` is supposed to
    // prevent this; the adapter's job is to say the report could not be read
    // (row 5, `REPORT_UNPARSEABLE`) rather than guess. Reconciliation turns that
    // into CONTRADICTED.
    let err = adapter()
        .extract_report(&RunOutputs {
            stdout_lines: Vec::new(),
            report_json: Some(fixture("schema-violation-last-message.json")),
        })
        .expect_err("a schema violation is an error");

    assert!(err.to_string().to_lowercase().contains("report"), "{err}");
}

#[test]
fn no_report_at_all_is_a_normal_outcome() {
    // Row 4: exit 0 with no report. Reconciliation is authoritative; the report
    // is optional evidence.
    assert_eq!(
        adapter()
            .extract_report(&RunOutputs::default())
            .expect("extract"),
        None
    );
}

#[test]
fn a_report_with_extra_fields_still_parses() {
    let report = adapter()
        .extract_report(&RunOutputs {
            stdout_lines: Vec::new(),
            report_json: Some(
                r#"{"claim":"PARTIAL","files_touched":[],"summary":"","usage":{"output_tokens":9}}"#
                    .to_string(),
            ),
        })
        .expect("extract")
        .into_iter()
        .next()
        .expect("a report");

    assert_eq!(report.claim, ReportClaim::Partial);
}

#[test]
fn prose_agent_messages_are_not_mistaken_for_reports() {
    // Without `--output-schema` — or when the model answers in prose anyway —
    // an `agent_message` is not a report. Treating it as one would either error
    // a healthy run or invent a claim the agent never made.
    let adapter = adapter();
    let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"I have finished the task."}}"#;

    assert_eq!(adapter.parse_event(line).expect("parse"), Vec::new());
    assert_eq!(
        adapter
            .extract_report(&RunOutputs {
                stdout_lines: vec![line.to_string()],
                report_json: None,
            })
            .expect("extract"),
        None
    );
}

// ---------------------------------------------------------------------------
// The rest of the interface
// ---------------------------------------------------------------------------

#[test]
fn classify_exit_follows_section_6_4() {
    let adapter = adapter();
    assert_eq!(adapter.classify_exit(Some(0), None), AttemptOutcome::Exited);
    assert_eq!(
        adapter.classify_exit(Some(1), None),
        AttemptOutcome::Crashed
    );
    // M15: Codex propagates the exit code of what it ran.
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
    // Neither observed: unknown must not be recorded as known (§5.2).
    assert_eq!(adapter.classify_exit(None, None), AttemptOutcome::Stale);
}

#[test]
fn capabilities_describe_codex_0_142_and_do_not_flatter_it() {
    let caps = adapter().capabilities();

    assert!(caps.session_resume, "codex exec resume <SESSION_ID>");
    assert!(caps.schema_enforced_final_output, "--output-schema");
    assert!(caps.streaming_events, "--json");
    assert!(caps.hermetic_config, "--ignore-user-config/--ignore-rules");
    // §6.2: session identity arrives in `thread.started`. Conductor cannot pick
    // it, and claiming otherwise would let a later slice build a gate on an id
    // that does not exist until the agent speaks.
    assert!(!caps.conductor_assigned_session_id);
    // codex-cli 0.142.0 `exec --help` has no budget flag. Claude's
    // `--max-budget-usd` is Claude's (§6.3).
    assert!(!caps.spend_cap);
}

#[test]
fn resume_uses_the_thread_id_and_keeps_every_containment_flag() {
    // A resumed attempt that quietly dropped `--sandbox workspace-write` would
    // be an unsandboxed run wearing a sandboxed run's record.
    let input = ResumeInput {
        start: start_input(),
        session_id: "01a007b9-325f-78e0-9f17-d8b2561bf576".to_string(),
    };
    let command = adapter().resume_command(&input).expect("codex can resume");

    assert_eq!(
        command.args.iter().take(3).cloned().collect::<Vec<_>>(),
        vec![
            "exec".to_string(),
            "resume".to_string(),
            "01a007b9-325f-78e0-9f17-d8b2561bf576".to_string()
        ]
    );
    assert_eq!(
        arg_value(&command.args, "--sandbox"),
        Some("workspace-write")
    );
    assert!(command.args.iter().any(|a| a == "--ignore-user-config"));
    assert!(command.args.iter().any(|a| a == "--ignore-rules"));
    assert_eq!(
        arg_value(&command.args, "--output-last-message"),
        Some("/artifacts/r-0041/1/report.json")
    );
    assert_eq!(command.cwd, PathBuf::from(WORKSPACE));
}

#[test]
fn the_adapter_id_is_stable() {
    assert_eq!(adapter().id(), "codex");
}

#[test]
fn the_published_schema_is_the_report_conductor_actually_parses() {
    // `--output-schema` is only worth having if the schema names the report
    // Conductor deserialises. A schema that drifts from `AgentReport` makes the
    // runtime enforce the wrong contract, which is worse than enforcing none.
    let schema: serde_json::Value =
        serde_json::from_str(REPORT_SCHEMA_JSON).expect("the schema is JSON");

    let required = schema["required"].as_array().expect("required");
    for field in ["claim", "files_touched", "summary"] {
        assert!(
            required.iter().any(|v| v == field),
            "{field} is missing from the schema"
        );
    }
    let claims = schema["properties"]["claim"]["enum"]
        .as_array()
        .expect("the claim enum");
    assert_eq!(claims.len(), 3);
    for claim in [
        ReportClaim::Complete,
        ReportClaim::Partial,
        ReportClaim::Failed,
    ] {
        let encoded = serde_json::to_value(claim).expect("encode");
        assert!(
            claims.contains(&encoded),
            "the schema does not allow {encoded}"
        );
    }
    // Codex's structured output requires the object to be closed.
    assert_eq!(
        schema["additionalProperties"],
        serde_json::Value::Bool(false)
    );
}
