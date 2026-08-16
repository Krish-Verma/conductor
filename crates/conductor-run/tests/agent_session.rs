//! The session an agent announces must be recorded — S10.
//!
//! §4.6's repair loop can resume an agent session, and
//! `repair::config::session_id_for` decides whether to. It reads
//! `attempt.agent_session_id`, which until S10 was written from exactly one
//! place: the session Conductor **assigned before the run**.
//!
//! That works for an adapter whose `conductor_assigned_session_id` is `true`,
//! and it is unreachable for one whose session identity arrives on the wire.
//! Codex is the second kind — §6.2: *"Session identity arrives in
//! `thread.started`, so it cannot be pre-assigned"* — so for Codex the column
//! was always `NULL`, `previous_session` always returned `None`, and the
//! `resume` support S10's scope names could never be reached through the
//! product.
//!
//! Nothing failed. The fake agent is the first kind, so every existing test
//! assigned its own session and got it back. That is exactly why this file
//! exists: the gap was invisible to a suite built around an adapter that does
//! not have the problem.

mod common;

use std::time::Duration;

use common::agent::{fake_agent_binary, warm_the_binary, write_scenario};
use common::vertical::{RUN, TASK, World};
use conductor_core::RunId;
use conductor_run::vertical::{VerticalConfig, run_task};

/// A scenario whose agent announces a session id Conductor did not choose —
/// the shape `thread.started` has.
///
/// `EmitRaw` rather than `Emit`: `Step::Emit` models only `kind` and `detail`,
/// so a `session_id` written there is silently dropped at deserialisation and
/// the fixture would prove nothing. Emitting the exact line is the only way to
/// reproduce what a real adapter puts on the wire.
const ANNOUNCES_A_SESSION: &str = r#"{
  "id": "announces-a-session",
  "description": "Emits agent.started carrying a session id the agent picked.",
  "steps": [
    {"step": "emit_raw", "line": "{\"kind\":\"agent.started\",\"session_id\":\"thread-01a007b9\"}"},
    {"step": "write_file", "path": "src/added.rs", "contents": "pub fn added() -> u32 { 1 }\n"},
    {"step": "report_on_stdout", "claim": "COMPLETE", "files_touched": ["src/added.rs"], "summary": "done"},
    {"step": "exit", "code": 0}
  ]
}"#;

fn config(world: &World) -> VerticalConfig {
    VerticalConfig {
        task_id: conductor_core::TaskId::new(TASK).expect("task id"),
        worker_id: "w-1".to_string(),
        source_repo: world.source.clone(),
        workspaces_root: world.workspaces(),
        artifacts_root: world.artifacts(),
        quarantine_root: world.quarantine(),
        profile_path: world.profile(),
        scratch_index: world.root().join("scratch").join("index"),
        supervisor: conductor_run::SupervisorConfig {
            startup_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            wall_timeout: Duration::from_secs(120),
            terminate_grace: Duration::from_millis(300),
            poll_interval: Duration::from_millis(10),
        },
        lease_ms: conductor_store::LEASE_MS,
        heartbeat_interval: Duration::from_millis(200),
        startup_grace: Duration::from_secs(30),
        sensitive: Default::default(),
        agent_env_extra: Default::default(),
        probe_key: conductor_run::containment::cache::ProbeKey::new(
            "fake", "test", "none", "n/a", "unprobed",
        ),
    }
}

/// `attempt.agent_session_id` as the database holds it.
fn recorded_session(world: &World) -> Option<String> {
    world
        .store()
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts")
        .last()
        .and_then(|attempt| attempt.agent_session_id.clone())
}

fn run(world: &World, scenario_json: &str) {
    warm_the_binary();
    let scenario = write_scenario(&world.root(), scenario_json);
    let adapter = conductor_agent::fake::FakeAgent::new(fake_agent_binary(), scenario)
        .with_max_lifetime_ms(20_000);
    let mut store = world.store();
    let _ = run_task(&mut store, &adapter, &config(world), &mut ());
}

#[test]
fn a_session_the_agent_announces_is_recorded_on_the_attempt() {
    // The claim. Conductor assigned nothing — `agent_session_id` is `None` in
    // the config — so the only way this column can hold the announced value is
    // if the run path read it off the event stream and wrote it down.
    let world = World::new();
    run(&world, ANNOUNCES_A_SESSION);

    assert_eq!(
        recorded_session(&world).as_deref(),
        Some("thread-01a007b9"),
        "the session the agent announced was not persisted, so §4.6's resume \
         can never find it — which makes `resume` unreachable for every adapter \
         whose identity arrives on the wire"
    );
}

#[test]
fn an_agent_that_announces_no_session_records_none() {
    // The other half. Without it, a build that wrote a constant into the column
    // would pass the test above. `NULL` must still mean "there is no session to
    // resume", because that is what `session_id_for` reads it as.
    let world = World::new();
    run(
        &world,
        r#"{
          "id": "announces-nothing",
          "description": "Never emits a session id.",
          "steps": [
            {"step": "write_file", "path": "src/added.rs", "contents": "pub fn added() -> u32 { 1 }\n"},
            {"step": "report_on_stdout", "claim": "COMPLETE", "files_touched": ["src/added.rs"], "summary": "done"},
            {"step": "exit", "code": 0}
          ]
        }"#,
    );

    assert_eq!(
        recorded_session(&world),
        None,
        "an attempt whose agent announced no session must record none, not a \
         placeholder that `session_id_for` would try to resume"
    );
}

#[test]
fn a_session_conductor_assigned_is_not_overwritten_by_silence() {
    // The regression this fix must not cause. An adapter of the *first* kind —
    // `conductor_assigned_session_id: true` — is handed its session before the
    // run and may never announce one. If recording the observed session cleared
    // the column when nothing was observed, every such adapter would lose the
    // resume it already had.
    let world = World::new();
    warm_the_binary();
    let scenario = write_scenario(
        &world.root(),
        r#"{
          "id": "assigned-not-announced",
          "description": "Conductor assigns the session; the agent stays quiet about it.",
          "steps": [
            {"step": "write_file", "path": "src/added.rs", "contents": "pub fn added() -> u32 { 1 }\n"},
            {"step": "report_on_stdout", "claim": "COMPLETE", "files_touched": ["src/added.rs"], "summary": "done"},
            {"step": "exit", "code": 0}
          ]
        }"#,
    );
    let adapter = conductor_agent::fake::FakeAgent::new(fake_agent_binary(), scenario)
        .with_max_lifetime_ms(20_000);
    let mut config = config(&world);
    let mut store = world.store();
    // Drive the worker directly: `run_task` starts a fresh session by design
    // (§4.6 owns the resume decision), so the assignment has to be made at the
    // layer that carries it.
    config.task_id = conductor_core::TaskId::new(TASK).expect("task id");
    let _ = conductor_run::vertical::run_task_with_session(
        &mut store,
        &adapter,
        &config,
        Some("assigned-by-conductor"),
        &mut (),
    );
    drop(store);

    assert_eq!(
        recorded_session(&world).as_deref(),
        Some("assigned-by-conductor"),
        "an assigned session was lost when the agent did not announce one"
    );
}

#[test]
fn an_announced_session_wins_over_the_one_conductor_assigned() {
    // The case that decides what the write must do, and the one a surviving
    // mutant exposed as untested.
    //
    // Conductor assigns `assigned-by-conductor`; the agent announces
    // `thread-announced`. Only one of those is a session that exists — the
    // assignment was a request, and the agent is what created the thing. A
    // resume aimed at the assigned id would name a session no agent ever had.
    let world = World::new();
    warm_the_binary();
    let scenario = write_scenario(&world.root(), ANNOUNCES_A_SESSION);
    let adapter = conductor_agent::fake::FakeAgent::new(fake_agent_binary(), scenario)
        .with_max_lifetime_ms(20_000);
    let mut store = world.store();
    let _ = conductor_run::vertical::run_task_with_session(
        &mut store,
        &adapter,
        &config(&world),
        Some("assigned-by-conductor"),
        &mut (),
    );
    drop(store);

    assert_eq!(
        recorded_session(&world).as_deref(),
        Some("thread-01a007b9"),
        "the session Conductor requested was recorded over the one the agent \
         actually created"
    );
}
