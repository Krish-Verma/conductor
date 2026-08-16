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
//!
//! # S11 — §4.3's binding rule, at the same call site
//!
//! > **Binding rule:** a task whose policy can produce an approval gate **may
//! > not run unattended** below tier A. Enforced by §4.2's eligibility check,
//! > not by documentation.
//!
//! S9 left that half unwired and said so, because the rule needs the set of
//! actions a task may perform and no task could declare one. S11's plan
//! document can, and materialization writes it to `task.declared_actions`, so
//! the second group of tests below drives the rule through the same
//! `vertical::run_task` entry point — never through
//! `approval::gate::unattended_requirements` directly, for the same reason the
//! first group never calls `eligibility::check`.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::agent::{fake_agent_binary, scenario_file, warm_the_binary};
use common::vertical::{RUN, TASK, World};
use conductor_core::containment::{Enforcement, ExecutionCapabilities};
use conductor_core::{RunId, RunState, TaskId, TaskState};
use conductor_run::containment::cache::{ProbeKey, upsert};
use conductor_run::policy::load::{parse_document, persist, resolve_documents, snapshot};
use conductor_run::policy::model::Origin;
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
        // The fake agent authenticates against nothing.
        credential_home: None,
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

/// A policy that **can** produce an approval gate for `git.push`.
const GATES_PUSH: &str =
    "policy:\n  rules:\n    - {id: g.push, action: git.push, effect: require_approval}\n";

/// A policy that gates something else entirely, so `git.push` is ungateable
/// under it. The negative half of §4.3's predicate, and the reason the
/// permitted path is a real path rather than an unreachable one.
const GATES_ONLY_DEPLOY: &str = "policy:\n  rules:\n    - {id: g.deploy, action: \
     deployment.execute, effect: require_approval}\n";

/// Pin the run to a real policy snapshot.
///
/// The fixture seeds a placeholder blob, which is enough for every test that
/// never asks what the policy says. §4.3's rule does ask, and it asks through
/// `run.policy_hash → policy_snapshot` — the run's own pinned snapshot, not a
/// file and not a value a caller supplies (§4.4, acceptance row 23) — so these
/// tests put a decodable one there and repoint the run at it.
fn pin_policy(world: &World, yaml: &str) {
    let document = parse_document(yaml, Origin::Global).expect("parse policy");
    let policy = resolve_documents(Some(document), None, None).expect("resolve policy");
    let taken = snapshot(&policy);
    let mut store = world.store();
    persist(store.conn_mut(), &taken, 0).expect("persist snapshot");
    store
        .conn()
        .execute(
            "UPDATE run SET policy_hash = ?1 WHERE id = ?2",
            rusqlite::params![taken.hash, RUN],
        )
        .expect("pin the run to it");
}

/// Pin the run to a snapshot whose blob does not decode.
///
/// Not a corrupted *hash* — the row exists and is found. What cannot be
/// established is what the rules are, which is the input §4.3's question needs
/// and the one this fixture removes.
fn corrupt_policy(world: &World) {
    let store = world.store();
    store
        .conn()
        .execute(
            "INSERT OR REPLACE INTO policy_snapshot (hash, canonical_blob, created_at)
             VALUES ('blake3:unreadable', 'this is not a canonical policy', 0)",
            [],
        )
        .expect("insert snapshot");
    store
        .conn()
        .execute(
            "UPDATE run SET policy_hash = 'blake3:unreadable' WHERE id = ?1",
            rusqlite::params![RUN],
        )
        .expect("pin the run to it");
}

/// Write the task's materialized `declared_actions` column, as S11's plan
/// materializer would. `None` leaves it `NULL` — never materialized.
fn declare(world: &World, json: Option<&str>) {
    let mut store = world.store();
    store
        .set_declared_actions(&TaskId::new(TASK).expect("id"), json)
        .expect("declare actions");
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

// ---------------------------------------------------------------------------
// S11 — §4.3's binding rule
// ---------------------------------------------------------------------------

#[test]
fn a_declared_action_the_policy_cannot_gate_launches_on_a_host_below_tier_a() {
    // The permitted half of §4.3's predicate. The task declares `git.push`, the
    // host has never been probed — so `control_surface` measures `None`, which
    // is below tier A — and the launch proceeds, because nothing in this policy
    // can turn `git.push` into an approval gate and a task that cannot be gated
    // has no approval integrity to protect.
    //
    // **Positive control: this cannot fail before the rule is wired.** It exists
    // so that the refusal below is a statement about *this policy*, not about
    // any task that declares anything.
    let world = World::new();
    pin_policy(&world, GATES_ONLY_DEPLOY);
    declare(&world, Some(r#"["git.push"]"#));

    let mut store = world.store();
    let vertical = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect("a task whose declared action nothing can gate must launch");
    assert!(
        matches!(vertical.outcome, VerticalOutcome::Complete { .. }),
        "the ungateable run did not complete: {:?}",
        vertical.outcome
    );
    assert_eq!(attempts(&world), 1, "the agent did not actually run");
}

#[test]
fn a_declared_action_the_policy_can_gate_may_not_run_unattended_below_tier_a() {
    // §4.3's binding rule, on the same unprobed host as the test above and with
    // one thing changed: the policy can produce an approval gate for the action
    // the task declares.
    //
    // The task's own `execution_requirements` column is untouched, deliberately.
    // §4.3 is the *only* source of the requirement here, so a build that read
    // the task's vector and stopped — which is exactly what S9 shipped — lets
    // this launch.
    let world = World::new();
    pin_policy(&world, GATES_PUSH);
    declare(&world, Some(r#"["git.push"]"#));

    let mut store = world.store();
    let outcome = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    );

    match outcome {
        Ok(vertical) => panic!(
            "a task whose policy can gate its declared action ran unattended \
             below tier A and produced {:?}",
            vertical.outcome
        ),
        Err(error) => {
            let text = error.to_string();
            // Row 30's "refuse with dimension named", for the dimension §4.3's
            // tier A *is*: `control_surface: Hard` (M10, M11).
            assert!(
                text.contains("control_surface"),
                "the refusal does not name the dimension: {text}"
            );
        }
    }

    assert_eq!(attempts(&world), 0, "an attempt row was created");
    assert!(
        !world.workspace().exists(),
        "a workspace was cloned for a run that was refused"
    );
    assert_eq!(world.run_state(), RunState::Blocked);
    assert_eq!(world.task_state(), TaskState::Blocked);

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
    assert!(
        refusal.evidence_ref.contains("control_surface"),
        "the finding does not name the dimension: {}",
        refusal.evidence_ref
    );
}

#[test]
fn the_same_gateable_task_launches_on_a_host_measured_hard() {
    // The positive control for the test above, and the one that keeps the rule
    // from being "refuse every task that declares an action". Identical fixture,
    // one seeded probe row: this host is measured at tier A, so the binding rule
    // is satisfied rather than violated.
    //
    // **Positive control: this cannot fail before the rule is wired** — an
    // unwired gate launches it too. It fails against an over-eager wiring, which
    // is the failure mode it exists to catch.
    let world = World::new();
    pin_policy(&world, GATES_PUSH);
    declare(&world, Some(r#"["git.push"]"#));
    measure(&world, &measured_hard());

    let mut store = world.store();
    let vertical = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect("a gateable task on a tier A host must launch");
    assert!(
        matches!(vertical.outcome, VerticalOutcome::Complete { .. }),
        "the contained run did not complete: {:?}",
        vertical.outcome
    );
    assert_eq!(attempts(&world), 1, "the agent did not actually run");
    assert_ne!(world.run_state(), RunState::Blocked);
}

#[test]
fn a_task_never_materialized_from_a_plan_is_not_gated_by_the_binding_rule() {
    // `declared_actions` is `NULL`: no plan document has ever been read for this
    // task, which is every task row written before schema v8 and every task the
    // CLI's `create_task` still writes. Schema v8's own note is the reason this
    // is not treated as "declares nothing gateable" — the honest statement about
    // such a row is "never checked", not "checked and found empty".
    //
    // Fail-closed would argue for refusing it. It does not, and that is a
    // deliberate ruling rather than an oversight: gating `NULL` would change the
    // meaning of every existing task row, and §4.3's rule would be enforced
    // against tasks whose declaration nobody has ever been asked for. The
    // pre-S11 behaviour is preserved and named, so it cannot be mistaken for the
    // rule having been applied and passed.
    //
    // **Positive control: this cannot fail before the rule is wired.**
    let world = World::new();
    pin_policy(&world, GATES_PUSH);
    // No `declare(...)` call: the column stays `NULL`.

    let mut store = world.store();
    let vertical = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect("a pre-S11 task keeps its S9 behaviour");
    assert!(matches!(vertical.outcome, VerticalOutcome::Complete { .. }));
    assert_eq!(attempts(&world), 1);
}

#[test]
fn a_task_that_materialized_an_empty_action_list_has_nothing_to_gate() {
    // `'[]'`: a plan document *was* read, and its author declared zero actions.
    // The rule is applied and answers "no gate is possible", which is a
    // different fact from the `NULL` case above reaching the same launch.
    //
    // **Positive control: this cannot fail before the rule is wired.**
    let world = World::new();
    pin_policy(&world, GATES_PUSH);
    declare(&world, Some("[]"));

    let mut store = world.store();
    let vertical = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect("a task that declares no action must launch");
    assert!(matches!(vertical.outcome, VerticalOutcome::Complete { .. }));
    assert_eq!(attempts(&world), 1);
}

#[test]
fn a_materialized_task_is_refused_when_the_runs_policy_cannot_be_read_and_a_never_materialized_one_is_not()
 {
    // The behavioural difference between `NULL` and `'[]'`, which is the whole
    // reason schema v8 keeps them apart.
    //
    // §4.3's predicate has two operands — *"a task whose **policy** can produce
    // an approval gate"* — so answering it for a materialized task means reading
    // the policy its run is pinned to. Both halves of this test point their run
    // at a snapshot row that exists and does not decode.
    //
    // * `NULL`: the rule does not apply, nothing asks what the policy says, and
    //   the launch proceeds exactly as it did at S9.
    // * `'[]'`: the rule applies, its second operand cannot be established, and
    //   an undecided rule is a refusal — never an empty policy, which would
    //   allow everything. The same reading `enforce::policy_gate` already takes
    //   of an undecodable snapshot.
    //
    // Collapsing the two would make a row nobody has ever materialized
    // indistinguishable from one that was materialized and proved harmless.
    let never_materialized = World::new();
    corrupt_policy(&never_materialized);
    {
        let mut store = never_materialized.store();
        let vertical = run_task(
            &mut store,
            &success_adapter(&never_materialized),
            &config(&never_materialized),
            &mut (),
        )
        .expect("a task no plan has ever described keeps its S9 behaviour");
        assert!(matches!(vertical.outcome, VerticalOutcome::Complete { .. }));
        assert_eq!(attempts(&never_materialized), 1);
    }

    let materialized = World::new();
    corrupt_policy(&materialized);
    declare(&materialized, Some("[]"));
    let mut store = materialized.store();
    let error = run_task(
        &mut store,
        &success_adapter(&materialized),
        &config(&materialized),
        &mut (),
    )
    .expect_err("a rule whose policy cannot be read must refuse");
    assert!(
        error.to_string().contains("policy"),
        "the refusal does not say which operand was missing: {error}"
    );
    assert_eq!(attempts(&materialized), 0);
    assert_eq!(materialized.run_state(), RunState::Blocked);
}

#[test]
fn declared_actions_that_are_not_a_json_string_array_refuse_rather_than_gating_nothing() {
    // The sibling of `an_unparseable_requirement_refuses_rather_than_defaulting_to_none`,
    // for the column §4.3 reads. A declaration that does not decode leaves the
    // rule's first operand unknown; treating it as "no actions" would mean the
    // one task whose declaration went wrong is the one task the rule stops
    // applying to.
    let world = World::new();
    pin_policy(&world, GATES_PUSH);
    declare(&world, Some(r#"{"git.push": true}"#));

    let mut store = world.store();
    let error = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect_err("an unreadable declaration must refuse");
    assert!(
        error.to_string().contains("declared_actions"),
        "the refusal does not name the column: {error}"
    );
    assert_eq!(attempts(&world), 0);
    assert_eq!(world.run_state(), RunState::Blocked);
}

#[test]
fn a_materialized_task_with_no_active_run_has_no_policy_to_ask_and_is_refused() {
    // The other way the rule's second operand can go missing: the policy §4.3
    // asks about is *the run's*, and a task with no active run has none. There
    // is no fallback to a policy on disk — that is what makes an edit to
    // `.conductor/policy.yaml` unable to change what a run is judged by — so the
    // rule is undecided, and undecided is a refusal.
    let world = World::new();
    pin_policy(&world, GATES_PUSH);
    declare(&world, Some(r#"["git.push"]"#));
    {
        let store = world.store();
        store
            .conn()
            .execute(
                "UPDATE run SET state = 'CANCELLED' WHERE id = ?1",
                rusqlite::params![RUN],
            )
            .expect("cancel the run");
    }

    let mut store = world.store();
    let error = run_task(
        &mut store,
        &success_adapter(&world),
        &config(&world),
        &mut (),
    )
    .expect_err("a task with no run has no pinned policy, so the rule is undecided");
    assert!(
        error.to_string().contains("policy"),
        "the refusal does not say which operand was missing: {error}"
    );
    assert_eq!(attempts(&world), 0);
}
