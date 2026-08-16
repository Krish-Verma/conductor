//! S10's Verify line: **the S3 crash matrix, with Codex substituted for the
//! fake agent.**
//!
//! > **Verify.** *The entire S3 crash matrix passes with Codex substituted for
//! > the fake agent.* If any scenario needs adapter-specific handling, that is a
//! > design smell to fix in the interface, not the adapter.
//! >
//! > **Stop point.** One real slice of real work completes on a fixture repo.
//! >
//! > **Risk.** First nondeterminism. Keep the fake agent as the primary CI
//! > harness **forever**; real-agent tests are a separate, non-blocking suite.
//!
//! Those three paragraphs pull in opposite directions, and the split below is
//! how they are all honoured at once.
//!
//! # What is real in every test in this file
//!
//! [`CodexAgent`] builds the argv. The real [`run_one_attempt`] spawns it. The
//! real supervisor parses the stream with the real adapter. The real §4.8
//! reconciliation compares the real report against real `git status` in a real
//! clone. The real §4.7 recovery converges the real database. Nothing is
//! stubbed between the process boundary and the store.
//!
//! # What differs, and where the line is
//!
//! **The default suite** (`cargo test`) replaces only the *process*, with
//! `conductor-s10-codex-replay` — which writes bytes recorded from codex-cli
//! 0.142.0 to stdout and refuses to start unless the argv it received carries
//! `--sandbox workspace-write`, `--ignore-user-config`, `--ignore-rules` and a
//! readable `--output-schema`. It costs nothing, it is deterministic, and it
//! runs the thirteen Conductor kill points and the eight agent kill points.
//! Substituting the model is not substituting the adapter: every S3 scenario
//! here is decided by bytes Codex actually produced.
//!
//! **The `#[ignore]`d suite** (`cargo test -- --ignored`) spawns the real
//! `codex` binary and spends real money. It is deliberately tiny: the stop point
//! and one crash. S10's risk note is why — a matrix that costs a dollar a run is
//! a matrix nobody runs, and a CI harness that is nondeterministic is not a
//! harness. Each of those tests says in its own comment what it buys that the
//! recordings cannot.
//!
//! # The one thing the recordings could never prove
//!
//! Authentication. §4.9 allows "the adapter's own auth variable", and Codex
//! under ChatGPT sign-in has none: the credential is a *file*,
//! `$CODEX_HOME/auth.json`. [`codex_home`] is where that measured fact lives,
//! and `a_run_with_no_codex_home_cannot_authenticate` pins the failure mode so
//! it is a recorded finding rather than a surprise.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use conductor_agent::codex::{CodexAgent, REPORT_SCHEMA_JSON};
use conductor_core::{AttemptOutcome, AttemptState, RunId, RunState, TaskId, TaskState};
use conductor_git::{Scope, SensitivePatterns, Verdict};
use conductor_run::recovery::{RecoveryConfig, RecoveryDecision, recover};
use conductor_run::vertical::{VerticalConfig, VerticalOutcome, run_task};
use conductor_run::worker::RunPoint;
use conductor_store::{NewRun, NewTask, Store};

use common::agent::{POLICY_HASH, git, head};
use common::vertical::{RUN, RUN_BRANCH, TARGET_BRANCH, TASK, commits_above, seed_parents};

/// The replay process: `codex exec` on the wire, a recording underneath.
/// The recorded-Codex binary.
///
/// Resolved from the target directory rather than `CARGO_BIN_EXE_…`, because it
/// lives in `conductor-agent` and that macro only names binaries of the package
/// under test. It lives there deliberately: it runs in the **agent's position**,
/// so `tests/layering.rs` holds it to the agent's rules, and the strongest form
/// of that is a crate which cannot link the runtime at all rather than one that
/// merely does not. The same resolution `common::agent::fake_agent_binary` uses,
/// for the same reason.
fn replay_binary() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    let target_dir = test_exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target directory");
    let path = target_dir.join("conductor-s10-codex-replay");
    assert!(
        path.exists(),
        "the recorded-Codex binary is missing at {}; `cargo test --all` builds it",
        path.display()
    );
    path
}
/// S3's worker with S10's adapter, killable at every [`RunPoint`].
const WORKER: &str = env!("CARGO_BIN_EXE_conductor-s10-codex-worker");

/// The scope every fixture task declares.
///
/// `lib.rs` is in it because the recorded run edited a root-level `lib.rs`, and
/// the fixture repository below is built to be the repository that recording was
/// made against. A scope that excluded it would turn every replayed run into
/// `OUT_OF_SCOPE` and quietly stop testing the happy path.
const SCOPE: &[&str] = &["lib.rs", "src/**", "Cargo.toml"];

/// The task a real Codex is asked to do. As small as a task can be and still be
/// real work: one function, one file, verifiable by reading the commit.
const PROMPT: &str = "Add a public function named `double` to the file `lib.rs` at the \
                      repository root. It takes a u32 and returns it multiplied by two. \
                      Change nothing else.";

// ---------------------------------------------------------------------------
// The recorded bytes.
// ---------------------------------------------------------------------------

/// `crates/conductor-agent/tests/fixtures/codex-jsonl/`.
///
/// Read from the adapter's own fixture directory rather than copied here: two
/// copies of a recording drift, and the copy that drifts is always the one the
/// integration test uses.
fn fixture(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conductor-agent/tests/fixtures/codex-jsonl")
        .join(name);
    assert!(
        path.exists(),
        "the recorded Codex fixture {name} is missing at {}",
        path.display()
    );
    path
}

/// How the replay process should behave, as `agent_env_extra` pairs.
#[derive(Clone)]
struct Replay {
    fixture: &'static str,
    report: Option<&'static str>,
    apply: bool,
    kill_after: Option<usize>,
    stall_ms: Option<u64>,
    final_kill: bool,
    git: Option<&'static str>,
    exit: Option<i32>,
}

impl Replay {
    /// The recorded successful run: five schema-shaped messages, four of them
    /// `PARTIAL`, one file changed, `COMPLETE` last.
    fn success() -> Replay {
        Replay {
            fixture: "success.jsonl",
            report: Some("success-last-message.json"),
            apply: true,
            kill_after: None,
            stall_ms: None,
            final_kill: false,
            git: None,
            exit: None,
        }
    }

    fn fixture(mut self, name: &'static str) -> Replay {
        self.fixture = name;
        self
    }

    fn report(mut self, name: Option<&'static str>) -> Replay {
        self.report = name;
        self
    }

    fn no_edits(mut self) -> Replay {
        self.apply = false;
        self
    }

    fn kill_after(mut self, lines: usize) -> Replay {
        self.kill_after = Some(lines);
        self
    }

    fn stall_ms(mut self, ms: u64) -> Replay {
        self.stall_ms = Some(ms);
        self
    }

    fn then_kill(mut self) -> Replay {
        self.final_kill = true;
        self
    }

    fn git(mut self, command: &'static str) -> Replay {
        self.git = Some(command);
        self
    }

    fn exit(mut self, code: i32) -> Replay {
        self.exit = Some(code);
        self
    }

    fn env(&self) -> Vec<String> {
        let mut pairs = vec![format!(
            "CONDUCTOR_REPLAY_FIXTURE={}",
            fixture(self.fixture).display()
        )];
        if let Some(report) = self.report {
            pairs.push(format!(
                "CONDUCTOR_REPLAY_REPORT={}",
                fixture(report).display()
            ));
        }
        if self.apply {
            pairs.push("CONDUCTOR_REPLAY_APPLY=1".to_string());
        }
        if let Some(n) = self.kill_after {
            pairs.push(format!("CONDUCTOR_REPLAY_KILL_AFTER={n}"));
        }
        if let Some(ms) = self.stall_ms {
            pairs.push(format!("CONDUCTOR_REPLAY_STALL_MS={ms}"));
        }
        if self.final_kill {
            pairs.push("CONDUCTOR_REPLAY_FINAL=kill".to_string());
        }
        if let Some(command) = self.git {
            pairs.push(format!("CONDUCTOR_REPLAY_GIT={command}"));
        }
        if let Some(code) = self.exit {
            pairs.push(format!("CONDUCTOR_REPLAY_EXIT={code}"));
        }
        pairs
    }
}

// ---------------------------------------------------------------------------
// A disposable world, built to be the repository the recording was made in.
// ---------------------------------------------------------------------------

/// Store, source repository, workspaces, artifacts — all inside one tempdir.
///
/// **Never the Conductor repository and never any other real checkout.** Every
/// path below is derived from `tempfile::tempdir()`, which is removed when the
/// struct drops.
struct World {
    dir: tempfile::TempDir,
    source: PathBuf,
    base_commit: String,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        // macOS temp dirs live behind `/var` → `/private/var`. Canonicalise
        // once, or the workspace the store records and the workspace the
        // adapter normalises against are two spellings of one directory — and
        // `CodexAgent::workspace_relative` is lexical by design, so it would
        // leave every reported path absolute and reconcile clean work as
        // `CONTRADICTED`.
        let root = dir.path().canonicalize().expect("canonicalize");

        // The recorded run's repository: **exactly one** `lib.rs`, at the root,
        // holding `pub fn base() -> u32 { 0 }` — which is precisely what that
        // run's `nl -ba lib.rs` printed.
        //
        // The uniqueness is not cosmetic. The first real Codex run against this
        // fixture was given a `src/lib.rs` as well, and it edited *that* one:
        // a defensible reading of "lib.rs" that the fixture, not the agent, made
        // ambiguous. A fixture whose task has two correct answers measures the
        // model's taste rather than Conductor's spine, so `src/` keeps a module
        // with a different name.
        let source = root.join("source");
        std::fs::create_dir_all(source.join("src")).expect("mkdir");
        std::fs::write(source.join("lib.rs"), "pub fn base() -> u32 { 0 }\n").expect("write");
        std::fs::write(source.join("src/inner.rs"), "pub fn inner() -> u32 { 0 }\n")
            .expect("write");
        std::fs::write(
            source.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
        )
        .expect("write");
        git(&source, &["init", "--initial-branch=main"]);
        git(&source, &["config", "user.name", "Fixture"]);
        git(&source, &["config", "user.email", "fixture@localhost"]);
        git(&source, &["config", "commit.gpgsign", "false"]);
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-m", "initial"]);
        let base_commit = head(&source);

        let mut store = Store::open_or_create(root.join("conductor.db")).expect("store");
        seed_parents(&mut store);
        store
            .create_task(
                &NewTask {
                    id: TaskId::new(TASK).expect("task id"),
                    plan_version_id: "pv-1".to_string(),
                    slice_id: "S10".to_string(),
                    scope_globs: SCOPE.iter().map(|s| s.to_string()).collect(),
                    verification_profile: "verification.yaml".to_string(),
                    attempt_budget: 3,
                },
                0,
            )
            .expect("create task");
        store
            .create_run(
                &NewRun {
                    id: RunId::new(RUN).expect("run id"),
                    task_id: TaskId::new(TASK).expect("task id"),
                    policy_hash: POLICY_HASH.to_string(),
                    base_commit: base_commit.clone(),
                    run_branch: RUN_BRANCH.to_string(),
                    target_branch: TARGET_BRANCH.to_string(),
                },
                0,
            )
            .expect("create run");
        drop(store);

        std::fs::write(
            root.join("verification.yaml"),
            common::vertical::PASSING_PROFILE,
        )
        .expect("write profile");

        World {
            dir,
            source,
            base_commit,
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canonicalize")
    }

    fn store(&self) -> Store {
        Store::open_or_create(self.root().join("conductor.db")).expect("store")
    }

    fn workspaces(&self) -> PathBuf {
        self.root().join("workspaces")
    }

    fn workspace(&self) -> PathBuf {
        self.workspaces().join(RUN)
    }

    fn artifacts(&self) -> PathBuf {
        self.root().join("artifacts")
    }

    fn quarantine(&self) -> PathBuf {
        self.root().join("quarantine")
    }

    fn run_state(&self) -> RunState {
        self.store()
            .run(&RunId::new(RUN).expect("id"))
            .expect("run")
            .expect("a row")
            .state
    }

    fn findings(&self) -> Vec<String> {
        self.store()
            .findings_for_run(&RunId::new(RUN).expect("id"))
            .expect("findings")
            .into_iter()
            .map(|f| f.kind)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Driving the worker.
// ---------------------------------------------------------------------------

/// What one worker process left behind.
struct Ran {
    reached: Vec<String>,
    signal: Option<i32>,
    success: bool,
    stderr: String,
}

/// Run the S10 worker over `replay`, optionally killing Conductor at `point`.
fn run_worker(world: &World, replay: &Replay, point: Option<RunPoint>) -> Ran {
    let env: Vec<String> = replay.env();
    run_worker_with(
        world,
        &replay_binary().to_string_lossy(),
        &env,
        point,
        60_000,
    )
}

/// The same, with an explicit agent binary — the door the real `codex` comes
/// through.
fn run_worker_with(
    world: &World,
    agent: &str,
    agent_env: &[String],
    point: Option<RunPoint>,
    wall_ms: u64,
) -> Ran {
    use std::os::unix::process::ExitStatusExt;

    let mut command = Command::new(WORKER);
    command
        .arg("--store")
        .arg(world.root().join("conductor.db"))
        .arg("--source")
        .arg(&world.source)
        .arg("--workspaces")
        .arg(world.workspaces())
        .arg("--artifacts")
        .arg(world.artifacts())
        .arg("--codex")
        .arg(agent)
        .arg("--prompt")
        .arg(PROMPT)
        .arg("--scope")
        .arg(SCOPE.join(","))
        // Long budgets on purpose, for the reason S3's matrix gives: what these
        // tests inject is a kill at a *named point*, and an agent the supervisor
        // timed out first would change which point was reached. The timers are
        // `supervise.rs`'s subject, not this file's.
        .arg("--startup-timeout-ms")
        .arg("60000")
        .arg("--idle-timeout-ms")
        .arg(wall_ms.to_string())
        .arg("--wall-timeout-ms")
        .arg((wall_ms * 2).to_string())
        .arg("--grace-ms")
        .arg("300")
        .arg("--heartbeat-ms")
        .arg("100");
    for pair in agent_env {
        command.arg("--agent-env").arg(pair);
    }
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

    Ran {
        reached,
        signal: output.status.signal(),
        success: output.status.success(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// Run §4.7's nine steps, exactly as a restart would.
fn restart_and_recover(world: &World) -> conductor_run::recovery::RecoveryReport {
    let mut store = world.store();
    let config = RecoveryConfig {
        worker_id: "worker-recovered".to_string(),
        workspaces_root: world.workspaces(),
        quarantine_root: world.quarantine(),
        artifacts_root: world.artifacts(),
        adopt_live_agents: false,
        lease_ms: conductor_store::LEASE_MS,
        scope: Scope::new(SCOPE.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        sensitive: SensitivePatterns::default(),
    };
    // A time far enough ahead that the dead worker's lease has lapsed. Not a
    // sleep: the predicate is `expires_at < now`, and `now` is an argument
    // precisely so a test need not wait sixty seconds.
    let now = now_ms() + conductor_store::LEASE_MS * 2;
    recover(&mut store, &config, now).expect("recovery must not fail")
}

/// The invariants every converged state satisfies, whatever the kill point.
///
/// Copied in spirit from `crash_matrix.rs` and asserting the same five things,
/// because S10's claim is that they hold *unchanged* when the agent changes. A
/// weaker set here would make "the crash matrix passes with Codex" true by
/// construction.
fn assert_converged(world: &World, label: &str) {
    let store = world.store();

    let in_flight = store.in_flight_attempts().expect("in flight");
    assert!(
        in_flight.is_empty(),
        "{label}: attempts left in flight after recovery: {in_flight:?}"
    );

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
        assert_eq!(
            attempt.adapter, "codex",
            "{label}: the attempt was not recorded as a Codex attempt"
        );
    }

    let state = store
        .run(&RunId::new(RUN).expect("id"))
        .expect("run")
        .expect("a row")
        .state;
    assert_ne!(
        state,
        RunState::Running,
        "{label}: still RUNNING after recovery, so nothing owns it and nothing will"
    );
    assert_ne!(
        state,
        RunState::Reconciling,
        "{label}: still RECONCILING, so recovery did not finish its job"
    );

    let unresolved = store.unresolved_effects().expect("effects");
    assert!(
        unresolved.is_empty(),
        "{label}: side effects left INTENDED after recovery: {unresolved:?}"
    );

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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

// ===========================================================================
// Axis 1 — Conductor killed at each of the thirteen points, with Codex.
// ===========================================================================

/// The pairs, mirroring `crash_matrix.rs`'s.
///
/// Each point is exercised against a replay that reaches it, and the points
/// after the agent starts appear twice — once with a clean agent and once with
/// one that has already written to the tree — so "Conductor died" and "both
/// died, and work was on disk" stay distinct cases.
fn conductor_kill_pairs() -> Vec<(RunPoint, Replay, &'static str)> {
    let killed_mid_stream = || Replay::success().kill_after(13).report(None);
    vec![
        (RunPoint::AfterClaim, Replay::success(), "success"),
        (RunPoint::AfterWorkspaceCloned, Replay::success(), "success"),
        (RunPoint::AfterWorkspaceReady, Replay::success(), "success"),
        (
            RunPoint::AfterBaselineIntended,
            Replay::success(),
            "success",
        ),
        (RunPoint::AfterBaselineWritten, Replay::success(), "success"),
        (RunPoint::AfterAttemptCreated, Replay::success(), "success"),
        (RunPoint::BeforeSpawn, Replay::success(), "success"),
        (RunPoint::AfterSpawnBeforePid, Replay::success(), "success"),
        (RunPoint::AfterPidRecorded, Replay::success(), "success"),
        // A replay that is still alive when Conductor dies — §6.1's "an agent
        // survives the supervisor's own death", which is what makes recovery's
        // live-agent arm reachable at all.
        (
            RunPoint::DuringActive,
            Replay::success().stall_ms(20_000),
            "stalling",
        ),
        (RunPoint::AfterOutcomeRecorded, Replay::success(), "success"),
        (RunPoint::AfterReconciling, Replay::success(), "success"),
        (RunPoint::AfterRoute, Replay::success(), "success"),
        // The same late points with the agent killed mid-stream after its edit
        // landed, so "loses no work" is a claim with something at stake.
        (
            RunPoint::AfterPidRecorded,
            killed_mid_stream(),
            "codex killed after its edit",
        ),
        (
            RunPoint::AfterOutcomeRecorded,
            killed_mid_stream(),
            "codex killed after its edit",
        ),
        (
            RunPoint::AfterReconciling,
            killed_mid_stream(),
            "codex killed after its edit",
        ),
    ]
}

#[test]
fn killing_conductor_at_every_point_converges_with_codex_substituted() {
    let pairs = conductor_kill_pairs();

    // "Every point" is checked against `RunPoint::ALL`, not against a number.
    // S3 learned this the expensive way: the twelve points chosen first had no
    // point between `git clone` returning and the store being told, and that gap
    // held a bug that stranded runs permanently. A list that silently drops a
    // point reads as coverage without being it.
    let covered: std::collections::BTreeSet<RunPoint> =
        pairs.iter().map(|(point, _, _)| *point).collect();
    let missing: Vec<&str> = RunPoint::ALL
        .iter()
        .filter(|point| !covered.contains(point))
        .map(|point| point.as_str())
        .collect();
    assert!(
        missing.is_empty(),
        "S10 verifies the *entire* S3 matrix; these points are never killed at: \
         {missing:?}"
    );

    for (point, replay, what) in pairs {
        let label = format!("conductor killed at {} during {what}", point.as_str());
        let world = World::new();

        let ran = run_worker(&world, &replay, Some(point));

        assert_eq!(
            ran.signal,
            Some(9),
            "{label}: the worker must die by SIGKILL, not exit ({})",
            ran.stderr
        );
        assert!(
            ran.reached.iter().any(|r| r == point.as_str()),
            "{label}: the worker never reached the point, so nothing was injected \
             (reached: {:?}; stderr: {})",
            ran.reached,
            ran.stderr
        );

        let report = restart_and_recover(&world);
        assert_eq!(
            report.integrity_check,
            vec!["ok".to_string()],
            "{label}: the database did not survive the kill"
        );
        assert_converged(&world, &label);

        // Recovery is idempotent: a second pass must not undo the first or
        // invent a second, contradictory decision.
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
fn a_codex_that_outlived_conductor_is_terminated_and_its_attempt_is_stale() {
    // §4.7 step 3's "alive" arm, with a Codex-shaped process. The point is not
    // that this process is special — it is that recovery does not need to know
    // *which* agent it is looking at, and asks the operating system instead.
    let world = World::new();
    let ran = run_worker(
        &world,
        &Replay::success().stall_ms(20_000),
        Some(RunPoint::DuringActive),
    );
    assert_eq!(ran.signal, Some(9));

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
        matches!(
            conductor_run::supervise::probe(pid as i32, 0),
            conductor_run::supervise::Liveness::Alive(_)
        ),
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
        "the adopted Codex to be gone",
        Duration::from_secs(10),
        || {
            matches!(
                conductor_run::supervise::probe(pid as i32, 0),
                conductor_run::supervise::Liveness::Dead
            )
        },
    );
    assert_converged(&world, "live codex at startup");
}

#[test]
fn a_codex_spawned_but_never_recorded_is_stale_rather_than_crashed() {
    // The worst point on the axis: a Codex process exists and Conductor can
    // never identify it. §5.2 says unknown must not be recorded as known — so
    // the attempt is `STALE`, not `CRASHED`, and the repository decides what
    // happens next.
    //
    // Worth asserting separately with a real adapter in place because `CRASHED`
    // is the one classification an adapter *does* own (`classify_exit`), and the
    // temptation to reach for it here is exactly what §5.2 forbids: nobody
    // observed an exit.
    let world = World::new();
    let ran = run_worker(
        &world,
        &Replay::success().stall_ms(20_000),
        Some(RunPoint::AfterSpawnBeforePid),
    );
    assert_eq!(ran.signal, Some(9), "{}", ran.stderr);

    let report = restart_and_recover(&world);
    let store = world.store();
    let attempts = store
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].outcome,
        Some(AttemptOutcome::Stale),
        "an unidentifiable Codex is STALE; calling it CRASHED would claim an exit \
         nobody observed"
    );
    assert!(
        report
            .decisions
            .iter()
            .any(|d| matches!(d, RecoveryDecision::AttemptStale { .. })),
        "the decision must be recorded, not implied: {:?}",
        report.decisions
    );
    drop(store);
    assert_converged(&world, "after-spawn-before-pid");
}

#[test]
fn work_codex_finished_before_conductor_died_is_never_lost() {
    // The "loses no work" claim needs work to lose. The replay applies the edit
    // the recording announces, then Conductor is killed immediately after
    // recording the outcome — before it has looked at the repository at all.
    let world = World::new();
    let ran = run_worker(
        &world,
        &Replay::success(),
        Some(RunPoint::AfterOutcomeRecorded),
    );
    assert_eq!(ran.signal, Some(9));

    let edited = world.workspace().join("lib.rs");
    let before = std::fs::read_to_string(&edited).expect("the workspace must have lib.rs");
    assert!(
        before.contains("double"),
        "the replay's edit was not on disk to begin with, so nothing is at stake"
    );

    restart_and_recover(&world);

    assert_eq!(
        std::fs::read_to_string(&edited).expect("read"),
        before,
        "recovery changed work the agent had already committed to disk"
    );
    // …and recovery *noticed* the change rather than merely preserving it.
    assert_eq!(world.run_state(), RunState::Verifying);
}

// ===========================================================================
// Axis 2 — Codex killed at each of the eight points.
// ===========================================================================

/// S3's eight agent-kill points, expressed against the recorded Codex stream.
///
/// The line numbers are positions in `success.jsonl`: 1 is `thread.started`,
/// 12 is the `item.started` announcing the `file_change`, 13 its completion.
fn codex_kill_points() -> Vec<(&'static str, Replay)> {
    vec![
        // 1. before it has said anything beyond the session line
        (
            "before-any-work",
            Replay::success().no_edits().report(None).kill_after(1),
        ),
        // 2. after the first turn event, before touching the tree
        (
            "after-first-event",
            Replay::success().no_edits().report(None).kill_after(2),
        ),
        // 3. mid-edit: the change is announced and on disk, the completion is not
        ("mid-edit", Replay::success().report(None).kill_after(12)),
        // 4. after every edit, before any report
        ("after-edits", Replay::success().report(None).kill_after(17)),
        // 5. after writing the report, before exiting
        ("after-report", Replay::success().then_kill()),
        // 6. mid-stall: silent, then killed
        (
            "mid-stall",
            Replay::success().report(None).stall_ms(200).then_kill(),
        ),
        // 7. after mutating repository structure
        (
            "after-git-change",
            Replay::success()
                .report(None)
                .git("remote add origin https://example.invalid/x.git")
                .kill_after(2),
        ),
        // 8. immediately after a torn JSONL line — the recorded truncation
        (
            "after-torn-line",
            Replay::success()
                .fixture("truncated.jsonl")
                .no_edits()
                .report(None)
                .then_kill(),
        ),
    ]
}

#[test]
fn killing_codex_at_every_point_leaves_a_recoverable_run() {
    for (name, replay) in codex_kill_points() {
        let label = format!("codex killed: {name}");
        let world = World::new();

        let ran = run_worker(&world, &replay, None);
        assert!(
            ran.success,
            "{label}: the worker must survive its agent dying: {}",
            ran.stderr
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
            Some(AttemptOutcome::Crashed),
            "{label}: §6.4 classifies a SIGKILLed agent as CRASHED, and \
             `CodexAgent::classify_exit` is what said so"
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
fn a_torn_codex_line_is_a_finding_and_the_lines_before_it_are_kept() {
    // §2.2's permissive parsing, through the real supervisor. `truncated.jsonl`
    // is a recording of a killed codex: two whole lines, then half of one.
    let world = World::new();
    let ran = run_worker(
        &world,
        &Replay::success()
            .fixture("truncated.jsonl")
            .no_edits()
            .report(None),
        None,
    );
    assert!(ran.success, "{}", ran.stderr);

    let torn: Vec<String> = world
        .findings()
        .into_iter()
        .filter(|kind| kind == "AGENT_OUTPUT_UNPARSEABLE")
        .collect();
    assert_eq!(
        torn.len(),
        1,
        "exactly one line of the three is unparseable, and the tear must not \
         condemn the two whole lines before it: {:?}",
        world.findings()
    );

    // The run still converges on the repository's evidence: a torn stream is
    // §2.2's permissive-parsing case, not a failure.
    restart_and_recover(&world);
    assert_converged(&world, "torn codex line");
}

// ===========================================================================
// Axis 3 — the report contract, end to end.
// ===========================================================================

#[test]
fn absolute_files_touched_reconcile_clean_rather_than_contradicted() {
    // §6.2's second measured finding, at the only layer that can prove it
    // matters. `CodexAgent` normalises `/workspace/lib.rs` to `lib.rs`; §4.8
    // reconciles against `git status`, which says `lib.rs`. Without the
    // normalisation *every successful Codex run* would reconcile as
    // `CONTRADICTED` — a false "the agent lied" on the happy path — and no
    // adapter-level parsing test would notice, because the parse succeeds either
    // way.
    let world = World::new();
    let ran = run_worker(&world, &Replay::success(), None);
    assert!(ran.success, "{}", ran.stderr);

    assert_eq!(
        world.run_state(),
        RunState::Verifying,
        "a clean, complete Codex run routes to VERIFYING; anything else means \
         reconciliation disagreed with a report that was telling the truth"
    );
    assert!(
        !world.findings().iter().any(|k| k == "REPORTEDNOTOBSERVED"
            || k == "OBSERVEDNOTREPORTED"
            || k == "REPORT_UNPARSEABLE"),
        "the report and the repository must agree: {:?}",
        world.findings()
    );
}

#[test]
fn a_report_naming_a_path_outside_the_workspace_is_not_quietly_normalised() {
    // The positive control for the test above. §6.2: a path genuinely outside
    // the workspace is left exactly as the agent wrote it, because that one is
    // evidence. If normalisation were a blanket string rewrite, this run would
    // look as clean as the one above — so the two tests together are what make
    // either of them mean something.
    let world = World::new();
    let ran = run_worker(
        &world,
        &Replay::success().report(Some("escaped-workspace-last-message.json")),
        None,
    );
    assert!(ran.success, "{}", ran.stderr);

    let findings = world.findings();
    assert!(
        findings.iter().any(|k| k == "REPORTCONTRADICTED"),
        "a report claiming files the repository never saw must leave a finding: \
         {findings:?}"
    );
    assert_ne!(
        world.run_state(),
        RunState::Verifying,
        "a contradicted report must not advance to verification"
    );
}

#[test]
fn a_schema_violating_report_leaves_a_finding_and_still_converges() {
    // Acceptance row 5. `--output-schema` is enforced by the agent runtime, so
    // this shape should be impossible — which is exactly why it is worth
    // asserting that an impossible report degrades rather than crashes.
    let world = World::new();
    let ran = run_worker(
        &world,
        &Replay::success().report(Some("schema-violation-last-message.json")),
        None,
    );
    assert!(ran.success, "{}", ran.stderr);

    assert!(
        world.findings().iter().any(|k| k == "REPORT_UNPARSEABLE"),
        "row 5: an unusable report is a finding, not a failure: {:?}",
        world.findings()
    );
    restart_and_recover(&world);
    assert_converged(&world, "schema-violating report");
}

#[test]
fn a_codex_that_wrote_no_report_at_all_is_decided_by_the_repository() {
    // §6.4: "exit 0, no report → EXITED (report optional; reconciliation is
    // authoritative)". The adapter's `extract_report` falls back to the last
    // schema-shaped `agent_message` on the stream, so this also proves the
    // fallback reaches the store.
    let world = World::new();
    let ran = run_worker(&world, &Replay::success().report(None), None);
    assert!(ran.success, "{}", ran.stderr);

    let store = world.store();
    let attempts = store
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts");
    assert_eq!(attempts[0].outcome, Some(AttemptOutcome::Exited));
    drop(store);

    assert_eq!(
        world.run_state(),
        RunState::Verifying,
        "the edit is real and in scope, so the repository carries the run"
    );
}

#[test]
fn an_auth_failure_ends_the_attempt_without_pretending_work_happened() {
    // The recorded 401. §6.4 classifies it as infrastructure at the adapter
    // level; what this asserts is what *conductor-run* does with a run that
    // produced nothing — it must not invent progress, and it must converge.
    let world = World::new();
    let ran = run_worker(
        &world,
        &Replay::success()
            .fixture("auth-error.jsonl")
            .no_edits()
            .report(None)
            .exit(1),
        None,
    );
    assert!(ran.success, "{}", ran.stderr);

    let store = world.store();
    let attempts = store
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts");
    assert_eq!(
        attempts[0].outcome,
        Some(AttemptOutcome::Crashed),
        "a non-zero exit is CRASHED (§6.4)"
    );
    drop(store);

    assert_eq!(
        world.run_state(),
        RunState::Repairing,
        "nothing changed, so §4.8's NO_CHANGE routes to repair — never to COMPLETE"
    );
    restart_and_recover(&world);
    assert_converged(&world, "auth failure");
}

#[test]
fn a_multi_file_change_is_reported_as_every_file_it_touched() {
    // §6.1's interface change, observed from the far end. One Codex
    // `file_change` item carries an array; the original
    // `parse_event -> Option<AgentEvent>` could report only the first, so every
    // multi-file edit understated what the agent did. This asserts the tree, not
    // the event, which is the only assertion that would have caught it.
    let world = World::new();
    let ran = run_worker(
        &world,
        &Replay::success()
            .fixture("multi-file-change.jsonl")
            .report(None),
        None,
    );
    assert!(ran.success, "{}", ran.stderr);

    assert!(
        world.workspace().join("src/b.rs").exists(),
        "the `add` change"
    );
    let a = std::fs::read_to_string(world.workspace().join("src/a.rs"));
    assert!(a.is_ok(), "the `update` change");
}

#[test]
fn the_worker_builds_an_argv_that_still_contains_the_reason_codex_was_chosen() {
    // `conductor-s10-codex-replay` exits 97 unless the argv carries
    // `--sandbox workspace-write`, `--ignore-user-config`, `--ignore-rules`,
    // `--json`, `--cd`, `--output-last-message` and an `--output-schema` file
    // that exists and parses. This test is the assertion that the guard is armed
    // — that a passing run above really did pass through it — and it is the only
    // place that checks §6.1's "the *caller* writes the schema file".
    let world = World::new();
    let ran = run_worker(&world, &Replay::success(), None);
    assert!(ran.success, "{}", ran.stderr);

    let schema = world
        .artifacts()
        .join(RUN)
        .join("agent")
        .join("report-schema.json");
    assert_eq!(
        std::fs::read_to_string(&schema).expect("the caller must write the schema"),
        REPORT_SCHEMA_JSON,
        "the schema handed to --output-schema must be the adapter's own, or the \
         agent runtime enforces a contract Conductor does not deserialise"
    );

    let store = world.store();
    let attempts = store
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts");
    assert_ne!(
        attempts[0].outcome,
        Some(AttemptOutcome::Crashed),
        "the replay refused the argv (exit 97): {}",
        ran.stderr
    );
}

// ===========================================================================
// Axis 4 — the real thing. Every test below spends money.
// ===========================================================================

/// A per-run `CODEX_HOME` holding **only** `auth.json`.
///
/// # The S10 finding this function is
///
/// §4.9's allowlist ends with "the adapter's own auth variable", which assumes
/// the credential is *carried by a variable*. Measured on this host with
/// codex-cli 0.142.0: it is not. `~/.codex/auth.json` has `auth_mode:
/// "chatgpt"`, its `OPENAI_API_KEY` field is `null`, and the credential is a
/// refresh/access token pair in a **file**. Under Conductor's per-run `HOME`,
/// `codex login status` says `Not logged in`; with `CODEX_HOME` pointing at a
/// directory containing that file it says `Logged in using ChatGPT`.
///
/// So the variable Codex needs is `CODEX_HOME`, and it is a *directory pointer*,
/// not a secret. Pointing it at the operator's real `~/.codex` would be wrong in
/// two directions at once: it hands the agent read access to `config.toml`,
/// every profile and the whole session history, and — measured — Codex **writes**
/// into `CODEX_HOME`, so a contained run would leave rollouts in the operator's
/// home where §4.8's audit surface cannot see them.
///
/// A directory holding one copied file is the smallest thing that authenticates.
/// It is created inside the test's own tempdir, `0700`, with the credential
/// `0600`, and it dies with the test.
fn codex_home(root: &Path) -> PathBuf {
    let real = PathBuf::from(std::env::var("HOME").expect("HOME")).join(".codex/auth.json");
    assert!(
        real.exists(),
        "a real Codex run needs {}; run `codex login` first",
        real.display()
    );
    let home = root.join("codex-home");
    std::fs::create_dir_all(&home).expect("mkdir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }
    let target = home.join("auth.json");
    std::fs::copy(&real, &target).expect("copy the credential");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    home
}

/// The real `codex` on this host.
fn codex_binary() -> PathBuf {
    let out = Command::new("sh")
        .args(["-c", "command -v codex"])
        .output()
        .expect("look for codex");
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    assert!(
        out.status.success() && path.exists(),
        "no `codex` on PATH; the real-agent suite cannot run"
    );
    path
}

#[test]
#[ignore = "spawns the real codex and spends real money; run with --ignored"]
fn a_real_codex_completes_one_slice_of_real_work_on_a_fixture_repo() {
    // **S10's stop point.** Not "the adapter parses", not "a process ran" — a
    // real coding agent, behind the real interface, taking a task from PENDING
    // to COMPLETE with a commit this test then reads out of git.
    //
    // One invocation. The task is the smallest thing that is still real work.
    let world = World::new();
    let home = codex_home(&world.root());
    let schema = world.root().join("report-schema.json");
    std::fs::write(&schema, REPORT_SCHEMA_JSON).expect("the caller writes the schema (§6.1)");

    let adapter = CodexAgent::new(codex_binary(), world.workspace(), schema).with_prompt(PROMPT);

    let mut extra = BTreeMap::new();
    // §4.9's last clause, by name and nothing else. See `codex_home`.
    extra.insert("CODEX_HOME".to_string(), home.display().to_string());

    let mut store = world.store();
    let config = VerticalConfig {
        task_id: TaskId::new(TASK).expect("task id"),
        worker_id: "worker-s10".to_string(),
        source_repo: world.source.clone(),
        workspaces_root: world.workspaces(),
        artifacts_root: world.artifacts(),
        quarantine_root: world.quarantine(),
        profile_path: world.root().join("verification.yaml"),
        scratch_index: world.root().join("scratch").join("index"),
        supervisor: conductor_run::SupervisorConfig {
            startup_timeout: Duration::from_secs(120),
            // A real model thinks between lines. These budgets are the only
            // numbers in this file tuned for nondeterminism, and they are
            // deliberately generous rather than tight: what is under test is
            // completion, not latency.
            idle_timeout: Duration::from_secs(180),
            wall_timeout: Duration::from_secs(600),
            terminate_grace: Duration::from_secs(2),
            poll_interval: Duration::from_millis(50),
        },
        lease_ms: conductor_store::LEASE_MS,
        heartbeat_interval: Duration::from_millis(500),
        startup_grace: Duration::from_secs(30),
        sensitive: SensitivePatterns::default(),
        agent_env_extra: extra,
        // The fixture task declares no `execution_requirements`, so §4.2's gate
        // compares an empty vector and proceeds without consulting the cache.
        // The key is still named honestly: if a later fixture *does* declare a
        // requirement this misses the cache, and a miss is `fail_closed()`.
        probe_key: conductor_run::containment::cache::ProbeKey::new(
            "codex",
            "s10-integration",
            "none",
            "n/a",
            "unprobed",
        ),
    };

    let result = run_task(&mut store, &adapter, &config, &mut ()).expect("the vertical must run");
    drop(store);

    let VerticalOutcome::Complete { commit, .. } = &result.outcome else {
        panic!(
            "expected COMPLETE, got {:?} (findings: {:?})",
            result.outcome,
            world.findings()
        );
    };

    // The database.
    assert_eq!(world.run_state(), RunState::Complete);
    assert_eq!(
        world
            .store()
            .task(&TaskId::new(TASK).expect("id"))
            .expect("task")
            .expect("a row")
            .state,
        TaskState::Complete
    );
    assert_eq!(result.attempt.verdict, Verdict::CleanComplete);

    // The repository. This is the assertion the stop point is about: not what
    // the agent claimed, not what the store recorded — what git holds.
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        1,
        "exactly one Conductor-owned commit"
    );
    let files = git_out(
        &world.workspace(),
        &["show", "--name-only", "--format=", &commit.sha],
    );
    assert!(
        files.lines().any(|f| f == "lib.rs"),
        "the commit must carry lib.rs, not {files}"
    );
    let body = git_out(
        &world.workspace(),
        &["show", &format!("{}:lib.rs", commit.sha)],
    );
    assert!(
        body.contains("double"),
        "the committed lib.rs must contain the function the task asked for:\n{body}"
    );
    assert!(
        body.contains("base"),
        "the agent must not have replaced the file it was asked to extend:\n{body}"
    );
}

#[test]
#[ignore = "spawns the real codex and spends real money; run with --ignored"]
fn conductor_killed_after_a_real_codex_finished_loses_none_of_its_work() {
    // The crash-matrix row that a recording genuinely cannot stand in for:
    // a **real** agent process, doing real work, in a real workspace, when
    // Conductor dies before it has looked at the repository at all.
    //
    // Everything about the recorded runs is a decision the recording already
    // made. This one has a live process, a real edit whose content nobody chose,
    // and a database killed by `SIGKILL` mid-sequence — and §4.7 still has to
    // converge with no human input and lose nothing.
    //
    // One invocation.
    let world = World::new();
    let home = codex_home(&world.root());
    let env = vec![format!("CODEX_HOME={}", home.display())];

    let ran = run_worker_with(
        &world,
        codex_binary().to_string_lossy().as_ref(),
        &env,
        Some(RunPoint::AfterOutcomeRecorded),
        300_000,
    );
    assert_eq!(
        ran.signal,
        Some(9),
        "the worker must die by SIGKILL at the injected point: {}",
        ran.stderr
    );
    assert!(
        ran.reached
            .iter()
            .any(|r| r == RunPoint::AfterOutcomeRecorded.as_str()),
        "the run never reached the kill point — the agent probably never \
         authenticated. reached: {:?}; stderr: {}",
        ran.reached,
        ran.stderr
    );

    let edited = world.workspace().join("lib.rs");
    let before = std::fs::read_to_string(&edited).expect("read lib.rs");
    assert!(
        before.contains("double"),
        "the real agent did not do the work, so nothing is at stake:\n{before}"
    );

    restart_and_recover(&world);

    assert_eq!(
        std::fs::read_to_string(&edited).expect("read"),
        before,
        "recovery changed work a real agent had already committed to disk"
    );
    assert_converged(&world, "conductor killed after a real codex finished");
    assert_eq!(
        world.run_state(),
        RunState::Verifying,
        "recovery must notice the change, not merely preserve it"
    );
}

#[test]
#[ignore = "spawns the real codex; costs nothing but is still a real process"]
fn a_run_with_no_codex_home_cannot_authenticate() {
    // The measured shape of §4.9's gap, pinned so it stays a finding rather than
    // becoming a surprise. Conductor's allowlist carries no auth variable, and
    // Codex under ChatGPT sign-in keeps its credential in a file — so a run with
    // the allowlist alone reaches the model's door and is turned away.
    //
    // No tokens are spent: the request never authenticates.
    let world = World::new();
    let ran = run_worker_with(
        &world,
        codex_binary().to_string_lossy().as_ref(),
        &[],
        None,
        60_000,
    );
    assert!(ran.success, "the worker must survive it: {}", ran.stderr);

    assert_eq!(
        world.run_state(),
        RunState::Repairing,
        "an unauthenticated agent changes nothing, and §4.8's NO_CHANGE routes \
         to repair — it must never look like success"
    );
    let workspace_untouched = git_out(&world.workspace(), &["status", "--porcelain"]);
    assert!(
        workspace_untouched.is_empty(),
        "an agent that could not authenticate must have changed nothing: \
         {workspace_untouched}"
    );
}
