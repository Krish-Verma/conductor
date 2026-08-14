//! One attempt, start to finish — the sequence startup recovery has to be able
//! to resume from any point in.
//!
//! The order is not arbitrary. Every write here is placed so that a crash
//! *immediately after it* leaves the database saying something true about the
//! world, because §4.7's recovery reads the database and then goes and looks:
//!
//! ```text
//! claim ─► workspace ─► baseline artifact ─► attempt CREATED ─► STARTING
//!       ─► spawn ─► ACTIVE(pid, start-time) ─► supervise ─► terminal outcome
//!       ─► RECONCILING ─► observe+reconcile ─► attempt RECONCILED ─► route
//! ```
//!
//! **`RunPoint` names the places a crash matters**, and the sequence calls
//! [`RunObserver::at`] at each of them. In production the observer does
//! nothing. In `tests/crash_matrix.rs` it sends this process `SIGKILL`. That is
//! why the points are here rather than in a test-only copy of the sequence: a
//! crash matrix that exercises a parallel implementation proves something about
//! the parallel implementation.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use conductor_agent::{AgentAdapter, RunOutputs, StartInput};
use conductor_core::attempt::TerminalPhase;
use conductor_core::effect::{
    OperationId, Precondition, SideEffectKind, SideEffectState, content_hash,
};
use conductor_core::{AgentReport, AttemptId, Fence, ReconciledRoute, RunId};
use conductor_git::{Baseline, Reconciliation, Scope, SensitivePatterns, Verdict, observe};
use conductor_store::{NewAttempt, Store, StoreError};

use crate::paths::{ArtifactRoot, Owner, OwnershipError};
use crate::supervise::{SupervisionEnd, SupervisorConfig, spawn};

/// The places a crash changes what recovery must do.
///
/// Chosen so that each one sits in a different gap between a durable write and
/// the world changing — the gaps are the only places a crash can produce a
/// disagreement between the two.
///
/// The slice asks for twelve. There are **thirteen**, because the twelve chosen
/// first had no point between `git clone` returning and the store being told,
/// and that gap held a real bug: the run was stranded permanently. The count is
/// not the property; covering every gap is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RunPoint {
    /// Owned the run; nothing else exists yet.
    AfterClaim,
    /// The clone exists on disk and **the store has not been told**.
    ///
    /// The thirteenth point, added by S3 after the twelve missed a real
    /// convergence bug: `create_workspace` refuses an existing path and every
    /// attempt of a run derives the same path, so a crash in this gap stranded
    /// the run permanently. A set of kill points that omits the one gap where a
    /// bug lives reads as coverage without being it.
    AfterWorkspaceCloned,
    /// The workspace exists and is recorded.
    AfterWorkspaceReady,
    /// `side_effect` says `INTENDED`; the artifact is not written (row 22).
    AfterBaselineIntended,
    /// The artifact is written; the receipt is not recorded (row 22).
    AfterBaselineWritten,
    /// The attempt row exists in `CREATED`.
    AfterAttemptCreated,
    /// About to spawn; the attempt says `STARTING`.
    BeforeSpawn,
    /// A process exists and **its pid has not been recorded**. The worst case:
    /// an agent nobody can probe.
    AfterSpawnBeforePid,
    /// The pid and start time are durable; the attempt says `ACTIVE`.
    AfterPidRecorded,
    /// The agent is running and the supervisor is heartbeating.
    DuringActive,
    /// The terminal outcome is durable; the run has not moved yet.
    AfterOutcomeRecorded,
    /// The run says `RECONCILING`; the repository has not been read.
    AfterReconciling,
    /// The run has been routed; the lease has not been released.
    AfterRoute,
}

impl RunPoint {
    /// Every point, in the order the sequence reaches them.
    pub const ALL: &'static [RunPoint] = &[
        RunPoint::AfterClaim,
        RunPoint::AfterWorkspaceCloned,
        RunPoint::AfterWorkspaceReady,
        RunPoint::AfterBaselineIntended,
        RunPoint::AfterBaselineWritten,
        RunPoint::AfterAttemptCreated,
        RunPoint::BeforeSpawn,
        RunPoint::AfterSpawnBeforePid,
        RunPoint::AfterPidRecorded,
        RunPoint::DuringActive,
        RunPoint::AfterOutcomeRecorded,
        RunPoint::AfterReconciling,
        RunPoint::AfterRoute,
    ];

    /// The name used on the command line and in reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            RunPoint::AfterClaim => "after-claim",
            RunPoint::AfterWorkspaceCloned => "after-workspace-cloned",
            RunPoint::AfterWorkspaceReady => "after-workspace-ready",
            RunPoint::AfterBaselineIntended => "after-baseline-intended",
            RunPoint::AfterBaselineWritten => "after-baseline-written",
            RunPoint::AfterAttemptCreated => "after-attempt-created",
            RunPoint::BeforeSpawn => "before-spawn",
            RunPoint::AfterSpawnBeforePid => "after-spawn-before-pid",
            RunPoint::AfterPidRecorded => "after-pid-recorded",
            RunPoint::DuringActive => "during-active",
            RunPoint::AfterOutcomeRecorded => "after-outcome-recorded",
            RunPoint::AfterReconciling => "after-reconciling",
            RunPoint::AfterRoute => "after-route",
        }
    }

    /// Parse a point by name.
    pub fn parse(name: &str) -> Option<RunPoint> {
        RunPoint::ALL.iter().copied().find(|p| p.as_str() == name)
    }
}

/// Notified as the sequence passes each [`RunPoint`].
pub trait RunObserver {
    /// The sequence has reached `point`.
    fn at(&mut self, point: RunPoint);
}

/// The production observer: the sequence is not observed at all.
impl RunObserver for () {
    fn at(&mut self, _point: RunPoint) {}
}

/// What a worker needs to run one attempt.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// This worker's identity — becomes `run.lease_owner` and path provenance.
    pub worker_id: String,
    /// Where run workspaces live.
    pub workspaces_root: PathBuf,
    /// Where artifacts live (§3.1).
    pub artifacts_root: PathBuf,
    /// The operator's repository, cloned per run.
    pub source_repo: PathBuf,
    /// Supervision budgets.
    pub supervisor: SupervisorConfig,
    /// Lease duration, milliseconds. §4.7: 60 s.
    pub lease_ms: i64,
    /// Heartbeat interval. §4.7: 15 s.
    pub heartbeat_interval: Duration,
    /// The task's declared scope (§4.8).
    pub scope: Scope,
    /// Which paths require policy evaluation (§4.8).
    pub sensitive: SensitivePatterns,
    /// Extra environment variables the agent is given.
    ///
    /// §4.9's allowlist is "`PATH`, redirected `HOME`, `LANG`, `TERM`, **the
    /// adapter's own auth variable**, nothing else". This is that last clause:
    /// a place for the one or two variables a specific adapter needs, added
    /// explicitly by name. It is still a build, not a filter — nothing arrives
    /// here by inheritance.
    pub agent_env_extra: std::collections::BTreeMap<String, String>,
}

/// What running one attempt produced.
#[derive(Debug, Clone)]
pub struct AttemptOutcomeRecord {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// How the process ended.
    pub end: SupervisionEnd,
    /// The persisted terminal state.
    pub attempt_state: conductor_core::AttemptState,
    /// The §4.8 verdict.
    pub verdict: Verdict,
    /// Where the run was routed.
    pub route: ReconciledRoute,
    /// Findings raised during this attempt.
    pub findings: Vec<String>,
}

/// Anything running an attempt can fail with.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The store said no.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// Git said no.
    #[error("git: {0}")]
    Git(#[from] conductor_git::GitError),
    /// A generated path is owned by somebody else.
    #[error("ownership: {0}")]
    Ownership(#[from] OwnershipError),
    /// The agent could not be spawned.
    #[error("spawning the agent failed: {0}")]
    Spawn(String),
    /// The adapter could not build a command.
    #[error("adapter: {0}")]
    Adapter(String),
    /// This worker lost its lease mid-attempt (acceptance row 27).
    #[error("fenced out mid-attempt: the lease moved to another worker")]
    FencedOut,
    /// A side effect could not be decided (§4.7). The run halts.
    #[error("side effect {operation_id} is ambiguous: {detail}")]
    AmbiguousEffect {
        /// The operation.
        operation_id: String,
        /// Why it could not be decided.
        detail: String,
    },
}

/// Run one attempt end to end.
///
/// The caller has already claimed the run and holds `fence`. Everything from
/// here is fenced, ordered and observable.
pub fn run_one_attempt(
    store: &mut Store,
    fence: &Fence,
    adapter: &dyn AgentAdapter,
    config: &WorkerConfig,
    observer: &mut dyn RunObserver,
) -> Result<AttemptOutcomeRecord, WorkerError> {
    let run_id = fence.run_id().clone();
    let owner = Owner::new(config.worker_id.clone(), std::process::id() as i32);
    let artifacts = ArtifactRoot::new(&config.artifacts_root);
    let mut findings: Vec<String> = Vec::new();

    observer.at(RunPoint::AfterClaim);

    // ---- workspace -------------------------------------------------------
    let (workspace_path, baseline) = ensure_workspace(store, fence, config, observer)?;
    observer.at(RunPoint::AfterWorkspaceReady);

    // ---- the attempt row -------------------------------------------------
    let ordinal = store.next_attempt_ordinal(&run_id)?;
    let attempt_id = AttemptId::new(format!("{}-a{ordinal}", run_id.as_str()))
        .map_err(|e| WorkerError::Adapter(e.to_string()))?;

    // The attempt's artifact directory, claimed exclusively. A second worker
    // that believes it is this attempt is refused here rather than silently
    // sharing the directory (S0's carried-forward lesson).
    let owned = artifacts.reclaim_attempt_dir(&run_id, ordinal, &owner)?;

    // ---- the baseline, through the side-effect ledger --------------------
    persist_baseline(store, fence, &owned, &baseline, ordinal, observer)?;

    let attempt = store.create_attempt(
        fence,
        NewAttempt {
            id: attempt_id.clone(),
            ordinal,
            kind: "IMPLEMENT".to_string(),
            adapter: adapter.id().to_string(),
            launcher: "none".to_string(),
            caps_snapshot: "{}".to_string(),
            agent_session_id: None,
        },
        now_ms(),
    )?;
    observer.at(RunPoint::AfterAttemptCreated);

    let attempt = attempt.starting();
    store.record_attempt_starting(fence, &attempt, now_ms())?;
    observer.at(RunPoint::BeforeSpawn);

    // ---- spawn and supervise --------------------------------------------
    let report_path = owned.path().join("report.json");
    let start = StartInput {
        run_id: run_id.clone(),
        task_id: TaskIdShim::from_store(store, &run_id)?,
        attempt_ordinal: ordinal,
        workspace: workspace_path.clone(),
        report_path: report_path.clone(),
        session_id: None,
        env: {
            let mut env = agent_environment(&workspace_path);
            env.extend(config.agent_env_extra.clone());
            env
        },
    };
    let command = adapter
        .command(&start)
        .map_err(|e| WorkerError::Adapter(e.to_string()))?;

    let spawned = match spawn(&command) {
        Ok(spawned) => spawned,
        Err(error) => {
            // Nothing ran. §4.7 calls this infrastructure; §5.2 forbids calling
            // it CRASHED, because no process ever existed to crash.
            let stale = attempt.spawn_failed(error.to_string());
            store.record_attempt_terminal(fence, &stale, now_ms())?;
            return finish_without_agent(
                store,
                fence,
                &stale,
                config,
                &workspace_path,
                &baseline,
                findings,
                observer,
            );
        }
    };
    observer.at(RunPoint::AfterSpawnBeforePid);

    let attempt = attempt.active(spawned.pid(), spawned.pid_start_time());
    store.record_attempt_active(fence, &attempt, now_ms())?;
    observer.at(RunPoint::AfterPidRecorded);

    let mut last_beat: Option<Instant> = None;
    let mut fenced_out = false;
    let mut beats = 0usize;
    let mut ticked_observer = false;
    let supervised = {
        // The store is borrowed by the callback, so the supervision happens in
        // its own scope and the borrow ends before anything else touches it.
        let store_ref = &mut *store;
        let heartbeat_interval = config.heartbeat_interval;
        let lease_ms = config.lease_ms;
        spawned.supervise(adapter, &config.supervisor, |beat| {
            if !ticked_observer {
                ticked_observer = true;
                observer.at(RunPoint::DuringActive);
            }
            if let Some(alive) = beat.alive {
                match crate::lease::heartbeat(
                    store_ref,
                    fence,
                    alive,
                    &mut last_beat,
                    heartbeat_interval,
                    now_ms(),
                    lease_ms,
                ) {
                    Ok(crate::lease::HeartbeatOutcome::Renewed { .. }) => beats += 1,
                    Ok(crate::lease::HeartbeatOutcome::NotDue) => {}
                    Ok(crate::lease::HeartbeatOutcome::FencedOut) => fenced_out = true,
                    Err(_) => fenced_out = true,
                }
            }
        })
    };
    let _ = beats;

    if fenced_out {
        return Err(WorkerError::FencedOut);
    }

    // ---- classify (§6.4) -------------------------------------------------
    let terminal = classify(attempt, &supervised);
    store.record_attempt_terminal(fence, &terminal, now_ms())?;
    observer.at(RunPoint::AfterOutcomeRecorded);

    // ---- leave RUNNING, which can only mean RECONCILING (§4.8) ----------
    store.advance_to_reconciling(fence, &terminal.evidence(), now_ms())?;
    observer.at(RunPoint::AfterReconciling);

    // ---- read the report, then the repository ---------------------------
    let report_json = std::fs::read_to_string(&report_path).ok();
    let outputs = RunOutputs {
        stdout_lines: supervised.stdout_lines.clone(),
        report_json,
    };
    let report: Option<AgentReport> = match adapter.extract_report(&outputs) {
        Ok(report) => report,
        Err(error) => {
            // Acceptance row 5. The report is unusable; the run still completes
            // on the repository's evidence, and the finding stays.
            findings.push(raise(
                store,
                fence,
                &run_id,
                "REPORT_UNPARSEABLE",
                "WARNING",
                &error.to_string(),
            )?);
            None
        }
    };

    for line in &supervised.parse_errors {
        findings.push(raise(
            store,
            fence,
            &run_id,
            "AGENT_OUTPUT_UNPARSEABLE",
            "INFO",
            line,
        )?);
    }
    for event in &supervised.events {
        if let conductor_agent::AgentEvent::ControlSocketAttempt { path, connected } = event {
            // Acceptance row 28. Under an unsandboxed launcher the connection
            // succeeds and noticing is all Conductor can do — so it notices, and
            // the finding never auto-resolves.
            findings.push(raise(
                store,
                fence,
                &run_id,
                "CONTROL_SOCKET_ATTEMPT",
                "CRITICAL",
                &format!("the agent connected={connected} to {path}"),
            )?);
        }
    }

    let observed = observe(&workspace_path, &baseline)?;
    let reconciliation = conductor_git::reconcile(
        &baseline,
        &observed,
        &config.scope,
        &config.sensitive,
        report.as_ref(),
        None,
    );

    for finding in reconciliation.findings() {
        findings.push(raise(
            store,
            fence,
            &run_id,
            &format!("{:?}", finding.kind).to_uppercase(),
            "WARNING",
            &finding.detail,
        )?);
    }

    // ---- the attempt is finished only now (§5.2) ------------------------
    let attempt_state = terminal.state();
    let reconciled = terminal.reconciled();
    store.record_attempt_reconciled(fence, &reconciled, now_ms())?;

    let route = route_for(&reconciliation);
    store.route_reconciled(
        fence,
        route,
        &format!("verdict={}", reconciliation.verdict),
        now_ms(),
    )?;
    observer.at(RunPoint::AfterRoute);

    store.release_lease(fence, now_ms())?;

    Ok(AttemptOutcomeRecord {
        attempt_id,
        end: supervised.end,
        attempt_state,
        verdict: reconciliation.verdict,
        route,
        findings,
    })
}

/// §4.8's verdict → §5.2's next state.
///
/// `COMPLETE` is not reachable and cannot be named: [`ReconciledRoute`] has no
/// such variant, because §5.2 forbids any `→ COMPLETE` without verification
/// bound to the final tree hash and verification is S4.
pub fn route_for(reconciliation: &Reconciliation) -> ReconciledRoute {
    match reconciliation.verdict {
        Verdict::Corrupt => ReconciledRoute::Blocked,
        Verdict::Contradicted => ReconciledRoute::AwaitingReview,
        Verdict::PolicySensitive => ReconciledRoute::AwaitingApproval,
        Verdict::OutOfScope => ReconciledRoute::AwaitingReview,
        // The attempt did nothing. §4.8: "attempt failed to act → repair or
        // review". Budget accounting is S6's; S3 routes to repair and leaves the
        // decision about how many repairs are allowed to the slice that owns it.
        Verdict::NoChange => ReconciledRoute::Repairing,
        Verdict::CleanComplete | Verdict::CleanNoReport => ReconciledRoute::Verifying,
    }
}

/// §6.4's table, applied to what the supervisor observed.
fn classify(
    attempt: conductor_core::Attempt<conductor_core::attempt::phase::Active>,
    supervised: &crate::supervise::Supervised,
) -> TerminalPhase {
    match &supervised.end {
        SupervisionEnd::Exited { code } => attempt.exited(*code),
        SupervisionEnd::Signalled { signal } => attempt.signalled(*signal),
        SupervisionEnd::TimedOut { reason } => match *reason {
            conductor_core::attempt::TIMEOUT_STALL => attempt.timed_out_idle(),
            conductor_core::attempt::TIMEOUT_NO_STARTUP => attempt.timed_out_startup(),
            _ => attempt.timed_out_wall(),
        },
        // The process is gone and no status was obtainable. §5.2: unknown must
        // not be recorded as known.
        SupervisionEnd::Vanished => attempt.stale(),
    }
}

fn ensure_workspace(
    store: &mut Store,
    fence: &Fence,
    config: &WorkerConfig,
    observer: &mut dyn RunObserver,
) -> Result<(PathBuf, Baseline), WorkerError> {
    let run_id = fence.run_id().clone();
    let row = store
        .run(&run_id)?
        .ok_or_else(|| WorkerError::Adapter(format!("run {run_id} vanished")))?;

    if let Some(path) = row.workspace_path.clone() {
        let path = PathBuf::from(path);
        let baseline = conductor_git::capture_baseline(&path)?;
        return Ok((path, baseline));
    }

    let target = config.workspaces_root.join(run_id.as_str());

    // A workspace on disk that the store has never heard of is the crash window
    // between `git clone` returning and `attach_workspace` committing. The clone
    // is the only durable record of it, and §4.1 makes that deliberate: the
    // descriptor exists so a workspace can be identified "with **no database at
    // all**".
    //
    // Adopting it is not a convenience. `create_workspace` refuses an existing
    // path, and every attempt of this run derives the same path — so without
    // this the run is stranded permanently after one badly timed crash, which is
    // the opposite of §4.7's "converges with no human input".
    //
    // Adoption is allowed **only** when the descriptor names this run. A
    // directory belonging to somebody else, or one that cannot say who it
    // belongs to, is left exactly where it is: §4.1's rule for a workspace
    // Conductor cannot account for is to preserve it, never to write into it.
    if target.exists() {
        let descriptor = conductor_git::read_descriptor(&target)?;
        if descriptor.run_id != run_id {
            return Err(WorkerError::Adapter(format!(
                "{} belongs to run {}, not to {run_id}; it is not this run's to take",
                target.display(),
                descriptor.run_id
            )));
        }
        let baseline = conductor_git::capture_baseline(&target)?;
        store.attach_workspace(
            fence,
            &format!("ws-{}", run_id.as_str()),
            &target.to_string_lossy(),
            &config.source_repo.to_string_lossy(),
            now_ms(),
        )?;
        return Ok((target, baseline));
    }

    let workspace = conductor_git::create_workspace(&conductor_git::WorkspaceRequest {
        source: config.source_repo.clone(),
        workspace: target,
        run_id: run_id.clone(),
        task_id: conductor_core::TaskId::new(row.task_id.clone())
            .map_err(|e| WorkerError::Adapter(e.to_string()))?,
        base_commit: row.base_commit.clone(),
        policy_hash: conductor_core::PolicyHash::new(policy_hash_of(store, &run_id)?)
            .map_err(|e| WorkerError::Adapter(e.to_string()))?,
    })?;

    // The clone is on disk and nothing durable knows. Everything between here
    // and `attach_workspace` is the window the adoption path above exists for.
    observer.at(RunPoint::AfterWorkspaceCloned);

    store.attach_workspace(
        fence,
        &format!("ws-{}", run_id.as_str()),
        &workspace.path.to_string_lossy(),
        &config.source_repo.to_string_lossy(),
        now_ms(),
    )?;

    Ok((workspace.path, workspace.baseline))
}

/// Write the baseline as an artifact, through §4.7's intent/receipt ledger.
///
/// The baseline is what reconciliation compares against, so it has to survive
/// the crash of the worker that captured it. That makes it exactly the kind of
/// effect the ledger exists for, and the crash window between `INTENDED` and
/// `CONFIRMED` is acceptance row 22 in miniature.
fn persist_baseline(
    store: &mut Store,
    fence: &Fence,
    owned: &crate::paths::OwnedDir,
    baseline: &Baseline,
    ordinal: i64,
    observer: &mut dyn RunObserver,
) -> Result<(), WorkerError> {
    let bytes =
        serde_json::to_vec_pretty(baseline).map_err(|e| WorkerError::Adapter(e.to_string()))?;
    let hash = content_hash(&bytes);
    let path = owned.path().join("baseline.json");
    let operation_id = OperationId::compute(
        SideEffectKind::ArtifactWrite,
        fence.run_id(),
        ordinal,
        &baseline.tree_hash,
    );
    let precondition = Precondition::FileWithHash {
        path: path.to_string_lossy().to_string(),
        content_hash: hash.clone(),
    };

    let state = store.intend_effect(
        fence,
        &operation_id,
        SideEffectKind::ArtifactWrite,
        &precondition,
        now_ms(),
    )?;
    observer.at(RunPoint::AfterBaselineIntended);

    match state {
        SideEffectState::Confirmed => return Ok(()),
        SideEffectState::Ambiguous => {
            return Err(WorkerError::AmbiguousEffect {
                operation_id: operation_id.to_string(),
                detail: "a previous attempt left this effect undecided".to_string(),
            });
        }
        SideEffectState::Intended | SideEffectState::Failed => {}
    }

    // Re-checking the precondition rather than blindly writing: a restart that
    // found the row already `INTENDED` is in exactly the crash window row 22
    // describes, and §4.7 resolves it by asking the world.
    if !precondition_holds(&precondition) {
        match owned.write_new("baseline.json", &bytes) {
            Ok(_) => {}
            Err(OwnershipError::AlreadyExists(_)) => {
                // The file is there but did not match the hash. Conductor cannot
                // tell whether it wrote a different baseline or somebody else
                // did. §4.7: mark AMBIGUOUS, halt, ask a human. **Never guess.**
                store.mark_effect_ambiguous(
                    fence,
                    &operation_id,
                    "baseline.json exists with unexpected content",
                    now_ms(),
                )?;
                return Err(WorkerError::AmbiguousEffect {
                    operation_id: operation_id.to_string(),
                    detail: "baseline.json exists with unexpected content".to_string(),
                });
            }
            Err(other) => return Err(other.into()),
        }
    }
    observer.at(RunPoint::AfterBaselineWritten);

    store.confirm_effect(
        fence,
        &operation_id,
        &format!("{{\"path\":{:?},\"content_hash\":{:?}}}", path, hash),
        now_ms(),
    )?;
    Ok(())
}

/// Ask the world whether the effect already happened — §4.7.
pub fn precondition_holds(precondition: &Precondition) -> bool {
    match precondition {
        Precondition::FileWithHash {
            path,
            content_hash: expected,
        } => match std::fs::read(path) {
            Ok(bytes) => &content_hash(&bytes) == expected,
            Err(_) => false,
        },
        Precondition::WorkspaceAtHead { path, head } => {
            let workspace = std::path::Path::new(path);
            if !workspace.exists() {
                return false;
            }
            match conductor_git::run_git(workspace, &["rev-parse", "HEAD"]) {
                Ok(output) if output.ok() => output.stdout_trimmed() == *head,
                _ => false,
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_without_agent(
    store: &mut Store,
    fence: &Fence,
    terminal: &TerminalPhase,
    config: &WorkerConfig,
    workspace_path: &std::path::Path,
    baseline: &Baseline,
    mut findings: Vec<String>,
    observer: &mut dyn RunObserver,
) -> Result<AttemptOutcomeRecord, WorkerError> {
    store.advance_to_reconciling(fence, &terminal.evidence(), now_ms())?;
    observer.at(RunPoint::AfterReconciling);

    let observed = observe(workspace_path, baseline)?;
    let reconciliation = conductor_git::reconcile(
        baseline,
        &observed,
        &config.scope,
        &config.sensitive,
        None,
        None,
    );
    for finding in reconciliation.findings() {
        findings.push(raise(
            store,
            fence,
            fence.run_id(),
            &format!("{:?}", finding.kind).to_uppercase(),
            "WARNING",
            &finding.detail,
        )?);
    }

    let attempt_state = terminal.state();
    let attempt_id = terminal.id().clone();
    let reconciled = terminal.clone().reconciled();
    store.record_attempt_reconciled(fence, &reconciled, now_ms())?;

    let route = route_for(&reconciliation);
    store.route_reconciled(fence, route, "spawn failed", now_ms())?;
    observer.at(RunPoint::AfterRoute);
    store.release_lease(fence, now_ms())?;

    Ok(AttemptOutcomeRecord {
        attempt_id,
        end: SupervisionEnd::Vanished,
        attempt_state,
        verdict: reconciliation.verdict,
        route,
        findings,
    })
}

fn raise(
    store: &mut Store,
    fence: &Fence,
    run_id: &RunId,
    kind: &str,
    severity: &str,
    evidence: &str,
) -> Result<String, WorkerError> {
    let id = format!("f-{}-{}-{}", run_id.as_str(), kind, blake3_short(evidence));
    store.record_finding(fence, &id, kind, severity, evidence, now_ms())?;
    Ok(id)
}

fn blake3_short(text: &str) -> String {
    content_hash(text.as_bytes())
        .trim_start_matches("blake3:")
        .chars()
        .take(12)
        .collect()
}

/// §4.9's allowlisted environment.
///
/// Not a denylist. `PATH` because agents run tools; a per-run `HOME` and
/// `TMPDIR` inside the workspace so `~/.aws`, `~/.config/gh` and ordinary temp
/// usage are simply absent rather than merely discouraged (M7). No
/// `SSH_AUTH_SOCK`, no `GH_TOKEN`, no cloud variables — and no way for one to
/// arrive later, because the map is built rather than filtered.
pub fn agent_environment(
    workspace: &std::path::Path,
) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }
    env.insert(
        "HOME".to_string(),
        workspace.join(".conductor-home").display().to_string(),
    );
    env.insert(
        "TMPDIR".to_string(),
        workspace.join(".conductor-tmp").display().to_string(),
    );
    env.insert("LANG".to_string(), "C".to_string());
    env.insert("TERM".to_string(), "dumb".to_string());
    // §4.9: git must never be able to prompt for, or find, a credential.
    env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    env.insert("GIT_ASKPASS".to_string(), "/bin/false".to_string());
    env
}

fn policy_hash_of(store: &Store, run_id: &RunId) -> Result<String, WorkerError> {
    let hash: String = store
        .conn()
        .query_row(
            "SELECT policy_hash FROM run WHERE id = ?1",
            rusqlite::params![run_id.as_str()],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    Ok(hash)
}

/// A shim that reads the task id off the run, so `StartInput` can carry it.
struct TaskIdShim;

impl TaskIdShim {
    fn from_store(store: &Store, run_id: &RunId) -> Result<conductor_core::TaskId, WorkerError> {
        let id: String = store
            .conn()
            .query_row(
                "SELECT task_id FROM run WHERE id = ?1",
                rusqlite::params![run_id.as_str()],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        conductor_core::TaskId::new(id).map_err(|e| WorkerError::Adapter(e.to_string()))
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
