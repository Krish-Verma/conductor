//! S6's acceptance bar — master plan §4.6, Part 8's S6, Part 9 rows 7, 8 and 9.
//!
//! > **Verify.** **No configuration of the fake agent can produce more than
//! > `max_attempts` agent invocations**, asserted by counting spawns.
//!
//! # "Counting spawns" is taken literally, three times over
//!
//! An assertion about invocations that counted *intentions* would pass while the
//! system spawned freely, so every counting test here counts three independent
//! things and asserts they agree:
//!
//! 1. **The adapter's counter** — incremented in `AgentAdapter::command`, which
//!    is the last thing Conductor does before `spawn()`. This is the intention.
//! 2. **The durable `attempt` rows** — what a restarting process would read, and
//!    the number the ceiling itself is checked against.
//! 3. **A file the spawned process appends to itself.** Not Conductor's opinion
//!    about how many processes it started: the processes' own testimony. The
//!    hostile adapter wraps every command in a shell that appends one line and
//!    then `exec`s the real agent, so the line count is the number of children
//!    that actually reached user code.
//!
//! Three numbers that agree is the evidence; one number is a claim.
//!
//! # Why a hostile adapter rather than hostile scenarios alone
//!
//! §4.6's breakers compare *failures*, and a failure is produced by the
//! verification profile, not by the agent. Reproducing an oscillation therefore
//! needs an agent whose edits change what the checks say — and one whose
//! behaviour depends on the attempt ordinal, which a single static scenario file
//! cannot express. The adapter selects a pre-generated scenario per ordinal; it
//! is still the real fake-agent binary, editing a real workspace, checked by
//! real subprocesses.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use conductor_agent::{
    AgentAdapter, AgentCommand, AgentResult, FunctionalCapabilities, ResumeInput, RunOutputs,
    StartInput,
};
use conductor_core::{AttemptState, RunId, RunState, TaskId, TaskState};
use conductor_run::repair::breaker::StopReason;
use conductor_run::repair::config::RepairConfig;
use conductor_run::repair::driver::{
    EscalationReason, RepairOutcome, Step, ceiling, drive, repair_once,
};
use conductor_run::repair::observation::{Observation, ObservationKind, history_for_run};
use conductor_run::vertical::VerticalConfig;
use conductor_store::Store;

use common::agent::{fake_agent_binary, warm_the_binary};
use common::vertical::{RUN, TASK, World};

// ---------------------------------------------------------------------------
// Verification profiles. Each one makes a *different* shape of failure.
// ---------------------------------------------------------------------------

/// A required check that prints whatever the agent last wrote to
/// `src/failure.txt` and fails.
///
/// The indirection is what lets a test choose whether two attempts produce the
/// same fingerprint or different ones: §4.6 hashes the failing assertion, and
/// this profile makes the agent the author of it.
const CONTENT_FAILURE_PROFILE: &str = r#"
verification:
  toolchain_fingerprint:
    - ["/bin/echo", "toolchain-v1"]
  required:
    - id: unit-tests
      command: ["/bin/sh", "-c", "cat src/failure.txt; exit 1"]
      timeout_seconds: 60
"#;

/// A required check whose program does not exist.
///
/// §4.5 classifies a check that could not be spawned as `INCONCLUSIVE`, which is
/// §4.7's infrastructure failure — "spawn failed, adapter missing, auth expired"
/// — and which must **not** consume the task's work budget.
const BROKEN_TOOLCHAIN_PROFILE: &str = r#"
verification:
  toolchain_fingerprint:
    - ["/bin/echo", "toolchain-v1"]
  required:
    - id: unit-tests
      command: ["/nonexistent/conductor-no-such-check"]
      timeout_seconds: 60
"#;

// ---------------------------------------------------------------------------
// The hostile agent.
// ---------------------------------------------------------------------------

/// The three fake-agent configurations Part 8's S6 names, plus the one §4.7
/// needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hostility {
    /// "Fake agents that always fail identically." Edits something different
    /// every time — so reconciliation is never `NO_CHANGE` and breaker 3 cannot
    /// be what stops it — while the check's output stays byte-identical.
    AlwaysFailsIdentically,
    /// "…that oscillate." A → B → A, each step a different fingerprint at a
    /// changed tree, so `progressed()` says yes at every step.
    Oscillates,
    /// "…that change nothing." Exits 0 having touched nothing at all.
    ChangesNothing,
    /// Every check is `INCONCLUSIVE`: §4.7's infrastructure failure.
    BreaksTheToolchain,
}

impl Hostility {
    fn profile(&self) -> &'static str {
        match self {
            Hostility::AlwaysFailsIdentically | Hostility::Oscillates => CONTENT_FAILURE_PROFILE,
            // Never reaches verification: the run is `NO_CHANGE` and §4.8 routes
            // it to repair before any check runs.
            Hostility::ChangesNothing => CONTENT_FAILURE_PROFILE,
            Hostility::BreaksTheToolchain => BROKEN_TOOLCHAIN_PROFILE,
        }
    }

    /// The scenario this configuration runs at `ordinal`.
    ///
    /// Every attempt adds a **new file**, not a new version of the same one.
    /// §4.8's reconciled surface is a set of *paths* — `git status` and
    /// `git diff --name-status` — so rewriting one untracked file with different
    /// bytes leaves the surface identical and reconciles as `NO_CHANGE`. That is
    /// correct behaviour and it is not the loop under test here: a fixture that
    /// tripped breaker 3 would prove nothing about breakers 1 and 2.
    fn scenario(&self, ordinal: i64) -> String {
        let edit = format!(
            r#"{{"step":"write_file","path":"src/attempt-{ordinal}.rs","contents":"pub fn attempt() -> u32 {{ {ordinal} }}\n"}}"#
        );
        match self {
            Hostility::AlwaysFailsIdentically => format!(
                r#"{{"id":"identical-{ordinal}","steps":[{edit},
                     {{"step":"write_file","path":"src/failure.txt",
                       "contents":"assertion failed: cannot resolve module 'alpha'\n"}},
                     {{"step":"exit","code":0}}]}}"#
            ),
            Hostility::Oscillates => {
                // A on odd attempts, B on even ones: 1→A, 2→B, 3→A is §4.6's
                // "fingerprint alternates A→B→A".
                let assertion = if ordinal % 2 == 1 {
                    "assertion failed: cannot resolve module 'alpha'"
                } else {
                    "assertion failed: type mismatch, expected u32 found &str"
                };
                format!(
                    r#"{{"id":"oscillate-{ordinal}","steps":[{edit},
                         {{"step":"write_file","path":"src/failure.txt","contents":"{assertion}\n"}},
                         {{"step":"exit","code":0}}]}}"#
                )
            }
            Hostility::ChangesNothing => format!(
                r#"{{"id":"empty-{ordinal}","steps":[
                     {{"step":"emit","kind":"agent.started","detail":"doing nothing"}},
                     {{"step":"exit","code":0}}]}}"#
            ),
            Hostility::BreaksTheToolchain => {
                format!(r#"{{"id":"infra-{ordinal}","steps":[{edit},{{"step":"exit","code":0}}]}}"#)
            }
        }
    }
}

/// The fake agent, per attempt ordinal, wrapped in a shell that leaves durable
/// evidence of its own existence.
struct HostileAgent {
    binary: PathBuf,
    scenarios: Vec<PathBuf>,
    marker: PathBuf,
    commands_built: AtomicUsize,
}

/// More ordinals than any ceiling in this file, so a runaway loop hits an
/// assertion rather than a missing file.
const PREPARED_ORDINALS: i64 = 16;

impl HostileAgent {
    fn new(root: &Path, hostility: Hostility) -> HostileAgent {
        let dir = root.join("hostile");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let scenarios = (1..=PREPARED_ORDINALS)
            .map(|ordinal| {
                let path = dir.join(format!("scenario-{ordinal}.json"));
                std::fs::write(&path, hostility.scenario(ordinal)).expect("write scenario");
                path
            })
            .collect();
        HostileAgent {
            binary: fake_agent_binary(),
            scenarios,
            marker: root.join("spawn-marker.log"),
            commands_built: AtomicUsize::new(0),
        }
    }

    /// Counting method 1: what Conductor intended.
    fn commands_built(&self) -> usize {
        self.commands_built.load(Ordering::SeqCst)
    }

    /// Counting method 3: what the children themselves recorded.
    fn marks_on_disk(&self) -> usize {
        std::fs::read_to_string(&self.marker)
            .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }
}

impl AgentAdapter for HostileAgent {
    fn id(&self) -> &str {
        "hostile"
    }

    fn capabilities(&self) -> FunctionalCapabilities {
        FunctionalCapabilities {
            conductor_assigned_session_id: true,
            // The fake has no sessions, and saying so is the point: §6.1's table
            // is what makes `session_id_for` refuse to hand one over.
            session_resume: false,
            schema_enforced_final_output: true,
            streaming_events: true,
            hermetic_config: true,
            spend_cap: false,
        }
    }

    fn command(&self, input: &StartInput) -> AgentResult<AgentCommand> {
        self.commands_built.fetch_add(1, Ordering::SeqCst);
        let scenario = self
            .scenarios
            .get((input.attempt_ordinal - 1).max(0) as usize)
            .unwrap_or_else(|| {
                panic!(
                    "attempt ordinal {} is past the {PREPARED_ORDINALS} this test \
                     prepared, which means the loop is not bounded",
                    input.attempt_ordinal
                )
            });

        let inner = conductor_agent::fake::FakeAgent::new(self.binary.clone(), scenario.clone())
            .with_max_lifetime_ms(20_000)
            .command(input)?;

        // The wrapper appends one line and then `exec`s, so the agent keeps the
        // pid Conductor recorded and the mark is written by the child rather
        // than by Conductor. `sh -c SCRIPT NAME ARGS…` binds `$0` to NAME.
        let mut env: BTreeMap<String, String> = inner.env.clone();
        env.insert(
            "CONDUCTOR_SPAWN_MARKER".to_string(),
            self.marker.display().to_string(),
        );
        let mut args = vec![
            "-c".to_string(),
            "printf '%s\\n' \"$$\" >> \"$CONDUCTOR_SPAWN_MARKER\"; exec \"$0\" \"$@\"".to_string(),
            inner.program.display().to_string(),
        ];
        args.extend(inner.args.clone());

        Ok(AgentCommand {
            program: PathBuf::from("/bin/sh"),
            args,
            env,
            cwd: inner.cwd,
        })
    }

    fn parse_event(&self, line: &str) -> AgentResult<Vec<conductor_agent::AgentEvent>> {
        conductor_agent::fake::FakeAgent::new(self.binary.clone(), PathBuf::new()).parse_event(line)
    }

    fn extract_report(&self, out: &RunOutputs) -> AgentResult<Option<conductor_core::AgentReport>> {
        conductor_agent::fake::FakeAgent::new(self.binary.clone(), PathBuf::new())
            .extract_report(out)
    }

    fn classify_exit(&self, code: Option<i32>, sig: Option<i32>) -> conductor_core::AttemptOutcome {
        conductor_agent::fake::FakeAgent::new(self.binary.clone(), PathBuf::new())
            .classify_exit(code, sig)
    }

    fn resume_command(&self, _input: &ResumeInput) -> Option<AgentCommand> {
        None
    }
}

// ---------------------------------------------------------------------------
// The world.
// ---------------------------------------------------------------------------

fn config(world: &World) -> VerticalConfig {
    VerticalConfig {
        task_id: TaskId::new(TASK).expect("task id"),
        worker_id: "worker-1".to_string(),
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
        sensitive: conductor_git::SensitivePatterns::default(),
        agent_env_extra: Default::default(),
        // No `execution_requirements` on these fixtures' tasks, so §4.2's
        // gate compares an empty vector and proceeds without a probe.
        probe_key: conductor_run::containment::cache::ProbeKey::new(
            "fake", "test", "none", "n/a", "unprobed",
        ),
    }
}

fn run_id() -> RunId {
    RunId::new(RUN).expect("run id")
}

/// Counting method 2: the durable `attempt` rows.
fn attempt_rows(world: &World) -> usize {
    world
        .store()
        .attempts_for_run(&run_id())
        .expect("attempts")
        .len()
}

/// Set the world up for one hostile configuration and drive it to a stop.
fn drive_hostile(
    hostility: Hostility,
    repair: &RepairConfig,
) -> (World, HostileAgent, RepairOutcome) {
    warm_the_binary();
    let world = World::new().with_profile(hostility.profile());
    let agent = HostileAgent::new(&world.root(), hostility);
    let vertical = config(&world);
    let mut store = world.store();
    let outcome = drive(&mut store, &agent, &vertical, repair, &mut ())
        .expect("the repair loop must not error");
    drop(store);
    (world, agent, outcome)
}

/// The three counts, asserted to agree and to respect the ceiling.
fn assert_bounded(world: &World, agent: &HostileAgent, repair: &RepairConfig, what: &str) -> usize {
    let intended = agent.commands_built();
    let durable = attempt_rows(world);
    let witnessed = agent.marks_on_disk();
    assert_eq!(
        (intended, durable, witnessed),
        (intended, intended, intended),
        "{what}: the three counts disagree — intended {intended}, durable \
         {durable}, witnessed on disk {witnessed}"
    );
    assert!(
        intended <= ceiling(repair),
        "{what}: {intended} agent invocations against a ceiling of {}",
        ceiling(repair)
    );
    intended
}

// ---------------------------------------------------------------------------
// The counting test — Part 8's S6 "Verify" line.
// ---------------------------------------------------------------------------

#[test]
fn no_hostile_configuration_exceeds_the_invocation_ceiling() {
    // > **No configuration of the fake agent can produce more than
    // > `max_attempts` agent invocations**, asserted by counting spawns.
    //
    // The ceiling is `1 + max_attempts + max_infra_retries` rather than
    // `max_attempts` alone, and the difference is not a relaxation: §4.6's
    // `max_attempts` counts *repairs*, so the initial attempt was never in it,
    // and §4.7 exempts infrastructure retries from the budget entirely — which
    // leaves them unbounded unless a term is spent on them. Both extra terms are
    // configuration, so the bound is still a number a person can compute from a
    // YAML file before any agent runs.
    let repair = RepairConfig::default();
    assert_eq!(ceiling(&repair), 4, "1 initial + 2 repairs + 1 infra retry");

    for hostility in [
        Hostility::AlwaysFailsIdentically,
        Hostility::Oscillates,
        Hostility::ChangesNothing,
        Hostility::BreaksTheToolchain,
    ] {
        let (world, agent, outcome) = drive_hostile(hostility, &repair);
        let spawns = assert_bounded(&world, &agent, &repair, &format!("{hostility:?}"));
        assert!(spawns >= 1, "{hostility:?}: nothing ran at all");
        assert!(
            matches!(outcome, RepairOutcome::Escalated { .. }),
            "{hostility:?} must end with a person, got {outcome:?}"
        );
        assert_eq!(
            world.task_state(),
            TaskState::AwaitingReview,
            "{hostility:?}"
        );
    }
}

#[test]
fn every_hostile_configuration_stops_for_the_reason_it_deserves() {
    // The counting test above would pass if one mechanism stopped everything.
    // This is the control: each configuration must be stopped by *its own*
    // breaker, because a bound that always reports the same cause tells a human
    // nothing about which of §4.6's three loops they are looking at.
    let repair = RepairConfig::default();
    let expected = [
        (
            Hostility::AlwaysFailsIdentically,
            EscalationReason::Breaker(StopReason::IdenticalFingerprint),
            2,
        ),
        (
            Hostility::Oscillates,
            EscalationReason::Breaker(StopReason::Oscillation),
            3,
        ),
        (
            Hostility::ChangesNothing,
            EscalationReason::Breaker(StopReason::EmptyEdit),
            2,
        ),
        (
            Hostility::BreaksTheToolchain,
            EscalationReason::Breaker(StopReason::BudgetExhausted {
                limit: conductor_run::repair::breaker::BudgetLimit::InfrastructureRetries,
            }),
            2,
        ),
    ];

    for (hostility, reason, spawns) in expected {
        let (world, agent, outcome) = drive_hostile(hostility, &repair);
        let RepairOutcome::Escalated {
            reason: actual,
            invocations,
        } = &outcome
        else {
            panic!("{hostility:?}: expected an escalation, got {outcome:?}");
        };
        assert_eq!(actual, &reason, "{hostility:?}");
        assert_eq!(invocations, &spawns, "{hostility:?}");
        assert_eq!(agent.commands_built(), spawns, "{hostility:?}");
        assert_eq!(agent.marks_on_disk(), spawns, "{hostility:?}");
        assert_eq!(world.run_state(), RunState::AwaitingReview, "{hostility:?}");
    }
}

#[test]
fn genuine_progress_is_not_stopped_and_reaches_complete() {
    // The control for all four: §4.6's breakers must not stop a run that is
    // fixing the problem. The same machinery, the same profile, and an agent
    // that repairs the failing check on its second attempt.
    warm_the_binary();
    let world = World::new().with_profile(CONTENT_FAILURE_PROFILE);
    let dir = world.root().join("hostile");
    std::fs::create_dir_all(&dir).expect("mkdir");

    // Attempt 1 leaves a failing check; attempt 2 removes the file the check
    // reads and replaces the check's input with nothing, so it passes.
    std::fs::write(
        dir.join("scenario-1.json"),
        r#"{"id":"broken","steps":[
             {"step":"write_file","path":"src/attempt-1.rs","contents":"pub fn a() -> u32 { 1 }\n"},
             {"step":"write_file","path":"src/failure.txt","contents":"assertion failed: alpha\n"},
             {"step":"exit","code":0}]}"#,
    )
    .expect("write");
    std::fs::write(
        dir.join("scenario-2.json"),
        r#"{"id":"fixed","steps":[
             {"step":"write_file","path":"src/attempt-2.rs","contents":"pub fn a() -> u32 { 2 }\n"},
             {"step":"delete_file","path":"src/failure.txt"},
             {"step":"exit","code":0}]}"#,
    )
    .expect("write");

    // The check passes once `src/failure.txt` is gone.
    std::fs::write(
        world.profile(),
        r#"
verification:
  toolchain_fingerprint:
    - ["/bin/echo", "toolchain-v1"]
  required:
    - id: unit-tests
      command: ["/bin/sh", "-c", "test ! -e src/failure.txt || { cat src/failure.txt; exit 1; }"]
      timeout_seconds: 60
"#,
    )
    .expect("write profile");

    let agent = HostileAgent {
        binary: fake_agent_binary(),
        scenarios: vec![dir.join("scenario-1.json"), dir.join("scenario-2.json")],
        marker: world.root().join("spawn-marker.log"),
        commands_built: AtomicUsize::new(0),
    };
    let repair = RepairConfig::default();
    let vertical = config(&world);
    let mut store = world.store();
    let outcome = drive(&mut store, &agent, &vertical, &repair, &mut ()).expect("drive");
    drop(store);

    let RepairOutcome::Complete { invocations, .. } = &outcome else {
        panic!("a run that fixed itself must complete, got {outcome:?}");
    };
    assert_eq!(*invocations, 2, "one failure and one repair");
    assert_eq!(agent.marks_on_disk(), 2);
    assert_eq!(world.task_state(), TaskState::Complete);
    assert_eq!(world.run_state(), RunState::Complete);
}

#[test]
fn budget_exhaustion_escalates_rather_than_completing() {
    // §4.6's `escalate_after: 2 → AWAITING_REVIEW`, reached without any
    // loop-breaker firing: every attempt is a *different* failure at a changed
    // tree, so `progressed()` is true throughout and only the budget is left.
    warm_the_binary();
    let world = World::new().with_profile(CONTENT_FAILURE_PROFILE);
    let dir = world.root().join("hostile");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let scenarios: Vec<PathBuf> = (1..=PREPARED_ORDINALS)
        .map(|n| {
            let path = dir.join(format!("scenario-{n}.json"));
            std::fs::write(
                &path,
                format!(
                    r#"{{"id":"novel-{n}","steps":[
                         {{"step":"write_file","path":"src/attempt-{n}.rs","contents":"pub fn a() -> u32 {{ {n} }}\n"}},
                         {{"step":"write_file","path":"src/failure.txt","contents":"assertion failed: distinct problem number {n}\n"}},
                         {{"step":"exit","code":0}}]}}"#
                ),
            )
            .expect("write");
            path
        })
        .collect();

    let agent = HostileAgent {
        binary: fake_agent_binary(),
        scenarios,
        marker: world.root().join("spawn-marker.log"),
        commands_built: AtomicUsize::new(0),
    };
    let repair = RepairConfig::default();
    let vertical = config(&world);
    let mut store = world.store();
    let outcome = drive(&mut store, &agent, &vertical, &repair, &mut ()).expect("drive");
    drop(store);

    let RepairOutcome::Escalated {
        reason,
        invocations,
    } = &outcome
    else {
        panic!("expected an escalation, got {outcome:?}");
    };
    assert!(
        matches!(
            reason,
            EscalationReason::Breaker(StopReason::BudgetExhausted { .. })
        ),
        "no loop-breaker should have fired: {reason:?}"
    );
    assert_eq!(
        *invocations, 3,
        "one initial attempt plus §4.6's two repairs"
    );
    assert_eq!(agent.marks_on_disk(), 3);
    assert!(*invocations <= ceiling(&repair));
    assert_eq!(world.run_state(), RunState::AwaitingReview);
}

// ---------------------------------------------------------------------------
// Durability — the reason schema v5 exists.
// ---------------------------------------------------------------------------

/// One pass of the loop against a store handle opened just for that pass.
///
/// Every pass opens the database, does its work and closes it, which is as close
/// to §4.7's restart as an in-process test can get without the `SIGKILL` harness
/// `crash_matrix.rs` uses: nothing is carried between passes except what a
/// committed transaction wrote.
fn restarted_pass(world: &World, agent: &HostileAgent, repair: &RepairConfig, now_ms: i64) -> Step {
    let vertical = config(world);
    let mut store = world.store();
    // §4.7 step 2, exactly as `resume_task` does it: a restarting process
    // reclaims what the vanished one held.
    store.expire_leases(now_ms).expect("expire leases");
    let step = repair_once(&mut store, agent, &vertical, repair, &mut ()).expect("one pass");
    drop(store);
    step
}

#[test]
fn the_bound_survives_a_restart_between_every_attempt() {
    // If `RepairHistory` lived in memory, this test would run forever: each pass
    // would see an empty history, decide nothing had happened yet, and spawn.
    // The bound would then be "per process", which under §4.7 — where the
    // process is expected to die — is no bound at all.
    warm_the_binary();
    let world = World::new().with_profile(CONTENT_FAILURE_PROFILE);
    let agent = HostileAgent::new(&world.root(), Hostility::AlwaysFailsIdentically);
    let repair = RepairConfig::default();

    let first = restarted_pass(&world, &agent, &repair, 1);
    assert!(matches!(first, Step::Attempted(_)), "{first:?}");
    assert_eq!(attempt_rows(&world), 1, "before the restart");

    let second = restarted_pass(&world, &agent, &repair, 2);
    assert!(matches!(second, Step::Attempted(_)), "{second:?}");
    assert_eq!(attempt_rows(&world), 2, "after one restart");

    // The third pass is the assertion. A process that had lost the history would
    // spawn a third agent here; one that reads it from the store sees the same
    // fingerprint twice and stops.
    let third = restarted_pass(&world, &agent, &repair, 3);
    let Step::Escalated { reason } = &third else {
        panic!("the restarted process must still see the loop, got {third:?}");
    };
    assert_eq!(
        reason,
        &EscalationReason::Breaker(StopReason::IdenticalFingerprint)
    );
    assert_eq!(attempt_rows(&world), 2, "no third invocation");
    assert_eq!(agent.marks_on_disk(), 2);
    assert_eq!(agent.commands_built(), 2);

    // …and the history a fourth process would read is the one the third saw.
    let store = world.store();
    let history = history_for_run(&store, &run_id()).expect("history");
    assert_eq!(history.entries().len(), 2);
    assert_eq!(history.work_attempts(), 2);
}

#[test]
fn the_ceiling_holds_when_every_crash_loses_the_observation() {
    // The crash window S6's ceiling exists for. `attempt` rows are committed
    // **before** `spawn()`; the observation is written **after** verification.
    // A process killed in between leaves an invocation on the record and no
    // memory of what it produced — so §4.6's history under-counts, `decide` sees
    // a run that has barely started, and the only thing standing between a
    // crash-restart cycle and unbounded spend is the ceiling.
    //
    // The window is injected by deleting the observation rows after each pass,
    // which is exactly the durable state such a crash leaves behind.
    warm_the_binary();
    let world = World::new().with_profile(CONTENT_FAILURE_PROFILE);
    let agent = HostileAgent::new(&world.root(), Hostility::AlwaysFailsIdentically);
    let repair = RepairConfig::default();
    let allowed = ceiling(&repair);

    // Bounded by the test rather than by the loop, so a broken ceiling fails an
    // assertion instead of hanging the suite.
    let mut last: Option<Step> = None;
    for pass in 0..(allowed * 3) {
        let step = restarted_pass(&world, &agent, &repair, pass as i64 + 1);
        let finished = !matches!(step, Step::Attempted(_));
        last = Some(step);
        if finished {
            break;
        }
        forget_observations(&world);
    }

    let Some(Step::Escalated { reason }) = &last else {
        panic!("the loop never stopped: {last:?}");
    };
    assert_eq!(
        reason,
        &EscalationReason::Ceiling {
            ceiling: allowed,
            invocations: allowed,
        },
        "with no history to read, the ceiling is the only bound left"
    );
    assert_eq!(agent.commands_built(), allowed);
    assert_eq!(attempt_rows(&world), allowed);
    assert_eq!(agent.marks_on_disk(), allowed);
    assert_eq!(world.run_state(), RunState::AwaitingReview);
}

/// Delete the durable observations, leaving the `attempt` rows.
///
/// The state a process killed between `spawn()` and the observation write leaves
/// behind. Written as a deletion rather than as a real `SIGKILL` because the
/// point under test is the *state*, and a kill harness would additionally be
/// testing the supervisor, which `crash_matrix.rs` already does at thirteen
/// points.
fn forget_observations(world: &World) {
    let mut store = world.store();
    conductor_store::with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "DELETE FROM repair_observation WHERE run_id = ?1",
            rusqlite::params![RUN],
        )?;
        Ok(())
    })
    .expect("forget");
}

// ---------------------------------------------------------------------------
// The durable observation itself.
// ---------------------------------------------------------------------------

#[test]
fn the_stored_fingerprint_equals_the_one_recomputed_from_its_own_inputs() {
    // The design rule schema v5 is built on: the inputs are the truth and the
    // digest is a convenience. Nothing reads the column back to decide anything,
    // so the only way it can be wrong is if it disagreed with its inputs at the
    // moment it was written — which is what this pins.
    warm_the_binary();
    let world = World::new().with_profile(CONTENT_FAILURE_PROFILE);
    let agent = HostileAgent::new(&world.root(), Hostility::AlwaysFailsIdentically);
    let repair = RepairConfig::default();
    let vertical = config(&world);
    let mut store = world.store();
    repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("one attempt");
    drop(store);

    let store = world.store();
    let rows = store
        .repair_observations_for_run(&run_id())
        .expect("observations");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.kind, "FAILED");
    assert_eq!(row.failing_checks, vec!["unit-tests".to_string()]);
    assert!(!row.tree_hash.is_empty());

    let recomputed = Observation {
        attempt_id: row.attempt_id.clone(),
        ordinal: row.ordinal,
        kind: ObservationKind::parse(&row.kind).expect("a kind this binary writes"),
        failing_checks: row.failing_checks.clone(),
        assertion: row.assertion.clone(),
        tree_hash: row.tree_hash.clone(),
    };
    assert_eq!(
        recomputed.fingerprint().as_str(),
        row.stored_fingerprint,
        "the stored digest must be the one its own inputs produce"
    );
    assert!(row.stored_fingerprint.starts_with("blake3:"));
}

#[test]
fn the_history_round_trips_through_the_store() {
    // Two attempts in, the reconstructed history must be the one the writer saw:
    // same length, same order, same failures. A history that lost the order
    // would silently disable breaker 2, whose whole subject is order.
    warm_the_binary();
    let world = World::new().with_profile(CONTENT_FAILURE_PROFILE);
    let agent = HostileAgent::new(&world.root(), Hostility::Oscillates);
    let repair = RepairConfig::default();
    let vertical = config(&world);
    let mut store = world.store();
    repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("attempt 1");
    repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("attempt 2");
    drop(store);

    let store = world.store();
    let rows = store
        .repair_observations_for_run(&run_id())
        .expect("observations");
    assert_eq!(
        rows.iter().map(|r| r.ordinal).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_ne!(
        rows[0].stored_fingerprint, rows[1].stored_fingerprint,
        "A and B are different failures, or this fixture is not oscillating"
    );

    let history = history_for_run(&store, &run_id()).expect("history");
    assert_eq!(history.entries().len(), 2);
    let last = history.last_failure().expect("the second attempt failed");
    assert_eq!(last.fingerprint().as_str(), rows[1].stored_fingerprint);
    assert_eq!(last.tree_hash(), rows[1].tree_hash);
}

// ---------------------------------------------------------------------------
// Rows 2, 7 and 9, and §4.6's session rule.
// ---------------------------------------------------------------------------

#[test]
fn row_9_a_repeated_identical_failure_never_starts_attempt_three() {
    // Row 9: "Repeated identical failure | same fingerprint | attempt 2 **not
    // started** | stop at once | no | **yes** | `AWAITING_REVIEW`".
    //
    // The row counts the *repair* attempts: the failure has to happen twice
    // before it can be identical, so the attempt that is never started is the
    // second repair — the third invocation.
    let repair = RepairConfig::default();
    let (world, agent, outcome) = drive_hostile(Hostility::AlwaysFailsIdentically, &repair);

    assert_eq!(
        agent.marks_on_disk(),
        2,
        "attempt 3 must never have started"
    );
    assert!(matches!(
        outcome,
        RepairOutcome::Escalated {
            reason: EscalationReason::Breaker(StopReason::IdenticalFingerprint),
            ..
        }
    ));
    assert_eq!(world.run_state(), RunState::AwaitingReview);
    assert_eq!(world.task_state(), TaskState::AwaitingReview);

    // Both attempts really did fail on the same fingerprint — the control that
    // stops this from passing because the second attempt never ran.
    let store = world.store();
    let rows = store
        .repair_observations_for_run(&run_id())
        .expect("observations");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].stored_fingerprint, rows[1].stored_fingerprint);
}

#[test]
fn row_2_a_crash_before_edits_is_retried_rather_than_read_as_an_empty_edit() {
    // Row 2: "Crash before edits | kill at t=1s | `CRASHED`, `NO_CHANGE` | new
    // attempt, same packet | yes | no | `COMPLETE`". §4.6's breaker 3 stops an
    // empty edit at once; if a crash were recorded as one, the retry this row
    // requires would never happen after the first attempt.
    warm_the_binary();
    let world = World::new();
    let dir = world.root().join("hostile");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join("scenario-1.json"),
        r#"{"id":"crash","steps":[
             {"step":"emit","kind":"agent.started","detail":"about to die"},
             {"step":"kill_self","signal":9}]}"#,
    )
    .expect("write");
    std::fs::write(
        dir.join("scenario-2.json"),
        r#"{"id":"recovered","steps":[
             {"step":"write_file","path":"src/added.rs","contents":"pub fn done() -> u32 { 1 }\n"},
             {"step":"exit","code":0}]}"#,
    )
    .expect("write");

    let agent = HostileAgent {
        binary: fake_agent_binary(),
        scenarios: vec![dir.join("scenario-1.json"), dir.join("scenario-2.json")],
        marker: world.root().join("spawn-marker.log"),
        commands_built: AtomicUsize::new(0),
    };
    let repair = RepairConfig::default();
    let vertical = config(&world);
    let mut store = world.store();

    let first = repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("attempt 1");
    let Step::Attempted(attempted) = &first else {
        panic!("expected an attempt, got {first:?}");
    };
    assert_eq!(
        attempted.vertical.attempt.attempt_state,
        AttemptState::Crashed
    );
    assert_eq!(
        attempted.observation.as_ref().map(|o| o.kind),
        Some(ObservationKind::Crashed),
        "a dead agent is not an empty edit"
    );

    let second = repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("attempt 2");
    assert!(
        matches!(second, Step::Complete(_)),
        "row 2's retry must happen, got {second:?}"
    );
    drop(store);
    assert_eq!(world.task_state(), TaskState::Complete);
    assert_eq!(agent.marks_on_disk(), 2);
}

#[test]
fn attempt_two_is_given_a_packet_that_names_what_attempt_one_already_tried() {
    // §6.5: "an explicit `do_not_retry` list of approaches already tried. That
    // last field is what stops attempt 2 from being attempt 1 again." §4.6 pairs
    // it with `new_session_on_attempt: 2`, and the pairing is the design: the
    // packet is what makes discarding the agent's context safe.
    warm_the_binary();
    let world = World::new().with_profile(CONTENT_FAILURE_PROFILE);
    let agent = HostileAgent::new(&world.root(), Hostility::AlwaysFailsIdentically);
    let repair = RepairConfig::default();
    let vertical = config(&world);
    let mut store = world.store();

    let first = repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("attempt 1");
    let Step::Attempted(one) = &first else {
        panic!("expected an attempt, got {first:?}");
    };
    assert_eq!(one.packet.attempt_ordinal, 1);
    assert!(
        one.packet.do_not_retry.is_empty(),
        "nothing had been tried yet"
    );
    assert!(
        !one.packet.fresh_session,
        "attempt 1 has nothing to discard"
    );

    let second = repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("attempt 2");
    let Step::Attempted(two) = &second else {
        panic!("expected an attempt, got {second:?}");
    };
    drop(store);

    assert_eq!(two.packet.attempt_ordinal, 2);
    assert!(
        two.packet.fresh_session,
        "§4.6: `new_session_on_attempt: 2` — the stuck context is the problem"
    );
    assert_eq!(two.packet.do_not_retry.len(), 1);
    let tried = &two.packet.do_not_retry[0];
    assert_eq!(tried.attempt_ordinal, 1);
    assert_eq!(tried.outcome, "FAILED");
    assert_eq!(tried.failing_checks, vec!["unit-tests".to_string()]);
    assert!(
        tried.assertion.contains("cannot resolve module"),
        "{tried:?}"
    );

    // The bounded excerpt, and the budget, both from durable state.
    let excerpt = two.packet.excerpt.as_ref().expect("attempt 1 failed");
    assert!(!excerpt.lines.is_empty());
    assert!(
        excerpt.lines.len() <= conductor_run::repair::packet::EXCERPT_LINES,
        "§6.5 caps the excerpt at 40 lines"
    );
    // Counted as the packet is built, so this attempt is inside both numbers:
    // §4.6's two repairs are both still ahead, and one of the ceiling's four
    // invocations has already been spent on attempt 1.
    assert_eq!(two.packet.remaining_budget.repairs, 2);
    assert_eq!(
        two.packet.remaining_budget.invocations,
        ceiling(&repair) - 1
    );

    // §6.5's "the diff of what the previous attempt changed", read from the
    // repository rather than from anybody's report.
    assert!(
        two.packet
            .previous_diff
            .changed_paths
            .iter()
            .any(|p| p.contains("src/attempt-1.rs")),
        "{:?}",
        two.packet.previous_diff
    );
}

#[test]
fn attempt_two_is_never_handed_a_session_the_adapter_cannot_resume() {
    // The other half of §4.6's session rule, asserted on the durable row rather
    // than on the decision: `attempt.agent_session_id` is what a restart reads,
    // and a value there that the adapter cannot use would be Conductor recording
    // a resumption that never happened.
    warm_the_binary();
    let world = World::new().with_profile(CONTENT_FAILURE_PROFILE);
    let agent = HostileAgent::new(&world.root(), Hostility::AlwaysFailsIdentically);
    let repair = RepairConfig::default();
    let vertical = config(&world);
    let mut store = world.store();
    repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("attempt 1");
    repair_once(&mut store, &agent, &vertical, &repair, &mut ()).expect("attempt 2");
    drop(store);

    let store = world.store();
    let mut stmt = store
        .conn()
        .prepare("SELECT agent_session_id FROM attempt WHERE run_id = ?1 ORDER BY ordinal")
        .expect("prepare");
    let sessions: Vec<Option<String>> = stmt
        .query_map([RUN], |row| row.get::<_, Option<String>>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(sessions, vec![None, None]);
}

/// Keeps `Store` in scope for the assertions that open one directly.
#[allow(dead_code)]
fn _store_is_used(_: &Store) {}
