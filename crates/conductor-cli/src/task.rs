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
//! # What `task run` claims, and what it refuses to invent (rewritten at S12)
//!
//! Until S12 this command read the S5-era `.conductor/task.yaml` and wrote a
//! `plan_version` row of its own at `version 0`, `state DRAFT`, purely to satisfy
//! §5.1's non-null foreign key. The comment that stood here predicted its own
//! replacement by S11 and the replacement never happened, with consequences well
//! past cosmetic: §4.3's approval gate never ran on the product path, and
//! `task.declared_actions`, `task.depends_on`, `task.acceptance_criteria` and
//! `task.execution_requirements` — the four columns S11's materializer writes and
//! §4.2/§4.3's gates read — were `NULL` on every real run, so both gates compared
//! nothing and proceeded.
//!
//! Now this command **claims** a task and invents nothing. The task row and its
//! `plan_version` come from [`conductor_run::plan::materialize`], which runs when
//! a human approves a version at the control socket, and
//! [`conductor_run::plan::runnable`] is the single place that decides whether a
//! given task may run at all. What is left here is what a CLI owes: resolving the
//! repository, the adapter and the store, mapping refusals onto §7.2's codes, and
//! rendering the result.
//!
//! §3.4's `Conductor-Plan` trailer is emitted as a consequence — there is now a
//! real approved plan version to name.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Subcommand};
use conductor_core::{RunState, TaskId, TaskState};
use conductor_run::vertical::{VerticalConfig, VerticalOutcome, run_task};
use conductor_store::{NewRun, Store};
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
    /// The task, which an approved plan version must declare.
    pub task_id: String,
    /// The repository. Defaults to the working directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Which agent to run: `fake` or `codex` (§6.2). Overrides
    /// `.conductor/project.yaml`'s `adapter:` for this run only.
    ///
    /// **No default.** §3.1 makes the adapter one of the decisions a human
    /// makes once in `project.yaml`, so defaulting here would mean a project
    /// that declares `adapter: codex` and one that declares nothing behave
    /// identically — which is what this flag used to do.
    #[arg(long)]
    pub adapter: Option<String>,
    /// The agent binary. Required for `fake`; for `codex` it defaults to
    /// `codex` on `PATH`.
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
    /// The approved plan version this run is pinned to — acceptance row 21's
    /// evidence, and the answer to "what authorized this run?".
    plan_version: u32,
    /// That version's content hash, so a report can be checked against the
    /// document without trusting the version number alone.
    plan_hash: String,
    /// The task's state when the command returned.
    state: TaskState,
    /// Which agent ran, and which of §3.1's two doors named it.
    adapter: AdapterReport,
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

/// Which adapter ran, and where its name came from.
///
/// Reported because §3.1 gives the adapter two possible sources and an operator
/// looking at an unexpected agent needs to know which one won without
/// re-deriving the precedence by hand.
#[derive(Debug, Serialize)]
struct AdapterReport {
    /// `fake` or `codex`.
    id: String,
    /// `.conductor/project.yaml`, or `--adapter`.
    source: &'static str,
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

    let task_id = conductor_core::TaskId::new(&args.task_id)
        .map_err(|e| Failure::new(exit::USAGE, e.to_string()))?;

    let mut store = open_store(shared)?;
    // The *name* is checked before anything durable is written; the object is
    // built after the run exists, because a Codex adapter cannot be built
    // without the run's workspace path. See [`Adapter`].
    let (adapter_name, adapter_source) = resolve_adapter(args, &repo)?;
    let selected = Adapter::parse(&adapter_name)?;
    let state = conductor_state_dir(shared, &repo)?;

    // §3.2's authority, asked before anything durable is written: which approved
    // plan version says this task exists and may run. Every refusal it can
    // return is a refusal to *start*, so the store is left as it was.
    let runnable = conductor_run::plan::runnable::resolve(&store, &repo, &task_id)
        .map_err(runnable_failure)?;

    // §4.5's profile, loaded before the run exists rather than at the moment the
    // checks would run. A task naming a profile Conductor cannot read is a task
    // whose criteria are bound to nothing — and discovering that after an agent
    // has already edited the repository turns a configuration mistake into a
    // wasted attempt and a workspace to clean up.
    let profile_path = runnable.profile_path();
    conductor_run::verify::profile::load(&profile_path).map_err(|e| {
        Failure::new(
            exit::NOT_INITIALIZED,
            format!(
                "task {task_id} names verification_profile {:?}, and {}: {e}. \
                 §4.5's clarification 3 is settled as a path relative to the \
                 repository root; {} is where `conductor init` puts it",
                runnable.task.verification_profile,
                profile_path.display(),
                conductor_run::plan::VERIFICATION_CONFIG_PATH
            ),
        )
    })?;

    // Asked and discarded, deliberately: it refuses **here** if the approved
    // document no longer declares this task, which is a disagreement between the
    // store and the repository (§3.3 halts on those). What the task says is read
    // again by the packet builder, from the same document — this is the check, not
    // the data.
    runnable.declared().map_err(runnable_failure)?;

    let run_id = ensure_run(&mut store, &repo, &runnable).map_err(|e| {
        // A policy that cannot be read, a detached HEAD, or a store write that
        // failed. None of them is "a human must decide", so none of them is 3.
        Failure::new(exit::FAILURE, e)
    })?;

    let workspaces_root = state.join("workspaces");
    let artifacts_root = state.join("artifacts");
    let launch = build_launch(
        selected,
        args,
        // The path `run_one_attempt` will clone into. The adapter is given it up
        // front because §6.2's adapter normalises reported paths against it, and
        // `CodexAgent::command` refuses a workspace that disagrees — so a
        // mistake here is a refusal, not a wrong answer.
        &workspaces_root.join(&run_id),
        &artifacts_root.join(&run_id),
    )?;
    let adapter = launch.adapter;

    let config = VerticalConfig {
        task_id: task_id.clone(),
        worker_id: format!("cli-{}", std::process::id()),
        source_repo: repo.clone(),
        workspaces_root: workspaces_root.clone(),
        artifacts_root: artifacts_root.clone(),
        quarantine_root: state.join("quarantine"),
        profile_path,
        scratch_index: state.join("scratch").join("index"),
        supervisor: conductor_run::SupervisorConfig::default(),
        lease_ms: conductor_store::LEASE_MS,
        heartbeat_interval: Duration::from_secs(15),
        startup_grace: Duration::from_secs(30),
        sensitive: conductor_git::SensitivePatterns::default(),
        agent_env_extra: Default::default(),
        // §4.9's last clause, for an adapter whose credential is a directory.
        // The value cannot be computed here — it lives inside a workspace that
        // does not exist yet — so what travels is the request, and the worker
        // materialises it once the clone is on disk.
        credential_home: launch.credential_home,
        // §4.2's gate reads this to find the measurement for this adapter on
        // this machine. Built from the detected host rather than hardcoded, so
        // an OS or CLI upgrade changes the key and a stale measurement stops
        // being found — which is the whole reason the cache is keyed this way.
        probe_key: conductor_run::enforce::launch::probe_key_for(
            adapter.id(),
            &conductor_run::containment::probe::Host::detect(),
        ),
        // §6.5's packet is derived by the worker, from the approved plan this run
        // is pinned to, once the workspace exists. `Some` here would be this
        // command deciding what the agent is told, which is what it used to do
        // with the objective string — see ADR-0017.
        instructions: None,
    };

    let result = match run_task(&mut store, adapter.as_ref(), &config, &mut ()) {
        Ok(result) => result,
        // # The store outranks the return value (found at S12)
        //
        // Several refusals write their verdict durably and *then* return `Err`.
        // Acceptance row 30 is the clearest: `enforce::launch::gate` refuses,
        // the task and run go to `BLOCKED` with a `CRITICAL` finding naming the
        // dimension, and only after that does `run_task` error. Mapping every
        // error to §7.2's `1` therefore reported "generic failure" for a state
        // §7.2 gives its own code to — *"3 action required … ← scriptable 'human
        // needed'"* — and this module's own docs already said `BLOCKED` belongs
        // there. A wrapper script could not tell row 30 from a crash, and
        // `--json` printed nothing at all, so the finding the refusal exists to
        // leave behind was invisible at the boundary that produced it.
        //
        // So the persisted state decides, exactly as §4.8's doctrine says it
        // must: what is in the store is what happened, and a library return
        // value is a claim about it.
        Err(error) => {
            let report = stopped_report(
                &store,
                &task_id,
                &run_id,
                &runnable,
                AdapterReport {
                    id: adapter_name,
                    source: adapter_source.as_str(),
                },
                &error.to_string(),
            );
            let code = match report.state {
                TaskState::AwaitingReview | TaskState::AwaitingApproval | TaskState::Blocked => {
                    exit::ACTION_REQUIRED
                }
                _ => exit::FAILURE,
            };
            // The message still goes to stderr: an operator reading a terminal
            // must see why, and `--json` carries the same reason for a script.
            eprintln!("error: {error}");
            emit(shared, &report, || render_run(&report));
            return Ok(ExitCode::from(code));
        }
    };

    let task_state = store
        .task(&task_id)
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
        task: task_id.as_str().to_string(),
        run: run_id,
        plan_version: runnable.version(),
        plan_hash: runnable.plan_version.content_hash.clone(),
        state: task_state,
        adapter: AdapterReport {
            id: adapter_name,
            source: adapter_source.as_str(),
        },
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

/// What the store says happened, for a run the vertical could not finish.
///
/// Everything here is read from the store rather than inferred from the error,
/// because the point of the path this serves is that the refusal was *recorded*
/// before it was returned. The one thing that cannot be read is a reconciliation
/// verdict: a run that never launched an agent has nothing to reconcile, and
/// §4.8's "every exit from `RUNNING` passes through reconciliation" stays
/// literally true precisely because row 30's gate runs before the claim. So the
/// field says `NOT_RECONCILED` rather than borrowing a verdict from somewhere.
fn stopped_report(
    store: &Store,
    task_id: &TaskId,
    run_id: &str,
    runnable: &conductor_run::plan::Runnable,
    adapter: AdapterReport,
    reason: &str,
) -> RunReport {
    let state = store
        .task(task_id)
        .ok()
        .flatten()
        .map(|task| task.state)
        .unwrap_or(TaskState::Pending);
    let findings = conductor_core::RunId::new(run_id)
        .ok()
        .and_then(|id| store.findings_for_run(&id).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|f| format!("{}: {}", f.kind, f.evidence_ref))
        .collect();

    RunReport {
        task: task_id.as_str().to_string(),
        run: run_id.to_string(),
        plan_version: runnable.version(),
        plan_hash: runnable.plan_version.content_hash.clone(),
        state,
        adapter,
        outcome: "STOPPED",
        reason: Some(reason.to_string()),
        verdict: "NOT_RECONCILED".to_string(),
        commit: None,
        integrated: None,
        verification: stored_checks(store, run_id)
            .into_iter()
            .map(|check| CheckReport {
                check_id: check.check_id,
                outcome: check.outcome,
                tree_hash: check.tree_hash,
                from_cache: false,
            })
            .collect(),
        refusals: Vec::new(),
        deferred: Vec::new(),
        findings,
    }
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

/// The global and project policy documents that exist, resolved into one policy.
///
/// A file that is absent is not an error — an operator with no global policy has
/// no global policy. A file that is present and malformed *is* one, because
/// carrying on would run the task under a policy nobody wrote.
fn resolve_policy(repo: &Path) -> Result<conductor_run::policy::model::ResolvedPolicy, String> {
    use conductor_run::policy::load;
    use conductor_run::policy::model::Origin;

    let read = |path: Option<PathBuf>, origin: Origin| -> Result<_, String> {
        match path {
            Some(path) if path.exists() => load::load_document(&path, origin)
                .map(Some)
                .map_err(|e| e.to_string()),
            _ => Ok(None),
        }
    };

    load::resolve_documents(
        read(load::global_policy_path(), Origin::Global)?,
        read(Some(repo.join(load::PROJECT_POLICY_PATH)), Origin::Project)?,
        None,
    )
    .map_err(|e| e.to_string())
}

/// §7.2's code for a refusal to start, and the message a human reads.
///
/// The line §7.2 draws is between *"no project / not initialized / store
/// unhealthy"* (`2`) and *"the command ran and the answer was no"* (`1`), and
/// [`crate::plan`] and [`crate::recover`] both draw it the same way. A store this
/// machine has never registered the project in is `2` — the cure is `plan
/// approve` or `recover`, not a decision. A plan whose document no longer hashes
/// to what was approved is `1`: both halves were read and a verdict was reached,
/// and §3.3's *"execution halts — it is never resynced"* is a thing a human must
/// adjudicate.
fn runnable_failure(error: conductor_run::plan::RunnableError) -> Failure {
    use conductor_run::plan::LedgerError;
    use conductor_run::plan::runnable::RunnableError as R;

    let code = match &error {
        R::Project(_) | R::Store(_) | R::Io { .. } | R::UnregisteredProject { .. } => {
            exit::NOT_INITIALIZED
        }
        R::Ledger(ledger) => match ledger {
            LedgerError::Project(_)
            | LedgerError::Io { .. }
            | LedgerError::Git(_)
            | LedgerError::Store(_)
            | LedgerError::UnknownProject { .. } => exit::NOT_INITIALIZED,
            _ => exit::FAILURE,
        },
        R::Plan(_)
        | R::MalformedId { .. }
        | R::ForeignTree { .. }
        | R::NoSuchTask { .. }
        | R::NotAuthoritative { .. }
        | R::ForeignPlanVersion { .. }
        | R::Terminal { .. }
        | R::TaskNotInDocument { .. } => exit::FAILURE,
    };
    Failure::new(code, error.to_string())
}

/// Create the run row the vertical claims, if this task does not already have
/// one.
///
/// The task row is **not** created here, and that is the whole point of S12's
/// correction: a task exists because a human approved a plan that declares it
/// (§3.2, §5.2), and a command that could conjure one would be a second
/// authority for what work exists.
fn ensure_run(
    store: &mut Store,
    repo: &Path,
    runnable: &conductor_run::plan::Runnable,
) -> Result<String, String> {
    let now = now_ms();
    let task_id = runnable.task.id.clone();

    // §4.4: "At run creation Conductor canonically serializes the resolved
    // policy … and pins `policy_hash` on the run." Resolved from the global and
    // project files as they stand *now*; from here on the run is judged by this
    // snapshot and not by the files, which is what makes editing
    // `.conductor/policy.yaml` mid-run unable to change a running decision.
    //
    // A malformed policy file stops the run here rather than being ignored.
    // That is the fail-closed direction: a run under a policy Conductor could
    // not read is a run under no policy.
    let policy = resolve_policy(repo)?;
    let snapshot = conductor_run::policy::load::snapshot(&policy);
    let policy_hash = snapshot.hash.clone();
    conductor_run::policy::load::persist(store.conn_mut(), &snapshot, now)
        .map_err(|e| e.to_string())?;

    if let Some(existing) = runnable.active_run.as_ref() {
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
                task_id: task_id.clone(),
                policy_hash,
                base_commit,
                run_branch: format!("conductor/{task_id}/{run_id}"),
                target_branch,
            },
            now,
        )
        .map_err(|e| e.to_string())?;
    Ok(run_id.as_str().to_string())
}

/// Which agent `--adapter` names.
///
/// Naming and construction are two steps because they can happen at two
/// different times. §6.2's adapter normalises the paths an agent reports against
/// the run's workspace, so it cannot be built until the run id exists — but an
/// adapter name nobody recognises must be refused *before* a task row is written
/// on the strength of it. So the name is parsed first and
/// [`build_launch`] runs after [`ensure_task_and_run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Adapter {
    /// S3's scripted stand-in, which is not an agent.
    Fake,
    /// §6.2's Codex adapter — the first real one (S10).
    Codex,
}

/// Where the adapter name came from — reported, so an operator debugging the
/// wrong agent does not have to guess which of the two sources won.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdapterSource {
    /// §3.1's `.conductor/project.yaml`.
    ProjectConfig,
    /// An explicit `--adapter` on this invocation.
    CommandLine,
}

impl AdapterSource {
    fn as_str(self) -> &'static str {
        match self {
            AdapterSource::ProjectConfig => conductor_run::plan::PROJECT_CONFIG_PATH,
            AdapterSource::CommandLine => "--adapter",
        }
    }
}

/// Decide which adapter runs this task, and say where the answer came from.
///
/// §3.1 lists the adapter among the things `.conductor/project.yaml` is
/// authoritative for, so the file is the normal source and `--adapter` is an
/// explicit, single-run override. The precedence is stated rather than implied,
/// because two authorities that both claim to name the agent is exactly the
/// contradictory surface §7.1 warns against.
///
/// **Neither source is a refusal, never a default.** `plan::project::load`
/// already refuses to invent a default project, and its reasoning names this
/// field specifically: an invented adapter *"decides which agent runs"*. A
/// default here would silently run one agent in a repository that asked for
/// another — which is the behaviour this function replaced, where `--adapter`
/// defaulted to `fake`.
///
/// The refusal names both doors, because the operator who hits it has two valid
/// fixes and no way to guess the second from an error that mentions only the
/// first.
fn resolve_adapter(args: &RunArgs, repo: &Path) -> Result<(String, AdapterSource), Failure> {
    if let Some(name) = &args.adapter {
        return Ok((name.clone(), AdapterSource::CommandLine));
    }
    let config = conductor_run::plan::project::load(repo).map_err(|err| {
        Failure::new(
            exit::NOT_INITIALIZED,
            format!(
                "no adapter: {err}. §3.1 makes {} authoritative for which agent \
                 runs this project's tasks, and there is no default — declare \
                 `adapter:` there, or override it for this run with --adapter",
                conductor_run::plan::PROJECT_CONFIG_PATH
            ),
        )
    })?;
    Ok((config.adapter, AdapterSource::ProjectConfig))
}

impl Adapter {
    fn parse(name: &str) -> Result<Adapter, Failure> {
        match name {
            "fake" => Ok(Adapter::Fake),
            "codex" => Ok(Adapter::Codex),
            other => Err(Failure::new(
                exit::USAGE,
                format!("unknown adapter {other:?}; the adapters are \"fake\" and \"codex\""),
            )),
        }
    }
}

/// An adapter, and the credential directory it needs materialised inside the
/// run's workspace before it launches.
struct Launch {
    adapter: Box<dyn conductor_agent::AgentAdapter>,
    credential_home: Option<conductor_run::enforce::env::CredentialHomeRequest>,
}

/// Build the adapter, and say what it needs to authenticate.
///
/// # Why Codex asks for a directory and not a variable (§4.9, amended by S10)
///
/// §4.9's allowlist ends with "the adapter's own auth variable", which assumes
/// the credential is *carried* by a variable. S10 measured that it is not:
/// `~/.codex/auth.json` has `auth_mode: "chatgpt"` and a null `OPENAI_API_KEY`,
/// and what Codex reads is `CODEX_HOME` — a directory pointer.
///
/// Handing it the operator's real `~/.codex` is wrong twice over, and the second
/// is the serious one. It would give the agent `config.toml`, every profile and
/// the whole session history, none of which is a credential. And **Codex writes
/// into `CODEX_HOME`**: a contained run would leave session rollouts in the
/// operator's home, outside the workspace, outside §4.8's reconciled surface and
/// outside the per-run `TMPDIR` audit — a hole in containment shaped exactly
/// like its own foundation.
///
/// So what this returns is a *request*: the variable's name, the directory to
/// copy from, and the one file to copy. The worker turns it into a `0700`
/// directory inside the workspace holding a single `0600` file, git-excluded so
/// a credential cannot become a commit, dying with the workspace.
fn build_launch(
    selected: Adapter,
    args: &RunArgs,
    workspace: &Path,
    run_artifacts: &Path,
) -> Result<Launch, Failure> {
    match selected {
        Adapter::Fake => {
            let binary = args.agent_binary.clone().ok_or_else(|| {
                Failure::new(exit::USAGE, "--agent-binary is required for --adapter fake")
            })?;
            let scenario = args.scenario.clone().ok_or_else(|| {
                Failure::new(exit::USAGE, "--scenario is required for --adapter fake")
            })?;
            Ok(Launch {
                adapter: Box::new(conductor_agent::fake::FakeAgent::new(binary, scenario)),
                credential_home: None,
            })
        }
        Adapter::Codex => {
            // §6.1 keeps I/O out of adapters, so the caller writes the schema.
            // It goes in the **run's** artifact tree rather than the attempt's:
            // `CodexAgent::new` needs the path before an attempt ordinal exists,
            // and the content is identical for every attempt anyway. Same
            // reasoning, same location, as `conductor-s10-codex-worker`.
            let schema = run_artifacts.join("agent").join("report-schema.json");
            let parent = schema.parent().expect("a joined path has a parent");
            std::fs::create_dir_all(parent)
                .map_err(|e| Failure::new(exit::INTERNAL, format!("{}: {e}", parent.display())))?;
            std::fs::write(&schema, conductor_agent::codex::REPORT_SCHEMA_JSON)
                .map_err(|e| Failure::new(exit::INTERNAL, format!("{}: {e}", schema.display())))?;

            // A bare name, resolved by the child's own `PATH`, is what an
            // operator with `codex` installed expects. `--agent-binary` names a
            // different build without a second flag existing to be forgotten.
            let binary = args
                .agent_binary
                .clone()
                .unwrap_or_else(|| PathBuf::from("codex"));

            Ok(Launch {
                // No prompt: what the agent is told is §6.5's packet, and the
                // worker builds it once the workspace exists — see
                // `StartInput::instructions`. This used to pass the task's
                // objective, one line of prose where §6.5 specifies the
                // acceptance criteria, the scope, the referenced decisions, the
                // verification commands and the policy boundaries.
                adapter: Box::new(conductor_agent::codex::CodexAgent::new(
                    binary,
                    workspace.to_path_buf(),
                    schema,
                )),
                credential_home: Some(conductor_run::enforce::env::CredentialHomeRequest {
                    variable: "CODEX_HOME".to_string(),
                    source: operator_codex_home()?,
                    // Only the credential. `config.toml` and `sessions/` are the
                    // operator's, and naming them here is the only way they
                    // could ever travel.
                    files: vec!["auth.json".to_string()],
                }),
            })
        }
    }
}

/// The operator's own Codex directory — the one directory Conductor reads a
/// credential *out of*, and never one an agent is pointed *at*.
///
/// `CODEX_HOME` is consulted first because that is how Codex itself finds its
/// home: an operator who keeps it elsewhere would otherwise have Conductor read
/// a directory they abandoned, and fail with a missing `auth.json` in a place
/// they do not use.
///
/// An empty value is a refusal rather than a fallback. `CODEX_HOME=""` would
/// otherwise resolve to the process's working directory, and a credential home
/// materialised from whatever happens to be there is exactly the ambiguity
/// §4.9 exists to remove.
fn operator_codex_home() -> Result<PathBuf, Failure> {
    if let Some(explicit) = std::env::var_os("CODEX_HOME") {
        if explicit.is_empty() {
            return Err(Failure::new(
                exit::USAGE,
                "CODEX_HOME is set to an empty string; unset it to use ~/.codex, \
                 or set it to the directory holding auth.json",
            ));
        }
        return Ok(PathBuf::from(explicit));
    }
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => Ok(PathBuf::from(home).join(".codex")),
        _ => Err(Failure::new(
            exit::NOT_INITIALIZED,
            "neither CODEX_HOME nor HOME is set, so there is no directory to \
             take the agent's credential from",
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
    out.push_str(&format!(
        "  adapter         {} (from {})\n",
        report.adapter.id, report.adapter.source
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
