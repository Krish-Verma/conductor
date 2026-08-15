//! One task, `PENDING` to `COMPLETE` — the S5 vertical.
//!
//! ```text
//! PENDING ─► READY ─► RUNNING ─► RECONCILING ─► VERIFYING ─► COMPLETE
//!    task      claim    agent      §4.8 verdict   §4.5 gate    commit + fetch
//! ```
//!
//! Nothing here is new machinery. S1 supplies the claim and the fence, S2 the
//! workspace, S3 the attempt and reconciliation, S4 the checks and the
//! completion gate, and `effects.rs` the two ledger-protected git effects. This
//! module's whole job is to put them in the one order §5.2 draws, and to refuse
//! every shortcut between them.
//!
//! # The shortcuts it does not take
//!
//! * **`RECONCILING` is not skippable.** The run reaches it through
//!   [`crate::run_one_attempt`], which leaves `RUNNING` only via
//!   `RunState::leave_running` — a function that takes a `TerminalAttempt` and
//!   returns one destination.
//! * **`COMPLETE` cannot be named without evidence.** The only
//!   `ReconciledRoute::Complete` in this file carries a `VerifiedComplete`
//!   obtained from `completion::evaluate`, and the store refuses the write
//!   unless the run is in `VERIFYING`.
//! * **The task row moves in step with the run**, through §5.2's legality
//!   table. A run that reached `COMPLETE` and a task that did not would be two
//!   answers to one question.
//! * **No effect happens on the strength of a report.** Integration runs after
//!   the gate, never before it.

use std::path::PathBuf;
use std::time::Duration;

use conductor_agent::AgentAdapter;
use conductor_core::completion::{
    AcceptanceEvidence, CompletionEvidence, FindingsEvidence, PolicyEvidence,
    ReconciliationEvidence, Refusal, Slice, VerifiedComplete,
};
use conductor_core::{Fence, ReconciledRoute, RunId, RunState, TaskId, TaskState};
use conductor_git::integrate::{FetchedRef, MadeCommit, Trailer, Trailers};
use conductor_git::{Scope, SensitivePatterns, TreeHasher};
use conductor_store::Store;

use crate::effects::{Integration, IntegrationConfig, IntegrationObserver, integrate};
use crate::paths::{ArtifactRoot, Owner};
use crate::supervise::SupervisorConfig;
use crate::verify::profile;
use crate::verify::runner::{CheckKind, RunnerConfig, VerificationReport, run_profile};
use crate::worker::{
    AttemptOutcomeRecord, RunObserver, WorkerConfig, WorkerError, run_one_attempt,
};

/// The severity at which a finding blocks completion.
///
/// §4.5's criterion 4 says "Zero unresolved findings" and Part 9 row 5 says a
/// malformed report leaves a finding that "stays" and the task still reaches
/// `COMPLETE` + finding. Those two cannot both be true as written. The severity
/// qualifier is what reconciles them, and it is not invented for the purpose:
/// S3 and S4 already raise `CRITICAL` for exactly the conditions that must stop
/// a run (workspace missing, baseline missing, an effect nobody can decide, an
/// agent reaching for the control socket) and `WARNING`/`INFO` for the ones that
/// must merely be seen.
///
/// **Reported as a master-plan contradiction, not resolved quietly.** If the
/// intended reading is the literal one, this constant is the single place to
/// change — and row 5 then has to move off `COMPLETE`.
pub const BLOCKING_FINDING_SEVERITY: &str = "CRITICAL";

/// What the vertical needs.
#[derive(Debug, Clone)]
pub struct VerticalConfig {
    /// The task to run.
    pub task_id: TaskId,
    /// This worker's identity.
    pub worker_id: String,
    /// The operator's repository.
    pub source_repo: PathBuf,
    /// Where run workspaces live.
    pub workspaces_root: PathBuf,
    /// Where artifacts live (§3.1).
    pub artifacts_root: PathBuf,
    /// Where orphan workspaces are moved. **Never deleted** (§4.1, row 18).
    pub quarantine_root: PathBuf,
    /// The verification profile (§4.5).
    pub profile_path: PathBuf,
    /// Where the tree hasher keeps its index. **Must be outside the workspace.**
    pub scratch_index: PathBuf,
    /// Supervision budgets.
    pub supervisor: SupervisorConfig,
    /// Lease duration, milliseconds.
    pub lease_ms: i64,
    /// Heartbeat interval.
    pub heartbeat_interval: Duration,
    /// The M29 absorber for verification checks.
    pub startup_grace: Duration,
    /// Which paths require policy evaluation (§4.8).
    pub sensitive: SensitivePatterns,
    /// Extra environment variables the agent is given (§4.9's allowlist).
    pub agent_env_extra: std::collections::BTreeMap<String, String>,
    /// Which (adapter × launcher × host) measurement §4.2's gate reads.
    ///
    /// Not `Option`, and not the task's *requirements* — those are durable on
    /// the task row precisely so no caller can omit them. This names the
    /// measurement to look up, which is a property of the host and the adapter.
    /// A key nobody has probed misses the cache, and a miss is `fail_closed()`,
    /// so getting this wrong refuses rather than permits.
    pub probe_key: crate::containment::cache::ProbeKey,
}

/// How the vertical ended.
#[derive(Debug, Clone)]
pub enum VerticalOutcome {
    /// Every criterion §4.5 owns held, and the work was integrated.
    Complete {
        /// The Conductor-owned commit.
        commit: MadeCommit,
        /// The ref update in the source repository.
        fetched: FetchedRef,
        /// The tree everything was bound to.
        tree_hash: String,
        /// Criteria a later slice owes (§4.5's 5 and 7).
        deferred: Vec<conductor_core::completion::Criterion>,
    },
    /// The run stopped somewhere short of `COMPLETE`, deliberately.
    Stopped {
        /// The state it stopped in.
        state: RunState,
        /// Why, in terms a human can act on.
        reason: String,
    },
}

/// What one pass of the vertical did.
#[derive(Debug, Clone)]
pub struct Vertical {
    /// The run.
    pub run_id: RunId,
    /// Everything the attempt produced (§4.8's verdict included).
    pub attempt: AttemptOutcomeRecord,
    /// The verification report, when the run got as far as verifying.
    pub verification: Option<VerificationReport>,
    /// Criteria the completion gate refused on, when it refused.
    pub refusals: Vec<Refusal>,
    /// How it ended.
    pub outcome: VerticalOutcome,
}

impl Vertical {
    /// Whether the task reached `COMPLETE`.
    pub fn is_complete(&self) -> bool {
        matches!(self.outcome, VerticalOutcome::Complete { .. })
    }
}

/// What the verify → gate → integrate stage needs, whether it arrived from a
/// fresh attempt or from a restart.
///
/// Both entry points converge here on purpose. A restart that ran a *different*
/// finishing sequence from a fresh run would be a second implementation of the
/// thing acceptance row 22 is about, and the matrix would then be proving
/// something about the copy.
struct Stage {
    ordinal: i64,
    attempt_id: Option<String>,
    verdict: conductor_git::Verdict,
    changed_paths: Vec<String>,
}

/// What §4.5's criteria 6 and 7 are told about this run's policy position.
///
/// Derived from the run's *own state*, not from a flag a caller passed: a run
/// that reached `VERIFYING` with a `POLICY_SENSITIVE` verdict got there by
/// passing the policy gate, and the gate only lets it through when the action
/// was authorized. Re-deriving it here rather than threading a boolean means a
/// future second path into `finish` inherits the same answer.
fn policy_position(
    store: &Store,
    run_id: &RunId,
    verdict: conductor_git::Verdict,
) -> (ReconciliationEvidence, PolicyEvidence) {
    if verdict != conductor_git::Verdict::PolicySensitive {
        return (
            ReconciliationEvidence::from(verdict),
            PolicyEvidence::NoSensitiveActions,
        );
    }

    // The run is in `VERIFYING` with a policy-sensitive verdict, which
    // `policy_gate::route_reconciliation` only permits after the action was
    // either allowed by a rule or authorized by a consumed grant. Name the
    // grant when there is one, so the completion evidence says *what*
    // authorized it rather than merely asserting that something did.
    let authorization = store
        .conn()
        .query_row(
            "SELECT g.id FROM approval_grant g
               JOIN approval_request r ON r.id = g.request_id
              WHERE r.run_id = ?1 AND g.state = 'CONSUMED'
              ORDER BY g.granted_at DESC LIMIT 1",
            rusqlite::params![run_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .map(|id| format!("grant {id}"))
        .unwrap_or_else(|| "the run's pinned policy allows the action".to_string());

    (
        ReconciliationEvidence::AuthorizedPolicySensitive {
            verdict: verdict.to_string(),
            authorization: authorization.clone(),
        },
        PolicyEvidence::AllGrantsPresent {
            detail: authorization,
        },
    )
}

/// What the shared stage produced.
struct Finished {
    verification: Option<VerificationReport>,
    refusals: Vec<Refusal>,
    outcome: VerticalOutcome,
}

/// Drive one task from `PENDING` to a terminal-or-waiting state.
///
/// A fresh session every time. §4.6's session policy belongs to repair, which
/// knows the attempt ordinal and the configuration; a caller with neither has
/// nothing to resume.
pub fn run_task(
    store: &mut Store,
    adapter: &dyn AgentAdapter,
    config: &VerticalConfig,
    observer: &mut dyn VerticalObserver,
) -> Result<Vertical, WorkerError> {
    run_task_with_session(store, adapter, config, None, observer)
}

/// The same, with §4.6's session decision already made.
///
/// `session` is the id the adapter should resume, and `None` covers all three of
/// [`crate::repair::config::session_id_for`]'s ways to get nothing: the policy is
/// `Fresh`, the adapter cannot resume, or there is no previous session. The
/// distinction between them is repair's to make and to record; by the time it
/// reaches here it is one `Option`.
///
/// A separate entry point rather than a field on [`VerticalConfig`]: the session
/// is a property of *this attempt*, not of the task's configuration, and putting
/// per-attempt state on a config that callers build once and reuse is how a
/// second attempt quietly resumes the first one's context.
pub fn run_task_with_session(
    store: &mut Store,
    adapter: &dyn AgentAdapter,
    config: &VerticalConfig,
    session: Option<&str>,
    observer: &mut dyn VerticalObserver,
) -> Result<Vertical, WorkerError> {
    let task = store
        .task(&config.task_id)?
        .ok_or_else(|| WorkerError::Adapter(format!("no task {}", config.task_id)))?;

    // §5.2: `PENDING ──deps met──► READY`. S5 has no dependency graph — that is
    // S11's, with the plan ledger — so "deps met" is vacuously true and is
    // written as one step rather than assumed away.
    if task.state == TaskState::Pending {
        store.set_task_state(&config.task_id, TaskState::Ready)?;
    }

    // §5.2's edge is `READY ──claim+eligibility──► RUNNING`, and this is the
    // second gate on it (acceptance row 30). It runs **before** the claim: the
    // claim moves the run to `RUNNING` atomically, and §4.8's "every exit from
    // `RUNNING` passes through reconciliation" must not need an exception for a
    // run that never launched an agent.
    //
    // A refusal is durable — `BLOCKED` plus a `CRITICAL` finding naming the
    // dimension — because row 30's expected persisted state is `BLOCKED` and a
    // refusal nobody can read afterwards is not a refusal a human can act on.
    match crate::enforce::launch::gate(store, &config.task_id, &config.probe_key) {
        Ok(None) => {}
        Ok(Some(refusal)) => return Err(block_ineligible(store, config, &refusal.detail)),
        // A gate that could not decide is a gate that has not permitted
        // anything. Falling through on error would turn every unreadable
        // requirement into an ungated launch — the single most valuable bug an
        // attacker could hope to find here — so the error routes to exactly the
        // same refusal the decided case does.
        Err(error) => return Err(block_ineligible(store, config, &error.to_string())),
    }

    let claimed = store
        .claim_next_run(&config.worker_id, now_ms(), config.lease_ms)?
        .ok_or_else(|| {
            WorkerError::Adapter(format!(
                "task {} has no claimable run; it is in {}",
                config.task_id, task.state
            ))
        })?;
    let fence = claimed.fence();
    let run_id = claimed.run_id.clone();

    // The run row is the source of truth for scope and integration target: a
    // spec edited mid-run must not change what a run in flight is doing
    // (acceptance row 21's principle).
    let run = store
        .run(&run_id)?
        .ok_or_else(|| WorkerError::Adapter(format!("run {run_id} vanished")))?;
    let scope = Scope::new(task.scope_globs.clone());

    mirror(store, &config.task_id, RunState::Running)?;

    // ---- the attempt (S3) -------------------------------------------------
    let worker = WorkerConfig {
        worker_id: config.worker_id.clone(),
        workspaces_root: config.workspaces_root.clone(),
        artifacts_root: config.artifacts_root.clone(),
        source_repo: config.source_repo.clone(),
        supervisor: config.supervisor.clone(),
        lease_ms: config.lease_ms,
        heartbeat_interval: config.heartbeat_interval,
        scope,
        sensitive: config.sensitive.clone(),
        agent_env_extra: config.agent_env_extra.clone(),
        agent_session_id: session.map(str::to_string),
    };
    let attempt = run_one_attempt(
        store,
        &fence,
        adapter,
        &worker,
        &mut ObserverBridge(observer),
    )?;

    // `run_one_attempt` hands the lease back when it is done with the attempt.
    // Renewing re-establishes ownership without moving the epoch — a claim would
    // move it and fence this worker out of its own run.
    store.renew_lease(&fence, now_ms(), config.lease_ms)?;

    // The task follows the run through §5.2's machine, one legal step at a time.
    mirror(store, &config.task_id, RunState::Reconciling)?;
    mirror(store, &config.task_id, attempt.route.state())?;

    let stage = Stage {
        ordinal: store.next_attempt_ordinal(&run_id)? - 1,
        attempt_id: Some(attempt.attempt_id.as_str().to_string()),
        verdict: attempt.verdict,
        changed_paths: attempt.changed_paths.clone(),
    };
    let finished = finish(
        store,
        &fence,
        config,
        &task,
        &run,
        claimed.policy_hash.as_str(),
        stage,
        attempt.route.state(),
        observer,
    )?;

    Ok(Vertical {
        run_id,
        attempt,
        verification: finished.verification,
        refusals: finished.refusals,
        outcome: finished.outcome,
    })
}

/// What resuming a crashed run produced.
#[derive(Debug, Clone)]
pub struct Resumed {
    /// The run.
    pub run_id: RunId,
    /// What §4.7's recovery decided on the way through.
    pub decisions: Vec<crate::recovery::RecoveryDecision>,
    /// The verification report, when the run got as far as verifying.
    pub verification: Option<VerificationReport>,
    /// Criteria the completion gate refused on, when it refused.
    pub refusals: Vec<Refusal>,
    /// How it ended.
    pub outcome: VerticalOutcome,
}

/// Pick a crashed run back up — §4.7's restart, continued to completion.
///
/// **No new agent attempt.** The agent already ran; §4.7's recovery reads the
/// repository and classifies it, and the work that survives is the work that
/// counts. Running the agent again would produce a second attempt against a
/// workspace that already holds the first one's edits, whose baseline would then
/// be captured *after* them — the run would reconcile as `NO_CHANGE` and the
/// finished work would be invisible.
///
/// `now_ms` is a parameter for the reason `recover` takes one: the lease
/// predicate is `expires_at < now`, and a restart should not have to wait sixty
/// seconds for a lease it can see is dead. Production passes the real clock; a
/// test passes a time past the lease it just killed.
pub fn resume_task(
    store: &mut Store,
    config: &VerticalConfig,
    now_ms: i64,
    observer: &mut dyn VerticalObserver,
) -> Result<Resumed, WorkerError> {
    // §4.7 step 2. Also moves the fencing epoch, so the killed worker — if it
    // somehow were not killed — could not write (row 27).
    store.expire_leases(now_ms)?;

    let task = store
        .task(&config.task_id)?
        .ok_or_else(|| WorkerError::Adapter(format!("no task {}", config.task_id)))?;
    let run_id = store.active_run_for_task(&config.task_id)?.ok_or_else(|| {
        WorkerError::Adapter(format!("task {} has no active run", config.task_id))
    })?;

    let claimed = store
        .claim_run(&run_id, &config.worker_id, now_ms, config.lease_ms)?
        .ok_or_else(|| {
            WorkerError::Adapter(format!(
                "run {run_id} is not claimable; either somebody else holds it or it \
                 is not in a state a restart may take"
            ))
        })?;
    let fence = claimed.fence();

    // §4.7 steps 3–7, through the *same* function a recovery pass uses. The
    // lease is deliberately kept: a run routed to `VERIFYING` is not claimable,
    // so releasing it here would strand the very run being recovered.
    let recovery_config = crate::recovery::RecoveryConfig {
        worker_id: config.worker_id.clone(),
        workspaces_root: config.workspaces_root.clone(),
        quarantine_root: config.quarantine_root.clone(),
        artifacts_root: config.artifacts_root.clone(),
        adopt_live_agents: false,
        lease_ms: config.lease_ms,
        scope: Scope::new(task.scope_globs.clone()),
        sensitive: config.sensitive.clone(),
    };
    let mut decisions = Vec::new();
    let reconciliation =
        crate::recovery::recover_one(store, &fence, &recovery_config, now_ms, &mut decisions)?;

    let run = store
        .run(&run_id)?
        .ok_or_else(|| WorkerError::Adapter(format!("run {run_id} vanished mid-resume")))?;
    mirror(store, &config.task_id, run.state)?;

    let Some(reconciliation) = reconciliation else {
        let reason = "recovery could not reconcile the run".to_string();
        store.release_lease(&fence, now_ms)?;
        return Ok(Resumed {
            run_id,
            decisions,
            verification: None,
            refusals: Vec::new(),
            outcome: VerticalOutcome::Stopped {
                state: run.state,
                reason,
            },
        });
    };

    let attempt_id = store
        .attempts_for_run(&run_id)?
        .last()
        .map(|a| a.id.as_str().to_string());
    let stage = Stage {
        ordinal: store.next_attempt_ordinal(&run_id)? - 1,
        attempt_id,
        verdict: reconciliation.verdict,
        changed_paths: reconciliation.changed_paths.clone(),
    };
    let finished = finish(
        store,
        &fence,
        config,
        &task,
        &run,
        claimed.policy_hash.as_str(),
        stage,
        run.state,
        observer,
    )?;

    Ok(Resumed {
        run_id,
        decisions,
        verification: finished.verification,
        refusals: finished.refusals,
        outcome: finished.outcome,
    })
}

/// Pick a run back up after a human answered its approval request — acceptance
/// rows 12, 13 and 25.
///
/// Deliberately **not** a second finishing sequence. It moves the run out of
/// `AWAITING_APPROVAL` and hands it to [`resume_task`], which is the same path
/// a crashed run takes: reconcile against the stored baseline, then verify,
/// gate and integrate. Two paths to `COMPLETE` would mean the crash matrix
/// proves things about one of them.
///
/// **No agent is launched**, for the reason [`resume_task`] documents: the work
/// is already in the workspace, and a fresh attempt would re-capture the
/// baseline from it and reconcile the approved change away as `NO_CHANGE`.
///
/// **The grant is not consumed here.** It is consumed at the policy gate on the
/// way through, where the binding is recomputed from the decision actually
/// being made (§4.3: "immediately before the side effect"). That is what makes
/// a revoked-in-the-meantime grant stop the run rather than merely being
/// noticed afterwards — and it is why this function does not need to know which
/// grant, or whether there is one.
pub fn resume_on_grant(
    store: &mut Store,
    config: &VerticalConfig,
    now_ms: i64,
    observer: &mut dyn VerticalObserver,
) -> Result<Resumed, WorkerError> {
    let run_id = store.active_run_for_task(&config.task_id)?.ok_or_else(|| {
        WorkerError::Adapter(format!("task {} has no active run", config.task_id))
    })?;
    store.resume_after_grant(&run_id, "an approval request was answered", now_ms)?;
    mirror(store, &config.task_id, RunState::Reconciling)?;
    resume_task(store, config, now_ms, observer)
}

/// Verify → gate → integrate → complete, for a run already in `VERIFYING`.
#[allow(clippy::too_many_arguments)]
fn finish(
    store: &mut Store,
    fence: &Fence,
    config: &VerticalConfig,
    task: &conductor_store::TaskRow,
    run: &conductor_store::RunRow,
    policy_hash: &str,
    stage: Stage,
    state: RunState,
    observer: &mut dyn VerticalObserver,
) -> Result<Finished, WorkerError> {
    let run_id = run.id.clone();

    if state != RunState::Verifying {
        let reason = format!(
            "reconciliation returned {}, which routes to {state}",
            stage.verdict
        );
        store.release_lease(fence, now_ms())?;
        return Ok(Finished {
            verification: None,
            refusals: Vec::new(),
            outcome: VerticalOutcome::Stopped { state, reason },
        });
    }

    // ---- verification (S4) ------------------------------------------------
    let workspace = store
        .run(&run_id)?
        .and_then(|r| r.workspace_path)
        .map(PathBuf::from)
        .ok_or_else(|| WorkerError::Adapter(format!("run {run_id} has no workspace")))?;

    let loaded = profile::load(&config.profile_path)
        .map_err(|e| WorkerError::Adapter(format!("verification profile: {e}")))?;
    let hasher = TreeHasher::new(&workspace, &config.scratch_index)?;

    let runner_config = RunnerConfig {
        workspace: workspace.clone(),
        scratch_index: config.scratch_index.clone(),
        artifacts: ArtifactRoot::new(&config.artifacts_root),
        run_id: run_id.clone(),
        attempt_ordinal: stage.ordinal,
        attempt_id: stage.attempt_id.clone(),
        owner: Owner::new(config.worker_id.clone(), std::process::id() as i32),
        // The same isolated environment the agent ran under. A verification
        // command that could reach a credential the agent could not would make
        // the boundary a property of which process is running, not of the run.
        env: crate::enforce::env::prepare(&workspace)
            .map_err(|e| WorkerError::Adapter(format!("verification environment: {e}")))?
            .into_vars(),
        commit_sha: run.base_commit.clone(),
        changed_paths: stage.changed_paths.clone(),
        startup_grace: config.startup_grace,
    };
    let report = run_profile(store, fence, &runner_config, &loaded, now_ms())
        .map_err(|e| WorkerError::Adapter(format!("verification: {e}")))?;

    for finding in &report.findings {
        store.record_finding(
            fence,
            &format!("f-{run_id}-{}", finding.kind),
            finding.kind,
            "WARNING",
            &finding.detail,
            now_ms(),
        )?;
    }

    // §4.5's criterion 1 is "PASS **at the current tree hash**", so the tree is
    // read now — after the checks — and every result is measured against it. A
    // check whose tree moved under it is caught here rather than trusted.
    let tree_hash = hasher.hash()?.as_str().to_string();

    // ---- the completion gate (S4, with S9's criteria 6 and 7) ------------
    let (reconciliation, policy) = policy_position(store, &run_id, stage.verdict);
    let evidence = CompletionEvidence {
        tree_hash: tree_hash.clone(),
        required: report.checks_evidence(CheckKind::Required),
        conditional: report.checks_evidence(CheckKind::Conditional),
        invariants: report.checks_evidence(CheckKind::Invariant),
        findings: FindingsEvidence::unresolved(blocking_findings(store, &run_id)?),
        acceptance: AcceptanceEvidence::NotEvaluated {
            owner: Slice::S11PlanLedger,
        },
        reconciliation,
        policy,
    };

    let verified = match conductor_core::completion::evaluate(&evidence) {
        Ok(verified) => verified,
        Err(refusals) => {
            // §4.5 distinguishes FAIL from INCONCLUSIVE because "the distinction
            // determines what happens next: FAIL → repair; INCONCLUSIVE →
            // bounded infra retry, then human". S6 owns the repair loop; the
            // routing decision is made here so S6 inherits it rather than
            // inventing it.
            //
            // **Corrected at S6.** S5 routed anything without a `FAIL` straight
            // to `AWAITING_REVIEW`, which made the *first* half of §4.5's
            // sentence unreachable: an `INCONCLUSIVE` check went to a person
            // with no retry at all, and acceptance row 8's "infra retry ×1, **no
            // budget spent**" could not happen. §5.2 gives `VERIFYING` exactly
            // one non-human successor, so the bounded retry has to leave through
            // `REPAIRING` — and what makes that safe is that the bound is real:
            // `repair::breaker::decide` counts infrastructure attempts against
            // `max_infra_retries` and hands the run to a person after them,
            // without touching the work budget §4.7 protects.
            let route = match crate::repair::config::retry_kind(&report) {
                crate::repair::config::RetryKind::Work
                | crate::repair::config::RetryKind::Infrastructure => ReconciledRoute::Repairing,
                // Every check passed and the gate still refused: that is a
                // criterion no further agent attempt can satisfy.
                crate::repair::config::RetryKind::None => ReconciledRoute::AwaitingReview,
            };
            let reason = refusals
                .iter()
                .map(|r| format!("{:?}: {}", r.criterion, r.detail))
                .collect::<Vec<_>>()
                .join("; ");
            store.route_verified(fence, route.clone(), &reason, now_ms())?;
            mirror(store, &config.task_id, route.state())?;
            store.release_lease(fence, now_ms())?;
            return Ok(Finished {
                verification: Some(report),
                refusals,
                outcome: VerticalOutcome::Stopped {
                    state: route.state(),
                    reason,
                },
            });
        }
    };

    // ---- integration (§4.1, §4.7) ----------------------------------------
    let integration_config = IntegrationConfig {
        source_repo: config.source_repo.clone(),
        workspace: workspace.clone(),
        run_branch: run.run_branch.clone(),
        target_branch: run.target_branch.clone().ok_or_else(|| {
            // Schema v4 made the column nullable, because a row written before
            // it genuinely does not know its target. Integrating into a branch
            // nobody chose is exactly the guess §4.7 forbids.
            WorkerError::Adapter(format!(
                "run {run_id} records no target branch, so there is nowhere to \
                 integrate it; re-create the run"
            ))
        })?,
        base_commit: run.base_commit.clone(),
        attempt_ordinal: stage.ordinal,
        tree_hash: tree_hash.clone(),
        subject: format!("{}: {}", config.task_id, task.slice_id),
        // The policy hash comes from the claim, which read it off the run row
        // inside the claiming transaction — the same snapshot the run is
        // executing under (acceptance row 23: "run keeps `policy_hash`").
        trailers: trailers_for(&run_id, policy_hash, &report),
    };

    match integrate(
        store,
        fence,
        &integration_config,
        &mut ObserverBridge(observer),
    )? {
        Integration::TargetMoved(divergence) => {
            // Row 16. The work is verified and cannot be integrated without a
            // person; the divergence is attached rather than resolved.
            store.record_finding(
                fence,
                &format!("f-{run_id}-TARGET_BRANCH_MOVED"),
                "TARGET_BRANCH_MOVED",
                "WARNING",
                &divergence.to_string(),
                now_ms(),
            )?;
            let reason = divergence.to_string();
            store.route_verified(fence, ReconciledRoute::AwaitingReview, &reason, now_ms())?;
            mirror(store, &config.task_id, RunState::AwaitingReview)?;
            store.release_lease(fence, now_ms())?;
            Ok(Finished {
                verification: Some(report),
                refusals: Vec::new(),
                outcome: VerticalOutcome::Stopped {
                    state: RunState::AwaitingReview,
                    reason,
                },
            })
        }
        Integration::NothingToCommit => {
            // The gate accepted a `CLEAN_*` verdict — which means reconciliation
            // saw changes — and staging found none, and no commit of this tree is
            // on the branch either. Two readings of one tree that disagree is not
            // something to resolve by picking one.
            let reason = format!(
                "reconciliation returned {} but the workspace has nothing to commit; \
                 the two readings of the tree disagree",
                stage.verdict
            );
            store.record_finding(
                fence,
                &format!("f-{run_id}-NOTHING_TO_INTEGRATE"),
                "NOTHING_TO_INTEGRATE",
                "CRITICAL",
                &reason,
                now_ms(),
            )?;
            store.route_verified(fence, ReconciledRoute::AwaitingReview, &reason, now_ms())?;
            mirror(store, &config.task_id, RunState::AwaitingReview)?;
            store.release_lease(fence, now_ms())?;
            Ok(Finished {
                verification: Some(report),
                refusals: Vec::new(),
                outcome: VerticalOutcome::Stopped {
                    state: RunState::AwaitingReview,
                    reason,
                },
            })
        }
        Integration::Integrated { commit, fetched } => {
            let deferred = verified.deferred().to_vec();
            complete(store, fence, &config.task_id, verified)?;
            store.release_lease(fence, now_ms())?;
            Ok(Finished {
                verification: Some(report),
                refusals: Vec::new(),
                outcome: VerticalOutcome::Complete {
                    commit,
                    fetched,
                    tree_hash,
                    deferred,
                },
            })
        }
    }
}

/// Record row 30's refusal and return the error the caller sees.
///
/// The run moves `READY → BLOCKED` and a `CRITICAL` finding carries the
/// dimension, the requirement and the measured value. Both happen in one store
/// transaction, so there is no window in which a run is blocked with no reason
/// attached or carries a reason while still looking launchable.
///
/// Best-effort on the *task* mirror only: if the run row is blocked and the
/// mirror write fails, the run is still blocked, and reporting the original
/// refusal is more useful than replacing it with a bookkeeping error.
fn block_ineligible(store: &mut Store, config: &VerticalConfig, detail: &str) -> WorkerError {
    let run_id = match store.active_run_for_task(&config.task_id) {
        Ok(Some(run_id)) => run_id,
        Ok(None) => {
            return WorkerError::Adapter(format!(
                "task {} is ineligible to launch ({detail}), and has no active \
                 run to record it against",
                config.task_id
            ));
        }
        Err(error) => return WorkerError::Store(error),
    };

    let finding_id = crate::enforce::launch::finding_id(&run_id);
    if let Err(error) = store.refuse_ineligible_launch(&run_id, &finding_id, detail, now_ms()) {
        return WorkerError::Store(error);
    }
    let _ = mirror(store, &config.task_id, RunState::Blocked);

    WorkerError::Adapter(format!(
        "run {run_id} may not launch unattended on this host: {detail}"
    ))
}

/// Write `COMPLETE` — the one place in the crate that can.
fn complete(
    store: &mut Store,
    fence: &Fence,
    task_id: &TaskId,
    verified: VerifiedComplete,
) -> Result<(), WorkerError> {
    let detail = format!(
        "verified at tree {}; deferred: {:?}",
        verified.tree_hash(),
        verified.deferred()
    );
    store.route_verified(
        fence,
        ReconciledRoute::Complete(verified),
        &detail,
        now_ms(),
    )?;
    mirror(store, task_id, RunState::Complete)?;
    Ok(())
}

/// Move the task to wherever the run just went (§5.2: the run "mirrors its
/// task").
///
/// A no-op when they already agree, because §5.2's table has no self-transition
/// — writing the state a task is already in looks like progress and is not.
fn mirror(store: &mut Store, task_id: &TaskId, run_state: RunState) -> Result<(), WorkerError> {
    let current = store
        .task(task_id)?
        .ok_or_else(|| WorkerError::Adapter(format!("no task {task_id}")))?
        .state;
    let target = run_state.as_task_state();
    if current == target {
        return Ok(());
    }
    store.set_task_state(task_id, target)?;
    Ok(())
}

/// How many unresolved findings block completion — §4.5's criterion 4.
///
/// See [`BLOCKING_FINDING_SEVERITY`] for why this is severity-filtered and why
/// that is reported rather than assumed.
fn blocking_findings(store: &Store, run_id: &RunId) -> Result<usize, WorkerError> {
    Ok(store
        .findings_for_run(run_id)?
        .into_iter()
        .filter(|f| f.resolution.is_none() && f.severity == BLOCKING_FINDING_SEVERITY)
        .count())
}

/// §3.4's trailers, **as far as they have a real source at S5**.
///
/// | trailer | source | status |
/// |---|---|---|
/// | `Conductor-Run` | `run.id` | emitted |
/// | `Conductor-Policy` | `run.policy_hash` | emitted |
/// | `Conductor-Verification` | digest of the evidence the gate read | emitted |
/// | `Conductor-Plan` | plan versioning — **S11** | absent |
/// | `Conductor-Approval` | approvals — **S8** | absent |
///
/// §3.4's purpose is that the audit trail "survives total local state loss".
/// A trailer with a value nobody can reproduce actively damages that: a reader
/// recovering from total loss has no way to tell an invented hash from a real
/// one, so every trailer becomes suspect. Absent is honest; invented is not.
fn trailers_for(run_id: &RunId, policy_hash: &str, report: &VerificationReport) -> Trailers {
    Trailers::new([
        Trailer::new("Conductor-Run", run_id.as_str()),
        Trailer::new("Conductor-Policy", policy_hash),
        Trailer::new("Conductor-Verification", report.evidence_digest()),
    ])
}

/// Notified as the vertical passes a point a crash matters at.
///
/// Two enums rather than one: S3's [`crate::worker::RunPoint`] names the gaps in
/// *one attempt*, and [`crate::effects::IntegrationPoint`] names the gaps in
/// integration. Merging them would renumber S3's matrix, whose thirteen points
/// are load-bearing evidence.
pub trait VerticalObserver: RunObserver + IntegrationObserver {}

impl<T: RunObserver + IntegrationObserver> VerticalObserver for T {}

/// Adapts one `&mut dyn VerticalObserver` into the two single-purpose observer
/// types the stages take, without either stage learning about the other's
/// points.
struct ObserverBridge<'a>(&'a mut dyn VerticalObserver);

impl RunObserver for ObserverBridge<'_> {
    fn at(&mut self, point: crate::worker::RunPoint) {
        <dyn VerticalObserver as RunObserver>::at(self.0, point)
    }
}

impl IntegrationObserver for ObserverBridge<'_> {
    fn at(&mut self, point: crate::effects::IntegrationPoint) {
        <dyn VerticalObserver as IntegrationObserver>::at(self.0, point)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
