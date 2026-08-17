//! The crash matrix — the S3 slice's acceptance bar.
//!
//! > **Failure injection.** `SIGKILL` the agent at 8 points · `SIGKILL`
//! > **Conductor** at 12 points · stall · malformed JSONL · duplicate spawn.
//! >
//! > **Verify.** For every (scenario × kill-point) pair, restart converges to
//! > the correct state **with no human input** and loses no work.
//!
//! # How the kills are delivered
//!
//! **Conductor** is a separate process (`conductor-s3-worker`), and it kills
//! *itself* with `SIGKILL` when the sequence reaches the named
//! [`RunPoint`](conductor_run::worker::RunPoint). Self-inflicted because an
//! external kill has to race a sleep to land between two particular statements;
//! this one lands there every time. `SIGKILL` because it cannot be caught: no
//! unwinding, no `Drop`, no flush — which is what the database sees during a
//! power failure.
//!
//! **The agent** kills itself the same way, at a checkpoint in its scenario.
//!
//! # What "converged" means here
//!
//! After each kill the test runs §4.7's nine steps — the *same* code path a
//! restart would run, in a fresh process — and then asserts on the **database**:
//! the run is in a state a human need not touch, no attempt is left claiming to
//! be in flight, nothing is recorded as known that was only guessed, and any
//! work the agent had already written to disk is still there.
//!
//! # A pair is a pair a scenario can actually reach
//!
//! "Every (scenario × kill-point)" is a cross product over *reachable* pairs.
//! Killing at `after-outcome-recorded` in a scenario that stalls until the
//! supervisor times it out is a different case from killing there in one that
//! exits cleanly — but killing at `during-active` in a scenario that never
//! spawns is not a pair at all, it is a point the scenario never reaches. The
//! table below pairs each point with the scenarios that reach it, and asserts
//! the pairing was real by checking the worker actually announced the point
//! before dying.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use conductor_core::{AttemptState, RunId, RunState};
use conductor_git::{Scope, SensitivePatterns};
use conductor_run::recovery::{RecoveryConfig, RecoveryDecision, recover};
use conductor_run::worker::RunPoint;
use conductor_store::Store;

use common::agent::{fake_agent_binary, scenario_file, warm_the_binary};

const RUN: &str = "r-0041";

/// A disposable world: store, source repository, workspaces, artifacts.
struct World {
    dir: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = common::agent::source_repo(dir.path());
        let mut store = Store::open_or_create(dir.path().join("conductor.db")).expect("store");
        seed(&mut store, &common::agent::head(&source), &source);
        World { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn store(&self) -> Store {
        Store::open_or_create(self.dir.path().join("conductor.db")).expect("store")
    }

    fn workspace(&self) -> PathBuf {
        self.dir.path().join("workspaces").join(RUN)
    }
}

/// Seed the parent rows this matrix needs.
///
/// `source` is the **registered tree**, and it stopped being a fiction at S12:
/// §6.5's packet is generated from durable state *plus the plan document in that
/// tree*, so a project row pointing at `/fixture` now means an attempt that cannot
/// be told what to do. `write_conductor_layout` (via `seed_parents_at`) puts a real
/// `.conductor/` there.
fn seed(store: &mut Store, base_commit: &str, source: &Path) {
    common::vertical::seed_parents_at(store, source);
    conductor_store::with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT INTO task (id, plan_version_id, slice_id, state, scope_globs,
                               verification_profile, attempt_budget, created_at)
             VALUES ('T-0012', 'pv-1', 'S3', 'READY', '[\"src/**\"]', '.conductor/verification.yaml', 3, 0)",
            [],
        )?;
        tx.execute(
            "INSERT INTO run (id, task_id, policy_hash, base_commit, run_branch, state,
                              priority, lease_epoch, created_at)
             VALUES ('r-0041', 'T-0012', ?1, ?2, 'conductor/T-0012/r-0041', 'READY', 100, 0, 0)",
            rusqlite::params![common::agent::POLICY_HASH, base_commit],
        )?;
        Ok(())
    })
    .expect("seed");
}

/// What one killed worker left behind.
struct Killed {
    reached: Vec<String>,
    signal: Option<i32>,
}

/// Run the worker until it kills itself at `point`.
fn run_worker_until(world: &World, scenario: &str, point: Option<RunPoint>) -> Killed {
    use std::os::unix::process::ExitStatusExt;

    warm_the_binary();
    let scenario_path = scenario_file(world.path(), scenario);
    let mut command = Command::new(worker_binary());
    command
        .arg("--store")
        .arg(world.path().join("conductor.db"))
        .arg("--source")
        .arg(world.path().join("source"))
        .arg("--workspaces")
        .arg(world.path().join("workspaces"))
        .arg("--artifacts")
        .arg(world.path().join("artifacts"))
        .arg("--quarantine")
        .arg(world.path().join("quarantine"))
        .arg("--scenario")
        .arg(&scenario_path)
        .arg("--fake-agent")
        .arg(fake_agent_binary())
        // A **long** startup budget, because that one absorbs M29's
        // first-execution scan rather than measuring anything.
        //
        // The agent budgets are long too, and deliberately: what this function
        // injects is a kill of *Conductor* at a named point, and an agent that
        // the supervisor times out first would change which point is reached.
        // Nothing here asserts a timeout classification — that is
        // `supervise.rs`'s subject — so a timer that can fire is only a source
        // of flakes under load, never a source of evidence.
        .arg("--startup-timeout-ms")
        .arg("60000")
        .arg("--idle-timeout-ms")
        .arg("60000")
        .arg("--wall-timeout-ms")
        .arg("120000")
        .arg("--grace-ms")
        .arg("300")
        .arg("--heartbeat-ms")
        .arg("100")
        // A hard ceiling on any agent this worker leaves behind, so a killed
        // supervisor cannot leak a process past the end of the suite.
        .arg("--agent-lifetime-ms")
        .arg("20000");
    if let Some(point) = point {
        command.arg("--die-at").arg(point.as_str());
    }

    let output = command.output().expect("run the worker");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let reached: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("at").and_then(|a| a.as_str()).map(str::to_string))
        })
        .collect();

    Killed {
        reached,
        signal: output.status.signal(),
    }
}

/// Run §4.7's nine steps, exactly as a restart would.
fn restart_and_recover(world: &World) -> conductor_run::recovery::RecoveryReport {
    let mut store = world.store();
    let config = RecoveryConfig {
        worker_id: "worker-recovered".to_string(),
        workspaces_root: world.path().join("workspaces"),
        quarantine_root: world.path().join("quarantine"),
        artifacts_root: world.path().join("artifacts"),
        // The conservative choice, and the one §4.7 defaults to in spirit: an
        // adopted agent is one nobody is reading the output of.
        adopt_live_agents: false,
        lease_ms: conductor_store::LEASE_MS,
        scope: Scope::new(["src/**".to_string(), "Cargo.toml".to_string()]),
        sensitive: SensitivePatterns::default(),
    };
    // A time far enough ahead that every lease the dead worker held has lapsed.
    // Not a sleep: the lease predicate is `expires_at < now`, and `now` is an
    // argument precisely so a test does not have to wait sixty seconds.
    let now = now_ms() + conductor_store::LEASE_MS * 2;
    recover(&mut store, &config, now).expect("recovery must not fail")
}

fn worker_binary() -> PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    let target = test_exe
        .parent()
        .and_then(Path::parent)
        .expect("target dir");
    let path = target.join("conductor-s3-worker");
    assert!(path.exists(), "missing worker binary at {}", path.display());
    path
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn run_state(store: &Store) -> RunState {
    store
        .run(&RunId::new(RUN).expect("id"))
        .expect("run")
        .expect("a row")
        .state
}

/// The invariants every converged state must satisfy, whatever the kill point.
fn assert_converged(world: &World, label: &str) {
    let store = world.store();

    // Nothing is left claiming to be running. An attempt still in CREATED,
    // STARTING or ACTIVE after recovery is a supervisor that nobody will ever
    // come back for.
    let in_flight = store.in_flight_attempts().expect("in flight");
    assert!(
        in_flight.is_empty(),
        "{label}: attempts left in flight after recovery: {in_flight:?}"
    );

    // Every attempt that got as far as having an outcome ended at RECONCILED —
    // §5.2: "an attempt is never finished until Conductor has looked at the
    // repository."
    for attempt in store
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts")
    {
        assert_eq!(
            attempt.state,
            AttemptState::Reconciled,
            "{label}: attempt {} ended in {:?}, not RECONCILED",
            attempt.id,
            attempt.state
        );
    }

    // The run is somewhere a human need not be involved to make progress, or
    // somewhere that deliberately requires one. Either is convergence; being
    // stuck in RUNNING is not.
    let state = run_state(&store);
    assert_ne!(
        state,
        RunState::Running,
        "{label}: the run is still RUNNING after recovery, so nothing owns it and nothing will"
    );
    assert!(
        state != RunState::Reconciling,
        "{label}: the run is still RECONCILING, so recovery did not finish its job"
    );

    // No side effect is left in the crash window.
    let unresolved = store.unresolved_effects().expect("effects");
    assert!(
        unresolved.is_empty(),
        "{label}: side effects left INTENDED after recovery: {unresolved:?}"
    );

    // The store itself is sound.
    assert_eq!(
        store.integrity_check().expect("integrity"),
        vec!["ok".to_string()],
        "{label}: integrity_check failed"
    );
    assert_eq!(
        store.foreign_key_check().expect("fk"),
        0,
        "{label}: foreign key violations"
    );
}

// ---------------------------------------------------------------------------
// Conductor killed at each of the twelve points.
// ---------------------------------------------------------------------------

/// The pairs. Each point is exercised against a scenario that reaches it, and
/// the points after the agent starts are exercised against both a clean agent
/// and one that dies on its own — so "Conductor died" and "both died" are
/// distinct cases rather than one case tested twice.
fn conductor_kill_pairs() -> Vec<(RunPoint, &'static str)> {
    vec![
        (RunPoint::AfterClaim, "success"),
        // The clone exists and the store has not been told. Added after this
        // gap turned out to strand the run permanently: `create_workspace`
        // refuses an existing path, so every later attempt derived the same
        // path and hit the same refusal, and no restart could ever converge.
        (RunPoint::AfterWorkspaceCloned, "success"),
        (RunPoint::AfterWorkspaceReady, "success"),
        (RunPoint::AfterBaselineIntended, "success"),
        (RunPoint::AfterBaselineWritten, "success"),
        (RunPoint::AfterAttemptCreated, "success"),
        (RunPoint::BeforeSpawn, "success"),
        (RunPoint::AfterSpawnBeforePid, "success"),
        (RunPoint::AfterPidRecorded, "success"),
        (RunPoint::DuringActive, "stall"),
        (RunPoint::AfterOutcomeRecorded, "success"),
        (RunPoint::AfterReconciling, "success"),
        (RunPoint::AfterRoute, "success"),
        // The same points again where the agent has already written work, so
        // "no work is lost" is a claim with something at stake.
        (RunPoint::AfterPidRecorded, "crash-after-edits"),
        (RunPoint::AfterOutcomeRecorded, "crash-after-edits"),
        (RunPoint::AfterReconciling, "crash-after-edits"),
    ]
}

#[test]
fn killing_conductor_at_every_point_converges_without_a_human() {
    for (point, scenario) in conductor_kill_pairs() {
        let label = format!("conductor killed at {} during {scenario}", point.as_str());
        let world = World::new();

        let killed = run_worker_until(&world, scenario, Some(point));

        assert_eq!(
            killed.signal,
            Some(9),
            "{label}: the worker must die by SIGKILL, not exit"
        );
        assert!(
            killed.reached.iter().any(|r| r == point.as_str()),
            "{label}: the worker never reached the point, so nothing was injected \
             (reached: {:?})",
            killed.reached
        );

        let report = restart_and_recover(&world);
        assert_eq!(
            report.integrity_check,
            vec!["ok".to_string()],
            "{label}: the database did not survive the kill"
        );
        assert_converged(&world, &label);

        // Recovery is idempotent: running it twice must not undo the first pass
        // or produce a second, contradictory decision.
        let again = restart_and_recover(&world);
        assert_converged(&world, &format!("{label} (second recovery)"));
        assert!(
            again
                .decisions
                .iter()
                .all(|d| !matches!(d, RecoveryDecision::EffectAmbiguous { .. })),
            "{label}: a second recovery pass invented an ambiguity"
        );
    }
}

#[test]
fn a_clone_that_outlived_the_worker_that_made_it_is_found_not_forgotten() {
    // The gap between `git clone` returning and `attach_workspace` committing.
    // The clone is the only durable record of itself, and §4.1 puts a descriptor
    // inside it exactly so it can be identified "with no database at all".
    //
    // `assert_converged` alone does **not** catch this: the run reaches a state
    // no human need touch either way. What distinguishes a recovered run from a
    // forgotten one is whether recovery went and looked at the disk — so that is
    // what is asserted here.
    let world = World::new();
    let killed = run_worker_until(&world, "success", Some(RunPoint::AfterWorkspaceCloned));
    assert_eq!(killed.signal, Some(9));
    assert!(
        killed
            .reached
            .iter()
            .any(|r| r == RunPoint::AfterWorkspaceCloned.as_str()),
        "the worker never reached the point, so nothing was injected: {:?}",
        killed.reached
    );

    // The crash window really is what it claims to be: clone on disk, store
    // silent about it.
    assert!(
        world.workspace().exists(),
        "the clone should have outlived its worker"
    );
    {
        let store = world.store();
        assert!(
            store
                .run(&RunId::new(RUN).expect("id"))
                .expect("run")
                .expect("a row")
                .workspace_path
                .is_none(),
            "the kill did not land inside the window"
        );
    }

    restart_and_recover(&world);

    let store = world.store();
    let recorded = store
        .run(&RunId::new(RUN).expect("id"))
        .expect("run")
        .expect("a row")
        .workspace_path;
    assert_eq!(
        recorded.as_deref(),
        Some(world.workspace().to_string_lossy().as_ref()),
        "recovery reported on a run whose workspace it never looked for; the next \
         attempt derives this same path, and `create_workspace` refuses a path that \
         exists — so the run would be stranded for good"
    );
    drop(store);
    assert_converged(&world, "clone that outlived its worker");
}

#[test]
fn work_the_agent_finished_before_conductor_died_is_never_lost() {
    // The claim "loses no work" needs work to lose. The agent writes a file and
    // dies; Conductor is killed immediately after recording the outcome, before
    // it has looked at the repository at all.
    let world = World::new();
    let killed = run_worker_until(
        &world,
        "crash-after-edits",
        Some(RunPoint::AfterOutcomeRecorded),
    );
    assert_eq!(killed.signal, Some(9));

    let edit = world.workspace().join("src/added.rs");
    assert!(
        edit.exists(),
        "the agent's work was not on disk to begin with"
    );
    let before = std::fs::read_to_string(&edit).expect("read");

    restart_and_recover(&world);

    assert!(
        edit.exists(),
        "recovery destroyed the work the agent had already committed to disk"
    );
    assert_eq!(std::fs::read_to_string(&edit).expect("read"), before);

    // …and recovery noticed the change rather than merely preserving it.
    let store = world.store();
    assert_eq!(run_state(&store), RunState::Verifying);
}

#[test]
fn a_conductor_killed_between_intent_and_effect_does_not_repeat_the_effect() {
    // Acceptance row 22, at the scale S3 owns: the artifact write. The ledger
    // says INTENDED and the file may or may not exist; recovery re-checks the
    // precondition against the world and never blindly retries.
    let world = World::new();
    let killed = run_worker_until(&world, "success", Some(RunPoint::AfterBaselineIntended));
    assert_eq!(killed.signal, Some(9));

    {
        let store = world.store();
        let unresolved = store.unresolved_effects().expect("effects");
        assert_eq!(
            unresolved.len(),
            1,
            "the kill must have landed inside the crash window"
        );
        assert_eq!(
            unresolved[0].state,
            conductor_core::SideEffectState::Intended
        );
    }

    let report = restart_and_recover(&world);
    let store = world.store();
    assert!(
        store.unresolved_effects().expect("effects").is_empty(),
        "recovery must decide every INTENDED row"
    );
    assert!(
        report.decisions.iter().any(|d| matches!(
            d,
            RecoveryDecision::EffectConfirmed { .. } | RecoveryDecision::EffectNotDone { .. }
        )),
        "the decision must be recorded, not implied: {:?}",
        report.decisions
    );
    // Exactly one ledger row for the operation, whatever happened.
    let rows: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM side_effect", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 1, "an effect must never acquire a second ledger row");
}

#[test]
fn a_conductor_killed_after_spawning_but_before_recording_the_pid_reports_stale() {
    // The worst case: a process exists that Conductor can never identify. §5.2
    // says unknown must not be recorded as known — so the attempt is STALE, not
    // CRASHED, and the repository decides what happens next.
    let world = World::new();
    let killed = run_worker_until(&world, "success", Some(RunPoint::AfterSpawnBeforePid));
    assert_eq!(killed.signal, Some(9));

    let report = restart_and_recover(&world);
    let store = world.store();

    let attempts = store
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].state, AttemptState::Reconciled);
    assert_eq!(
        attempts[0].outcome,
        Some(conductor_core::AttemptOutcome::Stale),
        "an unidentifiable process is STALE; calling it CRASHED would claim an \
         exit nobody observed"
    );
    assert!(
        report
            .decisions
            .iter()
            .any(|d| matches!(d, RecoveryDecision::AttemptStale { .. })),
        "the decision must be recorded: {:?}",
        report.decisions
    );
    assert_converged(&world, "after-spawn-before-pid");
}

#[test]
fn a_live_agent_found_at_startup_is_terminated_and_its_attempt_is_stale() {
    // §4.7 step 3, the "alive" arm. Conductor is killed while the agent is
    // stalling, so the agent is still there when recovery runs — and §6.1's
    // "survives the supervisor's own death" is what makes that possible.
    let world = World::new();
    let killed = run_worker_until(&world, "stall", Some(RunPoint::DuringActive));
    assert_eq!(killed.signal, Some(9));

    let pid: i64 = {
        let store = world.store();
        store
            .conn()
            .query_row("SELECT pid FROM attempt WHERE run_id = ?1", [RUN], |r| {
                r.get(0)
            })
            .expect("the pid was recorded before the kill")
    };
    assert!(
        conductor_run::supervise::start_time_us(pid as i32).is_some(),
        "the agent should have outlived its supervisor; there is nothing to recover otherwise"
    );

    let report = restart_and_recover(&world);
    assert!(
        report
            .decisions
            .iter()
            .any(|d| matches!(d, RecoveryDecision::TerminatedLiveAgent { .. })),
        "the live agent must be dealt with explicitly: {:?}",
        report.decisions
    );
    common::agent::wait_until(
        "the adopted agent to be gone",
        Duration::from_secs(10),
        || conductor_run::supervise::start_time_us(pid as i32).is_none(),
    );
    assert_converged(&world, "live agent at startup");
}

// ---------------------------------------------------------------------------
// The agent killed at each of eight points.
// ---------------------------------------------------------------------------

/// The eight agent kill points, as scenarios. Each dies at a different place in
/// its own script, and the script's checkpoints make the placement exact.
fn agent_kill_scenarios() -> Vec<(&'static str, &'static str)> {
    vec![
        // 1. before it has said anything beyond the readiness line
        (
            "agent-kill-1-before-any-work",
            r#"{"id":"agent-kill-1-before-any-work","steps":[{"step":"kill_self","signal":9}]}"#,
        ),
        // 2. after the first event, before touching the tree
        (
            "agent-kill-2-after-first-event",
            r#"{"id":"agent-kill-2-after-first-event","steps":[{"step":"emit","kind":"agent.started","detail":"x"},{"step":"kill_self","signal":9}]}"#,
        ),
        // 3. mid-edit: one file written, a second still to come
        (
            "agent-kill-3-mid-edit",
            r#"{"id":"agent-kill-3-mid-edit","steps":[{"step":"write_file","path":"src/one.rs","contents":"pub fn one() -> u32 { 1 }\n"},{"step":"checkpoint","name":"first-file"},{"step":"kill_self","signal":9}]}"#,
        ),
        // 4. after every edit, before any report
        (
            "agent-kill-4-after-edits",
            r#"{"id":"agent-kill-4-after-edits","steps":[{"step":"write_file","path":"src/one.rs","contents":"pub fn one() -> u32 { 1 }\n"},{"step":"write_file","path":"src/two.rs","contents":"pub fn two() -> u32 { 2 }\n"},{"step":"checkpoint","name":"after-edits"},{"step":"kill_self","signal":9}]}"#,
        ),
        // 5. after writing the report, before exiting
        (
            "agent-kill-5-after-report",
            r#"{"id":"agent-kill-5-after-report","steps":[{"step":"write_file","path":"src/one.rs","contents":"pub fn one() -> u32 { 1 }\n"},{"step":"write_report","claim":"COMPLETE","files_touched":["src/one.rs"],"summary":"done"},{"step":"checkpoint","name":"after-report"},{"step":"kill_self","signal":9}]}"#,
        ),
        // 6. mid-stall: silent, then killed
        (
            "agent-kill-6-mid-stall",
            r#"{"id":"agent-kill-6-mid-stall","steps":[{"step":"write_file","path":"src/one.rs","contents":"pub fn one() -> u32 { 1 }\n"},{"step":"sleep_ms","ms":200},{"step":"kill_self","signal":9}]}"#,
        ),
        // 7. after mutating repository structure
        (
            "agent-kill-7-after-git-change",
            r#"{"id":"agent-kill-7-after-git-change","steps":[{"step":"git","args":["remote","add","origin","https://example.invalid/x.git"]},{"step":"checkpoint","name":"after-remote"},{"step":"kill_self","signal":9}]}"#,
        ),
        // 8. immediately after a torn JSONL line
        (
            "agent-kill-8-after-torn-line",
            r#"{"id":"agent-kill-8-after-torn-line","steps":[{"step":"write_file","path":"src/one.rs","contents":"pub fn one() -> u32 { 1 }\n"},{"step":"emit_raw","line":"{\"kind\":\"file.writ"},{"step":"kill_self","signal":9}]}"#,
        ),
    ]
}

#[test]
fn killing_the_agent_at_every_point_leaves_a_recoverable_run() {
    for (name, json) in agent_kill_scenarios() {
        let label = format!("agent killed: {name}");
        let world = World::new();
        let scenario_path = common::agent::write_scenario(world.path(), json);

        warm_the_binary();
        let output = Command::new(worker_binary())
            .arg("--store")
            .arg(world.path().join("conductor.db"))
            .arg("--source")
            .arg(world.path().join("source"))
            .arg("--workspaces")
            .arg(world.path().join("workspaces"))
            .arg("--artifacts")
            .arg(world.path().join("artifacts"))
            .arg("--quarantine")
            .arg(world.path().join("quarantine"))
            .arg("--scenario")
            .arg(&scenario_path)
            .arg("--fake-agent")
            .arg(fake_agent_binary())
            .arg("--scope")
            .arg("src/**")
            .arg("--startup-timeout-ms")
            .arg("60000")
            // Generous **on purpose**. Every scenario here ends itself with
            // `SIGKILL`, so no timer is needed to finish the run — and a timer
            // short enough to fire first turns this test into a race it
            // sometimes loses. It did: under parallel load `git remote add`
            // (scenario 7) and a 200 ms sleep (scenario 6) both overran a
            // 1500 ms idle budget, the supervisor timed the agent out, and
            // §6.4's "a timeout wins over the signal Conductor used to enforce
            // it" correctly recorded `TIMED_OUT`. The product was right and the
            // assertion below was measuring the wrong thing.
            //
            // The timers are not going untested: they are the *subject* of
            // `supervise.rs`'s idle/wall/startup tests and of the `stall` and
            // `timeout` catalogued scenarios, where the budget is what is being
            // asserted rather than something in the way.
            .arg("--idle-timeout-ms")
            .arg("60000")
            .arg("--wall-timeout-ms")
            .arg("120000")
            .arg("--agent-lifetime-ms")
            .arg("20000")
            .output()
            .expect("run the worker");
        assert!(
            output.status.success(),
            "{label}: the worker must survive its agent dying: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let store = world.store();
        let attempts = store
            .attempts_for_run(&RunId::new(RUN).expect("id"))
            .expect("attempts");
        assert_eq!(attempts.len(), 1, "{label}");
        assert_eq!(
            attempts[0].state,
            AttemptState::Reconciled,
            "{label}: every attempt ends at RECONCILED"
        );
        assert_eq!(
            attempts[0].outcome,
            Some(conductor_core::AttemptOutcome::Crashed),
            "{label}: §6.4 classifies a SIGKILLed agent as CRASHED"
        );
        assert_eq!(
            attempts[0].signal,
            Some(9),
            "{label}: the signal must be recorded, not inferred"
        );
        drop(store);

        // A restart on top of an already-finished run must change nothing.
        restart_and_recover(&world);
        assert_converged(&world, &label);
    }
}

#[test]
fn an_agent_killed_mid_edit_keeps_the_file_it_had_already_written() {
    let world = World::new();
    let scenario_path = common::agent::write_scenario(
        world.path(),
        r#"{"id":"mid-edit","steps":[
             {"step":"write_file","path":"src/one.rs","contents":"pub fn one() -> u32 { 1 }\n"},
             {"step":"checkpoint","name":"first-file"},
             {"step":"kill_self","signal":9}]}"#,
    );

    warm_the_binary();
    let output = Command::new(worker_binary())
        .arg("--store")
        .arg(world.path().join("conductor.db"))
        .arg("--source")
        .arg(world.path().join("source"))
        .arg("--workspaces")
        .arg(world.path().join("workspaces"))
        .arg("--artifacts")
        .arg(world.path().join("artifacts"))
        .arg("--quarantine")
        .arg(world.path().join("quarantine"))
        .arg("--scenario")
        .arg(&scenario_path)
        .arg("--fake-agent")
        .arg(fake_agent_binary())
        .arg("--agent-lifetime-ms")
        .arg("20000")
        .output()
        .expect("run");
    assert!(output.status.success());

    assert!(
        world.workspace().join("src/one.rs").exists(),
        "the half-finished work is the whole point of row 3"
    );
    let store = world.store();
    assert_eq!(
        run_state(&store),
        RunState::Verifying,
        "changes in scope with no report are CLEAN_NO_REPORT, which advances"
    );
}

#[test]
fn recovering_a_run_whose_workspace_vanished_blocks_it_and_says_so() {
    // §4.7 step 4: "Locate workspace. Absent → run BLOCKED + finding."
    let world = World::new();
    let killed = run_worker_until(&world, "stall", Some(RunPoint::DuringActive));
    assert_eq!(killed.signal, Some(9));

    // The agent is still alive and holding the workspace open; remove it the way
    // a user with a full disk and a `rm -rf` would.
    std::fs::remove_dir_all(world.workspace()).expect("remove the workspace");

    let report = restart_and_recover(&world);
    let store = world.store();

    assert_eq!(run_state(&store), RunState::Blocked);
    assert!(
        report
            .decisions
            .iter()
            .any(|d| matches!(d, RecoveryDecision::WorkspaceMissing { .. })),
        "{:?}",
        report.decisions
    );
    let findings = store
        .findings_for_run(&RunId::new(RUN).expect("id"))
        .expect("findings");
    assert!(
        findings.iter().any(|f| f.kind == "WORKSPACE_MISSING"),
        "a lost workspace must leave a finding a human will see: {findings:?}"
    );
    assert!(findings.iter().all(|f| f.resolution.is_none()));
}
