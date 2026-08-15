//! S9 — acceptance row 30, wired to a real launch.
//!
//! | # | Scenario | Injected | Expected persisted state | Automatic behaviour | Final |
//! |---|---|---|---|---|---|
//! | 30 | Ineligible execution mode | sensitive task, caps below requirement | attempt never starts | refuse with dimension named | `BLOCKED` |
//!
//! S7 built `eligibility::check` as a pure function with full unit coverage and
//! a positive control, and the master plan was explicit that this was **not**
//! enough:
//!
//! > Until S9 does so, **row 30 must be scored `NOT RUN`, not `PASS`** — the
//! > decision is proven, the refusal is not yet reachable from a real launch.
//!
//! So every test in this file goes through `vertical::run_task`, the same entry
//! point production uses. None of them calls `eligibility::check` directly; a
//! test that did would be re-proving S7 and would stay green if the call site
//! were deleted tomorrow — which is precisely the state this file exists to end.
//!
//! # Both halves, always
//!
//! [`a_task_whose_requirement_the_host_cannot_meet_never_launches`] is paired
//! with [`the_same_task_launches_once_the_host_measures_enough`], differing in
//! one seeded probe row. A refusal test alone would pass against a build that
//! refuses everything, which is the failure mode S8 found twice in its own
//! approval tests.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::agent::{fake_agent_binary, scenario_file, warm_the_binary};
use common::vertical::{RUN, TASK, World};
use conductor_core::containment::{Enforcement, ExecutionCapabilities};
use conductor_core::{RunId, RunState, TaskId, TaskState};
use conductor_run::containment::cache::{ProbeKey, upsert};
use conductor_run::vertical::{VerticalConfig, VerticalOutcome, run_task};

/// The probe key every test in this file uses. Fixed strings: what varies is
/// whether a row exists for it, not what it is called.
fn probe_key() -> ProbeKey {
    ProbeKey::new("fake", "1.0.0", "none", "n/a", "test-os-1")
}

/// A measured vector in which every gating dimension is enforced in kernel.
///
/// Deliberately **not** what §4.2's table records for `FakeAgent` — that column
/// is `n/a`, and the master plan says recording FakeAgent as `Hard` "would be a
/// category error". Nothing here claims the fake agent is contained; the row is
/// a *measurement fixture* standing in for a host that has been probed, so that
/// the positive control can show the gate proceeding. The subject under test is
/// the gate, not the agent.
fn measured_hard() -> ExecutionCapabilities {
    let mut caps = ExecutionCapabilities::fail_closed();
    caps.filesystem_write = Enforcement::Hard;
    caps.network_egress = Enforcement::Hard;
    caps.control_surface = Enforcement::Hard;
    caps.credential_read = Enforcement::Hard;
    caps
}

fn config(world: &World) -> VerticalConfig {
    VerticalConfig {
        task_id: TaskId::new(TASK).expect("task id"),
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
        agent_env_extra: BTreeMap::new(),
        probe_key: probe_key(),
    }
}

/// The catalogued `success` scenario, as a real fake-agent adapter.
fn success_adapter(world: &World) -> conductor_agent::fake::FakeAgent {
    warm_the_binary();
    let scenario = scenario_file(&world.root(), "success");
    conductor_agent::fake::FakeAgent::new(fake_agent_binary(), scenario)
        .with_max_lifetime_ms(20_000)
}

/// Declare §4.2's requirement on the task, durably.
fn require(world: &World, yaml: &str) {
    let mut store = world.store();
    store
        .set_execution_requirements(&TaskId::new(TASK).expect("id"), Some(yaml))
        .expect("set requirements");
}

/// Seed a measurement for this host.
fn measure(world: &World, caps: &ExecutionCapabilities) {
    let mut store = world.store();
    upsert(store.conn_mut(), &probe_key(), caps, 1_000).expect("upsert probe");
}

fn attempts(world: &World) -> usize {
    world
        .store()
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts")
        .len()
}

#[test]
fn a_task_whose_requirement_the_host_cannot_meet_never_launches() {
    let world = World::new();
    require(&world, "execution_requirements:\n  control_surface: hard\n");
    // No probe row at all. §4.2: "A stale or absent probe forces every
    // dimension to `None` — fail closed."

    let mut store = world.store();
    let outcome = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    );

    // The launch is refused, not attempted-and-failed.
    match outcome {
        Ok(vertical) => panic!(
            "an ineligible task launched and produced {:?}",
            vertical.outcome
        ),
        Err(error) => {
            let text = error.to_string();
            // Row 30: "refuse with dimension named".
            assert!(
                text.contains("control_surface"),
                "the refusal does not name the dimension: {text}"
            );
        }
    }

    // "attempt never starts" — the strongest form: no attempt row, no
    // workspace on disk, no agent process ever built.
    assert_eq!(attempts(&world), 0, "an attempt row was created");
    assert!(
        !world.workspace().exists(),
        "a workspace was cloned for a run that was refused"
    );

    // Final: BLOCKED, for both the run and the task it mirrors.
    assert_eq!(world.run_state(), RunState::Blocked);
    assert_eq!(world.task_state(), TaskState::Blocked);

    // A durable, unresolved finding that names the dimension and the values.
    let findings = world
        .store()
        .findings_for_run(&RunId::new(RUN).expect("id"))
        .expect("findings");
    let refusal = findings
        .iter()
        .find(|f| f.kind == "INELIGIBLE_EXECUTION_MODE")
        .expect("a refusal finding");
    assert_eq!(refusal.severity, "CRITICAL");
    assert!(refusal.resolution.is_none(), "findings never auto-resolve");
    assert!(refusal.evidence_ref.contains("control_surface"));
    assert!(
        refusal.evidence_ref.to_ascii_lowercase().contains("hard"),
        "the requirement is not named: {}",
        refusal.evidence_ref
    );
}

#[test]
fn the_same_task_launches_once_the_host_measures_enough() {
    // The positive control for the test above. Identical in every respect
    // except that this host has been probed and meets the requirement. Without
    // this, a build whose gate refused unconditionally would score row 30 PASS.
    let world = World::new();
    require(&world, "execution_requirements:\n  control_surface: hard\n");
    measure(&world, &measured_hard());

    let mut store = world.store();
    let vertical = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect("an eligible task must launch");

    assert!(
        matches!(vertical.outcome, VerticalOutcome::Complete { .. }),
        "the eligible run did not complete: {:?}",
        vertical.outcome
    );
    assert_eq!(attempts(&world), 1, "the agent did not actually run");
    assert_ne!(world.run_state(), RunState::Blocked);
}

#[test]
fn a_measurement_one_dimension_short_still_refuses() {
    // The gate compares a vector. A host that is `Hard` everywhere except the
    // one dimension asked for must still refuse — otherwise the comparison is
    // "is anything enforced?" rather than "is *this* enforced?".
    let world = World::new();
    require(&world, "execution_requirements:\n  control_surface: hard\n");
    let mut caps = measured_hard();
    caps.control_surface = Enforcement::AuditOnly;
    measure(&world, &caps);

    let mut store = world.store();
    let error = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect_err("a short measurement must refuse");
    let text = error.to_string();
    assert!(text.contains("control_surface"), "{text}");
    assert!(
        text.to_ascii_lowercase().contains("audit_only"),
        "the measured value is not named: {text}"
    );
    assert_eq!(attempts(&world), 0);
    assert_eq!(world.run_state(), RunState::Blocked);
}

#[test]
fn a_probe_for_a_different_version_is_a_miss_and_refuses() {
    // §4.2: "sandbox behaviour changes with OS and CLI versions, and a
    // hardcoded table would silently become a lie after an upgrade." A row for
    // a *different* version triple must not satisfy this key.
    let world = World::new();
    require(&world, "execution_requirements:\n  control_surface: hard\n");
    {
        let mut store = world.store();
        let stale = ProbeKey::new("fake", "0.9.0", "none", "n/a", "test-os-1");
        upsert(store.conn_mut(), &stale, &measured_hard(), 1_000).expect("upsert");
    }

    let mut store = world.store();
    let error = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect_err("a stale probe must refuse");
    assert!(error.to_string().contains("control_surface"));
    assert_eq!(world.run_state(), RunState::Blocked);
}

#[test]
fn a_task_that_requires_nothing_launches_on_an_unprobed_host() {
    // §4.2's gate is "if any *required* dimension exceeds the measured value,
    // refuse". A task with an empty requirement vector compares nothing, so an
    // absent probe is not a refusal — and this is what keeps the gate from
    // becoming an outage on every unprobed host.
    //
    // It is also the reason every pre-S9 test still passes without seeding a
    // probe row, which the master plan predicted would otherwise be necessary.
    let world = World::new();
    // No requirements set, no probe row seeded.

    let mut store = world.store();
    let vertical = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect("a task requiring nothing must launch");
    assert!(matches!(vertical.outcome, VerticalOutcome::Complete { .. }));
    assert_eq!(attempts(&world), 1);
}

#[test]
fn a_requirement_naming_tool_interception_is_refused_at_the_task_row() {
    // §4.2 and §6.3: hooks have known bypasses, so `tool_interception` is
    // informational and may never satisfy a gating requirement. The type system
    // makes it unrepresentable in `ExecutionRequirements`; this test covers the
    // remaining door — a string typed into the durable column — and proves the
    // launch refuses rather than silently ignoring the line.
    //
    // Silently ignoring it would be the worst outcome available: an operator
    // who wrote a requirement would believe it was being enforced.
    let world = World::new();
    require(
        &world,
        "execution_requirements:\n  tool_interception: restricted\n",
    );

    let mut store = world.store();
    let error = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect_err("an unusable requirement must refuse, never be ignored");
    assert!(error.to_string().contains("tool_interception"), "{error}");
    assert_eq!(attempts(&world), 0);
}

#[test]
fn a_requirement_that_parses_to_nothing_refuses_rather_than_gating_nothing() {
    // The nastiest shape available: valid YAML, no error, and an empty vector.
    // `parse_yaml` returns nothing when the `execution_requirements` key is
    // absent — correct for a `project.yaml` that never mentions the block, and
    // silently catastrophic for a column somebody deliberately wrote into.
    //
    // Here the operator mis-nested by one level. Under a gate that accepted the
    // empty result, this task would launch ungated on any host, and the
    // operator would have every reason to believe `control_surface: hard` was
    // being enforced. Present-but-meaningless must not read as absent.
    let world = World::new();
    require(&world, "requirements:\n  control_surface: hard\n");

    let mut store = world.store();
    let error = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect_err("a requirement that gates nothing must refuse");
    assert!(
        error.to_string().contains("execution_requirements"),
        "the refusal does not explain the mis-nesting: {error}"
    );
    assert_eq!(attempts(&world), 0);
    assert_eq!(world.run_state(), RunState::Blocked);
}

#[test]
fn an_unparseable_requirement_refuses_rather_than_defaulting_to_none() {
    // Fail closed on a requirement nobody can read. Treating an unparseable
    // vector as "no requirements" would mean a typo silently disables the gate.
    let world = World::new();
    require(
        &world,
        "execution_requirements:\n  control_surface: [not, a, scalar]\n",
    );

    let mut store = world.store();
    let error = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect_err("an unreadable requirement must refuse");
    assert_eq!(attempts(&world), 0);
    assert_eq!(world.run_state(), RunState::Blocked);
    let _ = error;
}
