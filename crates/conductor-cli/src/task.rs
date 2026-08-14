//! `conductor task run | show | list` — master plan §7.1.
//!
//! > ```text
//! > conductor task list [--state …]
//! > conductor task run <task-id>        # claim → execute → verify → report   ← the core verb
//! > conductor task show <task-id>       # state, attempts, verification, findings, diff
//! > ```
//! > `--json` on every command.
//!
//! # Exit codes (§7.2)
//!
//! ```text
//! 0   success                                    the task reached COMPLETE
//! 1   generic failure
//! 2   no project / not initialized / store unhealthy
//! 3   action required — approval or review pending    ← scriptable "human needed"
//! 5   verification failed
//! 64  usage error
//! 70  internal error
//! ```
//!
//! §7.2 is explicit about why `3` exists: "'Conductor stopped and needs a human'
//! is the most common non-success outcome and must be distinguishable from
//! failure by a wrapper script." So `AWAITING_REVIEW`, `AWAITING_APPROVAL` and
//! `BLOCKED` all map to it — all three mean a person, and a script that has to
//! tell them apart can read `--json`. `REPAIRING` does **not**: §4.6 retries it
//! automatically once S6 exists, so calling it "a human is needed" would be
//! wrong the moment that lands.
//!
//! # What `task run` creates, and the one placeholder it writes
//!
//! Part 5.1 makes `task.plan_version_id` a non-null foreign key, and S5 has no
//! plan ledger. So `task run` writes a `plan_version` row standing for the
//! task-spec file, in **`DRAFT`** — never `APPROVED`. §5.2 gives `APPROVED` to
//! "a human at the control socket" and S8 owns that socket; writing `APPROVED`
//! here to satisfy a foreign key would be a lie in the one table whose whole
//! purpose is recording what was agreed. It is also why §3.4's `Conductor-Plan`
//! trailer is not emitted: there is no approved plan version to name.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Subcommand};
use conductor_core::{RunState, TaskId, TaskState};
use conductor_run::vertical::{VerticalConfig, VerticalOutcome, run_task};
use conductor_store::{NewRun, NewTask, Store};
use serde::Serialize;

use crate::exit;

/// `conductor task …`
#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Claim, execute, verify and report one task — the core verb.
    Run(RunArgs),
    /// State, attempts, verification, findings and diff for one task.
    Show(ShowArgs),
    /// Every task, optionally filtered by state.
    List(ListArgs),
}

/// Options shared by every `task` subcommand.
#[derive(Debug, Args)]
pub struct StoreArgs {
    /// Path to `conductor.db`. Defaults to §3.1's location.
    #[arg(long, global = true)]
    pub store: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long, global = true)]
    pub json: bool,
}

/// `conductor task run <task-id>`
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The task, which must match the spec's `id`.
    pub task_id: String,
    /// The repository. Defaults to the working directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// The task spec. Defaults to `<repo>/.conductor/task.yaml`.
    #[arg(long)]
    pub spec: Option<PathBuf>,
    /// Which agent to run. Only `fake` exists until S10.
    #[arg(long, default_value = "fake")]
    pub adapter: String,
    /// The agent binary, for the `fake` adapter.
    #[arg(long)]
    pub agent_binary: Option<PathBuf>,
    /// The scenario file, for the `fake` adapter.
    #[arg(long)]
    pub scenario: Option<PathBuf>,
}

/// `conductor task show <task-id>`
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// The task.
    pub task_id: String,
}

/// `conductor task list`
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Only tasks in this state.
    #[arg(long)]
    pub state: Option<String>,
}

/// What `task run` reports.
#[derive(Debug, Serialize)]
struct RunReport {
    task: String,
    run: String,
    /// The task's state when the command returned.
    state: TaskState,
    /// `COMPLETE` or `STOPPED`.
    outcome: &'static str,
    /// Why it stopped, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// §4.8's verdict.
    verdict: String,
    /// The Conductor-owned commit, when one was made.
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<CommitReport>,
    /// The ref update in the operator's repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    integrated: Option<IntegratedReport>,
    /// Every check that ran, and what it produced.
    verification: Vec<CheckReport>,
    /// Criteria the completion gate refused on.
    refusals: Vec<String>,
    /// §4.5's criteria a later slice owes.
    deferred: Vec<String>,
    /// Findings raised. They never auto-resolve (§4.8).
    findings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CommitReport {
    sha: String,
    tree: String,
}

#[derive(Debug, Serialize)]
struct IntegratedReport {
    reference: String,
    sha: String,
}

#[derive(Debug, Serialize)]
struct CheckReport {
    check_id: String,
    outcome: String,
    tree_hash: String,
    from_cache: bool,
}

/// What `task show` reports.
#[derive(Debug, Serialize)]
struct ShowReport {
    task: TaskSummary,
    run: Option<RunSummary>,
    attempts: Vec<AttemptSummary>,
    verification: Vec<StoredCheck>,
    findings: Vec<FindingSummary>,
    effects: Vec<EffectSummary>,
    /// The paths this run's workspace has changed, read from the workspace when
    /// it is still on disk. `[]` when it has been cleaned up — never a guess.
    diff: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TaskSummary {
    id: String,
    state: TaskState,
    slice_id: String,
    scope_globs: Vec<String>,
    verification_profile: String,
    attempt_budget: i64,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    id: String,
    state: RunState,
    base_commit: String,
    run_branch: String,
    target_branch: Option<String>,
    workspace_path: Option<String>,
}

#[derive(Debug, Serialize)]
struct AttemptSummary {
    id: String,
    ordinal: i64,
    state: String,
    outcome: Option<String>,
    exit_code: Option<i32>,
    signal: Option<i32>,
}

#[derive(Debug, Serialize)]
struct StoredCheck {
    check_id: String,
    outcome: String,
    tree_hash: String,
    exit_code: Option<i32>,
    duration_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
struct FindingSummary {
    id: String,
    kind: String,
    severity: String,
    evidence: String,
    resolution: Option<String>,
}

#[derive(Debug, Serialize)]
struct EffectSummary {
    operation_id: String,
    kind: String,
    state: String,
    receipt: Option<String>,
}

/// What `task list` reports.
#[derive(Debug, Serialize)]
struct ListReport {
    tasks: Vec<TaskSummary>,
}

/// Dispatch.
pub fn run(command: &TaskCommand, shared: &StoreArgs) -> ExitCode {
    match command {
        TaskCommand::Run(args) => match do_run(args, shared) {
            Ok(code) => code,
            Err(failure) => failure.report(),
        },
        TaskCommand::Show(args) => match do_show(args, shared) {
            Ok(code) => code,
            Err(failure) => failure.report(),
        },
        TaskCommand::List(args) => match do_list(args, shared) {
            Ok(code) => code,
            Err(failure) => failure.report(),
        },
    }
}

/// A command that could not produce a report, and the code it exits with.
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn new(code: u8, message: impl Into<String>) -> Failure {
        Failure {
            code,
            message: message.into(),
        }
    }

    fn report(self) -> ExitCode {
        eprintln!("error: {}", self.message);
        ExitCode::from(self.code)
    }
}

fn open_store(shared: &StoreArgs) -> Result<Store, Failure> {
    let path = match &shared.store {
        Some(path) => path.clone(),
        None => {
            Store::default_path().map_err(|e| Failure::new(exit::NOT_INITIALIZED, e.to_string()))?
        }
    };
    // §7.2's exit 2 is "no project / not initialized / **store unhealthy**", so
    // every way of failing to open belongs to it.
    Store::open_or_create(&path).map_err(|e| {
        Failure::new(
            exit::NOT_INITIALIZED,
            format!("cannot open the store at {}: {e}", path.display()),
        )
    })
}

fn do_run(args: &RunArgs, shared: &StoreArgs) -> Result<ExitCode, Failure> {
    let repo = match &args.repo {
        Some(repo) => repo.clone(),
        None => std::env::current_dir()
            .map_err(|e| Failure::new(exit::NOT_INITIALIZED, e.to_string()))?,
    };
    let repo = repo
        .canonicalize()
        .map_err(|e| Failure::new(exit::NOT_INITIALIZED, format!("{}: {e}", repo.display())))?;

    let spec_path = args
        .spec
        .clone()
        .unwrap_or_else(|| repo.join(conductor_run::spec::DEFAULT_SPEC_PATH));
    let (spec, spec_hash) = conductor_run::spec::load(&spec_path)
        .map_err(|e| Failure::new(exit::FAILURE, e.to_string()))?;

    if spec.id().as_str() != args.task_id {
        return Err(Failure::new(
            exit::USAGE,
            format!(
                "asked for task {} but {} defines {}",
                args.task_id,
                spec_path.display(),
                spec.id()
            ),
        ));
    }

    let mut store = open_store(shared)?;
    let adapter = build_adapter(args)?;
    let state = conductor_state_dir(shared, &repo)?;

    let run_id = ensure_task_and_run(&mut store, &repo, &spec, &spec_hash)
        .map_err(|e| Failure::new(exit::FAILURE, e))?;

    let config = VerticalConfig {
        task_id: spec.id().clone(),
        worker_id: format!("cli-{}", std::process::id()),
        source_repo: repo.clone(),
        workspaces_root: state.join("workspaces"),
        artifacts_root: state.join("artifacts"),
        quarantine_root: state.join("quarantine"),
        profile_path: repo.join(spec.verification_profile()),
        scratch_index: state.join("scratch").join("index"),
        supervisor: conductor_run::SupervisorConfig::default(),
        lease_ms: conductor_store::LEASE_MS,
        heartbeat_interval: Duration::from_secs(15),
        startup_grace: Duration::from_secs(30),
        sensitive: conductor_git::SensitivePatterns::default(),
        agent_env_extra: Default::default(),
    };

    let result = run_task(&mut store, adapter.as_ref(), &config, &mut ())
        .map_err(|e| Failure::new(exit::FAILURE, e.to_string()))?;

    let task_state = store
        .task(spec.id())
        .ok()
        .flatten()
        .map(|t| t.state)
        .unwrap_or(TaskState::Pending);
    let findings = store
        .findings_for_run(&result.run_id)
        .unwrap_or_default()
        .into_iter()
        .map(|f| format!("{}: {}", f.kind, f.evidence_ref))
        .collect();

    let verification: Vec<CheckReport> = result
        .verification
        .iter()
        .flat_map(|r| r.results.iter())
        .map(|r| CheckReport {
            check_id: r.check_id.clone(),
            outcome: r.outcome.as_str().to_string(),
            tree_hash: r.tree_hash.clone(),
            from_cache: r.from_cache,
        })
        .collect();

    let (outcome, reason, commit, integrated, deferred) = match &result.outcome {
        VerticalOutcome::Complete {
            commit,
            fetched,
            deferred,
            ..
        } => (
            "COMPLETE",
            None,
            Some(CommitReport {
                sha: commit.sha.clone(),
                tree: commit.tree.clone(),
            }),
            Some(IntegratedReport {
                reference: fetched.reference.clone(),
                sha: fetched.sha.clone(),
            }),
            deferred.iter().map(|c| format!("{c:?}")).collect(),
        ),
        VerticalOutcome::Stopped { reason, .. } => {
            ("STOPPED", Some(reason.clone()), None, None, Vec::new())
        }
    };

    let report = RunReport {
        task: spec.id().as_str().to_string(),
        run: run_id,
        state: task_state,
        outcome,
        reason,
        verdict: result.attempt.verdict.to_string(),
        commit,
        integrated,
        verification,
        refusals: result
            .refusals
            .iter()
            .map(|r| format!("{:?}: {}", r.criterion, r.detail))
            .collect(),
        deferred,
        findings,
    };

    let code = exit_code_for(&result, task_state);
    emit(shared, &report, || render_run(&report));
    Ok(ExitCode::from(code))
}

/// §7.2, applied to what the vertical produced.
fn exit_code_for(result: &conductor_run::vertical::Vertical, task_state: TaskState) -> u8 {
    match task_state {
        TaskState::Complete => exit::SUCCESS,
        // "3 action required — approval or review pending ← scriptable 'human
        // needed'". BLOCKED is here too: it is the state row 30 reaches, and it
        // needs a person just as much.
        TaskState::AwaitingReview | TaskState::AwaitingApproval | TaskState::Blocked => {
            exit::ACTION_REQUIRED
        }
        _ => {
            // A failing check is exit 5 whatever state the run ended in; the
            // check is the thing the operator has to look at.
            let failed = result.verification.as_ref().is_some_and(|report| {
                report
                    .results
                    .iter()
                    .any(|r| r.outcome == conductor_core::VerificationOutcome::Fail)
            });
            if failed {
                exit::VERIFICATION_FAILED
            } else {
                exit::FAILURE
            }
        }
    }
}

fn do_show(args: &ShowArgs, shared: &StoreArgs) -> Result<ExitCode, Failure> {
    let store = open_store(shared)?;
    let task_id =
        TaskId::new(&args.task_id).map_err(|e| Failure::new(exit::USAGE, e.to_string()))?;
    let task = store
        .task(&task_id)
        .map_err(|e| Failure::new(exit::NOT_INITIALIZED, e.to_string()))?
        .ok_or_else(|| Failure::new(exit::FAILURE, format!("no task {task_id}")))?;

    let run_id = store
        .active_run_for_task(&task_id)
        .ok()
        .flatten()
        .or_else(|| {
            // A completed run is no longer "active", and it is exactly the one a
            // human asks about after the fact.
            store
                .conn()
                .query_row(
                    "SELECT id FROM run WHERE task_id = ?1 ORDER BY created_at DESC LIMIT 1",
                    rusqlite::params![task_id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .and_then(|id| conductor_core::RunId::new(id).ok())
        });

    let run = run_id.as_ref().and_then(|id| store.run(id).ok().flatten());
    let attempts = run_id
        .as_ref()
        .and_then(|id| store.attempts_for_run(id).ok())
        .unwrap_or_default();
    let findings = run_id
        .as_ref()
        .and_then(|id| store.findings_for_run(id).ok())
        .unwrap_or_default();

    let verification = run_id
        .as_ref()
        .map(|id| stored_checks(&store, id.as_str()))
        .unwrap_or_default();
    let effects = run_id
        .as_ref()
        .map(|id| stored_effects(&store, id.as_str()))
        .unwrap_or_default();

    let diff = run
        .as_ref()
        .and_then(|r| r.workspace_path.as_ref())
        .map(|path| changed_paths(Path::new(path)))
        .unwrap_or_default();

    let report = ShowReport {
        task: summarise(&task),
        run: run.as_ref().map(|r| RunSummary {
            id: r.id.as_str().to_string(),
            state: r.state,
            base_commit: r.base_commit.clone(),
            run_branch: r.run_branch.clone(),
            target_branch: r.target_branch.clone(),
            workspace_path: r.workspace_path.clone(),
        }),
        attempts: attempts
            .iter()
            .map(|a| AttemptSummary {
                id: a.id.as_str().to_string(),
                ordinal: a.ordinal,
                state: a.state.to_string(),
                outcome: a.outcome.map(|o| o.as_str().to_string()),
                exit_code: a.exit_code,
                signal: a.signal,
            })
            .collect(),
        verification,
        findings: findings
            .iter()
            .map(|f| FindingSummary {
                id: f.id.clone(),
                kind: f.kind.clone(),
                severity: f.severity.clone(),
                evidence: f.evidence_ref.clone(),
                resolution: f.resolution.clone(),
            })
            .collect(),
        effects,
        diff,
    };

    emit(shared, &report, || render_show(&report));
    Ok(ExitCode::from(exit::SUCCESS))
}

fn do_list(args: &ListArgs, shared: &StoreArgs) -> Result<ExitCode, Failure> {
    let filter = match &args.state {
        // A typo'd state must not silently match nothing: "no such tasks" is a
        // different answer from "that is not a state", and a script cannot tell
        // them apart from an empty list.
        Some(name) => Some(name.parse::<TaskState>().map_err(|e| {
            Failure::new(
                exit::USAGE,
                format!(
                    "{e}; the states are {}",
                    TaskState::ALL
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })?),
        None => None,
    };

    let store = open_store(shared)?;
    let tasks = store
        .tasks(filter)
        .map_err(|e| Failure::new(exit::NOT_INITIALIZED, e.to_string()))?;
    let report = ListReport {
        tasks: tasks.iter().map(summarise).collect(),
    };
    emit(shared, &report, || render_list(&report));
    Ok(ExitCode::from(exit::SUCCESS))
}

fn summarise(task: &conductor_store::TaskRow) -> TaskSummary {
    TaskSummary {
        id: task.id.as_str().to_string(),
        state: task.state,
        slice_id: task.slice_id.clone(),
        scope_globs: task.scope_globs.clone(),
        verification_profile: task.verification_profile.clone(),
        attempt_budget: task.attempt_budget,
    }
}

fn stored_checks(store: &Store, run_id: &str) -> Vec<StoredCheck> {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT check_id, outcome, tree_hash, exit_code, duration_ms
           FROM verification_check WHERE run_id = ?1 ORDER BY rowid",
    ) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![run_id], |row| {
        Ok(StoredCheck {
            check_id: row.get(0)?,
            outcome: row.get(1)?,
            tree_hash: row.get(2)?,
            exit_code: row.get(3)?,
            duration_ms: row.get(4)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

fn stored_effects(store: &Store, run_id: &str) -> Vec<EffectSummary> {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT operation_id, kind, state, receipt FROM side_effect
          WHERE run_id = ?1 ORDER BY intended_at, operation_id",
    ) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![run_id], |row| {
        Ok(EffectSummary {
            operation_id: row.get(0)?,
            kind: row.get(1)?,
            state: row.get(2)?,
            receipt: row.get(3)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// The workspace's changed paths, from git. Empty when the workspace is gone —
/// which is a fact, not a guess.
fn changed_paths(workspace: &Path) -> Vec<String> {
    if !workspace.exists() {
        return Vec::new();
    }
    match conductor_git::run_git(workspace, &["status", "--porcelain", "-z"]) {
        Ok(out) if out.ok() => conductor_git::git::nul_records(&out.stdout)
            .into_iter()
            .filter_map(|record| record.get(3..).map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Create the task and run rows the vertical needs, if they are not there.
fn ensure_task_and_run(
    store: &mut Store,
    repo: &Path,
    spec: &conductor_core::task::ValidatedTaskSpec,
    spec_hash: &str,
) -> Result<String, String> {
    let now = now_ms();
    let project_id = format!(
        "p-{}",
        short(&conductor_core::effect::content_hash(
            repo.display().to_string().as_bytes(),
        ))
    );
    let plan_version_id = format!("pv-{}", short(spec_hash));
    let policy_blob = "{}";
    let policy_hash = conductor_core::effect::content_hash(policy_blob.as_bytes());

    conductor_store::with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT OR IGNORE INTO project
               (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                project_id,
                repo.display().to_string(),
                conductor_core::effect::content_hash(repo.display().to_string().as_bytes()),
                "main",
                spec_hash,
                now
            ],
        )?;
        // DRAFT, never APPROVED — see this module's docs. The row exists to
        // satisfy `task.plan_version_id`, and it says exactly what it is: the
        // task-spec file S11 will replace.
        tx.execute(
            "INSERT OR IGNORE INTO plan_version
               (id, project_id, version, content_hash, state, source_path)
             VALUES (?1, ?2, 0, ?3, 'DRAFT', ?4)",
            rusqlite::params![
                plan_version_id,
                project_id,
                spec_hash,
                conductor_run::spec::DEFAULT_SPEC_PATH
            ],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO policy_snapshot (hash, canonical_blob, created_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![policy_hash, policy_blob, now],
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())?;

    if store.task(spec.id()).map_err(|e| e.to_string())?.is_none() {
        store
            .create_task(
                &NewTask {
                    id: spec.id().clone(),
                    plan_version_id,
                    slice_id: "S5".to_string(),
                    scope_globs: spec.scope().to_vec(),
                    verification_profile: spec.verification_profile().to_string(),
                    attempt_budget: spec.attempt_budget(),
                },
                now,
            )
            .map_err(|e| e.to_string())?;
    }

    if let Some(existing) = store
        .active_run_for_task(spec.id())
        .map_err(|e| e.to_string())?
    {
        return Ok(existing.as_str().to_string());
    }

    // The integration target is the branch the operator is on, and the base is
    // where it points now. Both are recorded on the run so that a checkout the
    // operator makes mid-run cannot change where the work is destined (§4.1).
    let target_branch = git_line(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if target_branch == "HEAD" {
        return Err(
            "the repository has a detached HEAD, so there is no branch to \
                    integrate into"
                .to_string(),
        );
    }
    let base_commit = git_line(repo, &["rev-parse", "HEAD"])?;

    let ordinal: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) + 1 FROM run", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let run_id =
        conductor_core::RunId::new(format!("r-{ordinal:04}")).map_err(|e| e.to_string())?;

    store
        .create_run(
            &NewRun {
                id: run_id.clone(),
                task_id: spec.id().clone(),
                policy_hash,
                base_commit,
                run_branch: format!("conductor/{}/{run_id}", spec.id()),
                target_branch,
            },
            now,
        )
        .map_err(|e| e.to_string())?;
    Ok(run_id.as_str().to_string())
}

fn build_adapter(args: &RunArgs) -> Result<Box<dyn conductor_agent::AgentAdapter>, Failure> {
    match args.adapter.as_str() {
        "fake" => {
            let binary = args.agent_binary.clone().ok_or_else(|| {
                Failure::new(exit::USAGE, "--agent-binary is required for --adapter fake")
            })?;
            let scenario = args.scenario.clone().ok_or_else(|| {
                Failure::new(exit::USAGE, "--scenario is required for --adapter fake")
            })?;
            Ok(Box::new(conductor_agent::fake::FakeAgent::new(
                binary, scenario,
            )))
        }
        // §6.2's Codex adapter is S10. Naming it here would be an adapter that
        // does not exist.
        other => Err(Failure::new(
            exit::USAGE,
            format!("unknown adapter {other:?}; only \"fake\" exists before S10"),
        )),
    }
}

/// Where run workspaces and artifacts live (§3.1).
fn conductor_state_dir(shared: &StoreArgs, repo: &Path) -> Result<PathBuf, Failure> {
    // Beside the store, so that a `--store` pointing at a scratch directory
    // keeps its workspaces there too rather than in the operator's real one.
    match &shared.store {
        Some(path) => Ok(path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| repo.to_path_buf())),
        None => Store::default_path()
            .map(|p| p.parent().map(Path::to_path_buf).unwrap_or_default())
            .map_err(|e| Failure::new(exit::NOT_INITIALIZED, e.to_string())),
    }
}

fn git_line(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = conductor_git::run_git(repo, args).map_err(|e| e.to_string())?;
    if !out.ok() {
        return Err(format!("git {args:?} failed: {}", out.stderr));
    }
    Ok(out.stdout_trimmed())
}

fn short(hash: &str) -> String {
    hash.trim_start_matches("blake3:")
        .chars()
        .take(12)
        .collect()
}

fn emit<T: Serialize>(shared: &StoreArgs, report: &T, render: impl FnOnce() -> String) {
    if shared.json {
        match serde_json::to_string_pretty(report) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("error: {e}"),
        }
    } else {
        print!("{}", render());
    }
}

fn render_run(report: &RunReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "task {}  run {}  {}\n",
        report.task, report.run, report.state
    ));
    out.push_str(&format!("  reconciliation  {}\n", report.verdict));
    for check in &report.verification {
        out.push_str(&format!(
            "  check {:<16} {}{}\n",
            check.check_id,
            check.outcome,
            if check.from_cache { " (cached)" } else { "" }
        ));
    }
    if let Some(commit) = &report.commit {
        out.push_str(&format!("  commit          {}\n", commit.sha));
    }
    if let Some(integrated) = &report.integrated {
        out.push_str(&format!(
            "  integrated      {} -> {}\n",
            integrated.reference, integrated.sha
        ));
    }
    for refusal in &report.refusals {
        out.push_str(&format!("  refused         {refusal}\n"));
    }
    for finding in &report.findings {
        out.push_str(&format!("  finding         {finding}\n"));
    }
    if let Some(reason) = &report.reason {
        out.push_str(&format!("  stopped         {reason}\n"));
    }
    if !report.deferred.is_empty() {
        out.push_str(&format!(
            "  not evaluated   {} (a later slice owes these)\n",
            report.deferred.join(", ")
        ));
    }
    out
}

fn render_show(report: &ShowReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "task {}  {}\n  scope {:?}\n  profile {}\n  budget {}\n",
        report.task.id,
        report.task.state,
        report.task.scope_globs,
        report.task.verification_profile,
        report.task.attempt_budget
    ));
    if let Some(run) = &report.run {
        out.push_str(&format!(
            "run {}  {}\n  base {}\n  branch {} -> {}\n",
            run.id,
            run.state,
            run.base_commit,
            run.run_branch,
            run.target_branch.as_deref().unwrap_or("<none recorded>")
        ));
    }
    for attempt in &report.attempts {
        out.push_str(&format!(
            "  attempt {} #{} {} {}\n",
            attempt.id,
            attempt.ordinal,
            attempt.state,
            attempt.outcome.as_deref().unwrap_or("-")
        ));
    }
    for check in &report.verification {
        out.push_str(&format!(
            "  check {:<16} {} at {}\n",
            check.check_id, check.outcome, check.tree_hash
        ));
    }
    for effect in &report.effects {
        out.push_str(&format!("  effect {:<20} {}\n", effect.kind, effect.state));
    }
    for finding in &report.findings {
        out.push_str(&format!(
            "  finding {} [{}] {}\n",
            finding.kind, finding.severity, finding.evidence
        ));
    }
    for path in &report.diff {
        out.push_str(&format!("  changed {path}\n"));
    }
    out
}

fn render_list(report: &ListReport) -> String {
    let mut out = String::new();
    for task in &report.tasks {
        out.push_str(&format!("{:<12} {}\n", task.id, task.state));
    }
    if report.tasks.is_empty() {
        out.push_str("no tasks\n");
    }
    out
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
