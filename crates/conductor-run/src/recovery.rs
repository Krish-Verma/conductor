//! Startup recovery — master plan §4.7's nine steps, in order.
//!
//! ```text
//! 1. Open DB, migrate, integrity_check.
//! 2. Find runs in RUNNING/RECONCILING/VERIFYING with expired leases.
//! 3. Probe recorded pid → alive & start-time matches?
//!        alive → adopt or terminate (config); record.
//!        dead  → attempt := STALE.
//! 4. Locate workspace. Absent → run BLOCKED + finding.
//! 5. Capture current git state; diff against stored baseline.
//! 6. Re-run verification only if the tree hash has no cached valid result.
//! 7. Classify (§4.8) and route.
//! 8. Scan for orphaned workspaces → QUARANTINE, never delete.
//! 9. Expire overdue approvals; restore AWAITING_APPROVAL waits.
//! ```
//!
//! Two things this file refuses to do, both because §5.2 and §4.7 say so:
//!
//! * **It never records unknown as known.** A recorded pid that is gone becomes
//!   `STALE`, not `CRASHED`; a recorded pid whose start time does not match is
//!   somebody else's process and is also `STALE`, never adopted.
//! * **It never resolves an `INTENDED` side effect by retrying.** It re-checks
//!   the precondition against the world. When the world will not say, the effect
//!   becomes `AMBIGUOUS`, the run halts, and a human decides.
//!
//! Recovery **claims** each run it touches. Writing to an unowned run would be
//! an unfenced write, and §4.7 has none.

use std::collections::BTreeSet;
use std::path::PathBuf;

use conductor_core::attempt::AttemptState;
use conductor_core::{Attempt, AttemptId, EventKind, Fence, ReconciledRoute, RunId, RunState};
use conductor_git::{Baseline, Quarantined, find_orphans, observe, quarantine};
use conductor_store::{ExpiredLease, Store};
use serde::Serialize;

use crate::effects::{PreconditionAnswer, check_precondition};
use crate::paths::ArtifactRoot;
use crate::supervise::{Liveness, probe};
use crate::worker::WorkerError;

/// What recovery is allowed to do.
#[derive(Debug, Clone)]
pub struct RecoveryConfig {
    /// This worker's identity.
    pub worker_id: String,
    /// Where run workspaces live.
    pub workspaces_root: PathBuf,
    /// Where orphans are moved. **Never deleted** (§4.1, row 18).
    pub quarantine_root: PathBuf,
    /// Where artifacts live.
    pub artifacts_root: PathBuf,
    /// §4.7 step 3: "alive → adopt or terminate (config)".
    ///
    /// `false` — terminate — is the conservative default: an adopted agent is
    /// one this process did not spawn, whose stdout nobody is reading and whose
    /// budgets nobody is enforcing.
    pub adopt_live_agents: bool,
    /// Lease duration for the runs recovery claims.
    pub lease_ms: i64,
    /// The task scope used when reconciling a recovered run.
    pub scope: conductor_git::Scope,
    /// Which paths are policy-sensitive.
    pub sensitive: conductor_git::SensitivePatterns,
}

/// One decision recovery made, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RecoveryDecision {
    /// A live agent was found and left running.
    AdoptedLiveAgent {
        /// The run.
        run_id: RunId,
        /// The attempt.
        attempt_id: AttemptId,
        /// Its pid.
        pid: i32,
    },
    /// A live agent was found and killed.
    TerminatedLiveAgent {
        /// The run.
        run_id: RunId,
        /// The attempt.
        attempt_id: AttemptId,
        /// Its pid.
        pid: i32,
    },
    /// The attempt's process is gone, or is not the process we recorded.
    ///
    /// `STALE`, never `CRASHED`: nobody observed an exit.
    AttemptStale {
        /// The run.
        run_id: RunId,
        /// The attempt.
        attempt_id: AttemptId,
        /// What the probe found.
        reason: String,
    },
    /// The workspace is gone, so nothing about the attempt can be established.
    WorkspaceMissing {
        /// The run.
        run_id: RunId,
        /// Where it should have been.
        expected: String,
    },
    /// The run was reconciled and routed onwards.
    Reconciled {
        /// The run.
        run_id: RunId,
        /// The §4.8 verdict.
        verdict: String,
        /// Where it went.
        route: ReconciledRoute,
        /// Whether verification still has to run at this tree (§4.7 step 6).
        verification_needed: bool,
    },
    /// An `INTENDED` effect was re-checked and had in fact happened.
    EffectConfirmed {
        /// The operation.
        operation_id: String,
    },
    /// An `INTENDED` effect was re-checked and had not happened.
    EffectNotDone {
        /// The operation.
        operation_id: String,
    },
    /// An `INTENDED` effect could not be decided. The run halts (§4.7).
    EffectAmbiguous {
        /// The operation.
        operation_id: String,
        /// Why the world would not say.
        detail: String,
    },
    /// A run could not be claimed, so recovery left it alone.
    ///
    /// Not a failure: another worker holds it, which is the fencing working.
    NotClaimable {
        /// The run.
        run_id: RunId,
    },
}

/// What one recovery pass did.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryReport {
    /// Step 1: `PRAGMA integrity_check`.
    pub integrity_check: Vec<String>,
    /// Step 1: the schema version after migration.
    pub schema_version: Option<i64>,
    /// Step 2: runs whose leases had lapsed.
    pub expired_leases: Vec<ExpiredLease>,
    /// Steps 3–7.
    pub decisions: Vec<RecoveryDecision>,
    /// Step 8: orphan workspaces moved to quarantine, never deleted.
    pub quarantined: Vec<String>,
    /// Step 9: approval requests whose TTL had passed.
    pub expired_approvals: Vec<String>,
    /// Step 9: approvals still waiting, restored.
    pub restored_waits: Vec<String>,
}

/// Run one recovery pass — §4.7's nine steps.
pub fn recover(
    store: &mut Store,
    config: &RecoveryConfig,
    now_ms: i64,
) -> Result<RecoveryReport, WorkerError> {
    // Step 1. The store was opened and migrated by `Store::open_or_create`; the
    // check that the file is sound is ours.
    let integrity_check = store.integrity_check()?;
    let schema_version = store.schema_version()?;

    // Step 2. Runs whose worker is no longer there. The sweep also moves the
    // fencing epoch, so a worker that wakes up later cannot write (row 27).
    let expired_leases = store.expire_leases(now_ms)?;

    let mut decisions = Vec::new();

    // Steps 3–7, per run that now needs attention.
    let candidates: Vec<RunId> = store
        .runs_in_states(&[RunState::Reconciling])?
        .into_iter()
        .map(|row| row.id)
        .collect();

    for run_id in candidates {
        let Some(claimed) = store.claim_run(&run_id, &config.worker_id, now_ms, config.lease_ms)?
        else {
            decisions.push(RecoveryDecision::NotClaimable { run_id });
            continue;
        };
        let fence = claimed.fence();
        recover_one(store, &fence, config, now_ms, &mut decisions)?;
        // The run has been routed; hand the lease back so a normal worker can
        // pick it up. Releasing does not move the epoch — nothing was revoked,
        // the worker is finished with it.
        store.release_lease(&fence, now_ms)?;
    }

    // Step 8. Orphans are quarantined, never deleted: "an orphan may hold the
    // only copy of an hour of work" (§4.1).
    let active: BTreeSet<RunId> = store.active_runs()?.into_iter().map(|row| row.id).collect();
    let orphans = find_orphans(&config.workspaces_root, &active)?;
    let mut quarantined: Vec<String> = Vec::new();
    for orphan in &orphans {
        let Quarantined { to, .. } = quarantine(orphan, &config.quarantine_root)?;
        quarantined.push(to.display().to_string());
    }

    // Step 9.
    let expired_approvals = store.expire_approvals(now_ms)?;
    let restored_waits = store
        .pending_approvals()?
        .into_iter()
        .map(|pending| pending.id)
        .collect();

    Ok(RecoveryReport {
        integrity_check,
        schema_version,
        expired_leases,
        decisions,
        quarantined,
        expired_approvals,
        restored_waits,
    })
}

/// Steps 3–7 for **one** run the caller already owns.
///
/// Public because the S5 vertical resumes a crashed run through exactly this
/// path and then keeps going. A restarting foreground worker (§2.4: "Foreground
/// supervisor for S1–S13" — there is no daemon until S14) has to recover *and*
/// continue in the same process, and it must not hand the lease back in between:
/// a run routed to `VERIFYING` is not claimable — §4.7's claim predicate is
/// `READY`/`RECONCILING`, and deliberately so, since the lease sweep already
/// forces `VERIFYING` to `RECONCILING` — so a released lease at that moment
/// would strand the run. Sharing this function is also what keeps a restart and
/// a recovery pass from drifting into two different notions of "recovered".
///
/// Returns the reconciliation when the run got as far as one, so the caller does
/// not have to observe the repository a second time to learn what it said.
pub fn recover_one(
    store: &mut Store,
    fence: &Fence,
    config: &RecoveryConfig,
    now_ms: i64,
    decisions: &mut Vec<RecoveryDecision>,
) -> Result<Option<conductor_git::Reconciliation>, WorkerError> {
    let run_id = fence.run_id().clone();

    // Step 3. Probe every in-flight attempt of this run.
    let attempts = store.attempts_for_run(&run_id)?;
    for row in attempts.iter().filter(|a| a.state.is_in_flight()) {
        let attempt = Attempt::create(row.id.clone(), run_id.clone(), row.ordinal);
        match (row.pid, row.pid_start_time) {
            (Some(pid), Some(start)) => match probe(pid, start) {
                Liveness::Alive(_) if config.adopt_live_agents => {
                    decisions.push(RecoveryDecision::AdoptedLiveAgent {
                        run_id: run_id.clone(),
                        attempt_id: row.id.clone(),
                        pid,
                    });
                    store.record_event(
                        fence,
                        EventKind::RecoveryDecision,
                        &format!("{{\"adopted\":{pid},\"attempt\":\"{}\"}}", row.id),
                        now_ms,
                    )?;
                    // An adopted agent is still running, so its attempt stays in
                    // flight and the run stays where it is. Nothing further to
                    // classify this pass.
                    return Ok(None);
                }
                Liveness::Alive(_) => {
                    // Terminate: nobody is reading its output or enforcing its
                    // budgets, so leaving it running is worse than killing it.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    decisions.push(RecoveryDecision::TerminatedLiveAgent {
                        run_id: run_id.clone(),
                        attempt_id: row.id.clone(),
                        pid,
                    });
                    let stale = attempt.starting().active(pid, start).stale();
                    store.record_attempt_terminal(fence, &stale, now_ms)?;
                    let reconciled = stale.reconciled();
                    store.record_attempt_reconciled(fence, &reconciled, now_ms)?;
                }
                Liveness::Dead => {
                    let stale = attempt.starting().active(pid, start).stale();
                    store.record_attempt_terminal(fence, &stale, now_ms)?;
                    let reconciled = stale.reconciled();
                    store.record_attempt_reconciled(fence, &reconciled, now_ms)?;
                    decisions.push(RecoveryDecision::AttemptStale {
                        run_id: run_id.clone(),
                        attempt_id: row.id.clone(),
                        reason: format!("pid {pid} is gone; no exit was observed"),
                    });
                }
                Liveness::Recycled { actual_start } => {
                    // §4.7 step 3: "a recycled pid is not your process". The
                    // live process belongs to somebody else and must not be
                    // touched, adopted, or counted as ours.
                    let stale = attempt.starting().active(pid, start).stale();
                    store.record_attempt_terminal(fence, &stale, now_ms)?;
                    let reconciled = stale.reconciled();
                    store.record_attempt_reconciled(fence, &reconciled, now_ms)?;
                    decisions.push(RecoveryDecision::AttemptStale {
                        run_id: run_id.clone(),
                        attempt_id: row.id.clone(),
                        reason: format!(
                            "pid {pid} is alive but started at {actual_start}, not at {start}: \
                             it is a different process"
                        ),
                    });
                }
            },
            // No pid was ever recorded. Either the spawn never happened or the
            // supervisor died between spawning and recording — and there is no
            // way to tell which. `STALE` is the only honest answer, and the
            // repository is the evidence that decides what to do next.
            _ => {
                let stale = attempt.starting().spawn_failed(
                    "no pid was recorded; the supervisor died before or during the spawn",
                );
                store.record_attempt_terminal(fence, &stale, now_ms)?;
                let reconciled = stale.reconciled();
                store.record_attempt_reconciled(fence, &reconciled, now_ms)?;
                decisions.push(RecoveryDecision::AttemptStale {
                    run_id: run_id.clone(),
                    attempt_id: row.id.clone(),
                    reason: "no pid was recorded".to_string(),
                });
            }
        }
    }

    // §5.2: "Every path ends at `RECONCILED` — an attempt is never finished
    // until Conductor has looked at the repository."
    //
    // The probe loop above only sees attempts that were still *in flight*. An
    // attempt whose terminal outcome was recorded a moment before the
    // supervisor died is not in flight and not finished either: it is sitting
    // in `EXITED`/`CRASHED`/`TIMED_OUT`/`STALE` with nobody left to advance it.
    // Recovery is the act of looking, and every remaining path in this function
    // reaches the repository, so the attempts are closed here.
    finalize_terminal_attempts(store, fence, &attempts, now_ms)?;

    // Resolve the crash window: an `INTENDED` effect is decided by asking the
    // world, never by retrying (§4.7, row 22).
    resolve_effects(store, fence, now_ms, decisions)?;

    // Step 4. Locate the workspace.
    let row = store
        .run(&run_id)?
        .ok_or_else(|| WorkerError::Adapter(format!("run {run_id} vanished mid-recovery")))?;
    // "Locate" means look at the world, not only at the database. A run whose
    // row has no `workspace_path` may still have a clone on disk: the gap
    // between `git clone` returning and the store being told is a real crash
    // window, and §4.1 puts the descriptor in the workspace precisely so it can
    // be identified "with **no database at all**". Routing such a run onwards as
    // "no workspace was ever created" would record something the filesystem
    // plainly contradicts — and leave an hour of an agent's work invisible.
    let workspace_path = match row.workspace_path.clone().map(PathBuf::from) {
        Some(path) => path,
        None => match adopt_unrecorded_workspace(store, fence, config, &run_id, now_ms)? {
            Some(path) => path,
            None => {
                // Nothing in the database and nothing on disk: this run never
                // got as far as a workspace. There is no work at risk, so it
                // goes back for repair rather than being blocked.
                store.route_reconciled(
                    fence,
                    ReconciledRoute::Repairing,
                    "no workspace was ever created",
                    now_ms,
                )?;
                decisions.push(RecoveryDecision::Reconciled {
                    run_id,
                    verdict: "NO_CHANGE".to_string(),
                    route: ReconciledRoute::Repairing,
                    verification_needed: false,
                });
                return Ok(None);
            }
        },
    };

    if !workspace_path.exists() {
        // §4.7 step 4: "Absent → run BLOCKED + finding." The work that was in
        // there cannot be recovered and must not be silently forgotten.
        store.record_finding(
            fence,
            &format!("f-{}-WORKSPACE_MISSING", run_id.as_str()),
            "WORKSPACE_MISSING",
            "CRITICAL",
            &format!("{} is gone", workspace_path.display()),
            now_ms,
        )?;
        store.route_reconciled(fence, ReconciledRoute::Blocked, "workspace absent", now_ms)?;
        decisions.push(RecoveryDecision::WorkspaceMissing {
            run_id,
            expected: workspace_path.display().to_string(),
        });
        return Ok(None);
    }

    // Step 5. Diff the repository against the stored baseline.
    let ordinal = attempts.last().map(|a| a.ordinal).unwrap_or(1);
    let Some(baseline) = read_baseline(&config.artifacts_root, &run_id, ordinal)? else {
        // Without a baseline there is nothing to compare against, and a
        // comparison Conductor cannot make must not be guessed at.
        store.record_finding(
            fence,
            &format!("f-{}-BASELINE_MISSING", run_id.as_str()),
            "BASELINE_MISSING",
            "CRITICAL",
            "no baseline artifact, so the workspace cannot be reconciled",
            now_ms,
        )?;
        store.route_reconciled(fence, ReconciledRoute::Blocked, "baseline absent", now_ms)?;
        decisions.push(RecoveryDecision::Reconciled {
            run_id,
            verdict: "CORRUPT".to_string(),
            route: ReconciledRoute::Blocked,
            verification_needed: false,
        });
        return Ok(None);
    };

    let observed = observe(&workspace_path, &baseline)?;
    let reconciliation = conductor_git::reconcile(
        &baseline,
        &observed,
        &config.scope,
        &config.sensitive,
        None,
        None,
    );
    for finding in reconciliation.findings() {
        store.record_finding(
            fence,
            &format!(
                "f-{}-{:?}-{}",
                run_id.as_str(),
                finding.kind,
                finding.path.clone().unwrap_or_default()
            ),
            &format!("{:?}", finding.kind).to_uppercase(),
            "WARNING",
            &finding.detail,
            now_ms,
        )?;
    }

    // Step 6. Ask whether verification still owes work at this tree. Running it
    // is S4's; knowing whether it is owed is recovery's, because that is what
    // decides whether the run can move on.
    let verification_needed = !store.has_valid_verification(&observed.repo.tree_hash)?;

    // Step 7. Classify and route — through the **same** function the attempt
    // path uses (§4.8's policy consultation included). Recovery is also the
    // path a run takes after a human answers an approval, so a policy check
    // that lived only in the attempt path would never see the grant and the
    // granted run would route straight back to `AWAITING_APPROVAL` forever.
    let (route, detail, _request_id) =
        crate::enforce::policy_gate::route_reconciliation(store, fence, &reconciliation, now_ms)
            .map_err(|e| WorkerError::Adapter(format!("policy gate: {e}")))?;
    store.route_reconciled(
        fence,
        route.clone(),
        &format!("recovered: {detail}"),
        now_ms,
    )?;
    decisions.push(RecoveryDecision::Reconciled {
        run_id,
        verdict: reconciliation.verdict.to_string(),
        route,
        verification_needed,
    });
    Ok(Some(reconciliation))
}

/// Attach a workspace that exists on disk but was never recorded.
///
/// The crash window between `git clone` returning and `attach_workspace`
/// committing. Adoption requires the workspace's own descriptor to name this
/// run: a directory that belongs to somebody else, or one that cannot say who it
/// belongs to, is left exactly where it is. §4.1's rule for a workspace
/// Conductor cannot account for is to preserve it, never to write into it — and
/// step 8's orphan scan is what deals with it.
fn adopt_unrecorded_workspace(
    store: &mut Store,
    fence: &Fence,
    config: &RecoveryConfig,
    run_id: &RunId,
    now_ms: i64,
) -> Result<Option<PathBuf>, WorkerError> {
    let candidate = config.workspaces_root.join(run_id.as_str());
    if !candidate.exists() {
        return Ok(None);
    }
    let Ok(descriptor) = conductor_git::read_descriptor(&candidate) else {
        return Ok(None);
    };
    if &descriptor.run_id != run_id {
        return Ok(None);
    }
    store.attach_workspace(
        fence,
        &format!("ws-{}", run_id.as_str()),
        &candidate.to_string_lossy(),
        &config.workspaces_root.to_string_lossy(),
        now_ms,
    )?;
    Ok(Some(candidate))
}

/// Close every attempt that reached a terminal outcome but never reached
/// `RECONCILED`.
///
/// The terminal classification is **not** rewritten — only the state moves. The
/// timeout reason, exit code and signal stay exactly as the supervisor observed
/// them, because recovery did not observe them and has no business restating
/// them.
fn finalize_terminal_attempts(
    store: &mut Store,
    fence: &Fence,
    attempts: &[conductor_store::AttemptRow],
    now_ms: i64,
) -> Result<(), WorkerError> {
    for row in attempts.iter().filter(|a| a.state.is_terminal_outcome()) {
        let Some(terminal) = rebuild_terminal(row, fence.run_id()) else {
            continue;
        };
        store.record_attempt_reconciled(fence, &terminal.reconciled(), now_ms)?;
    }
    Ok(())
}

/// Rebuild the typestate for an attempt from what the database recorded.
///
/// Recovery's whole job is reconstructing state from evidence, and this is that
/// reconstruction for one attempt. It can only produce a terminal phase, so it
/// cannot be used to resurrect an attempt into `ACTIVE`.
fn rebuild_terminal(
    row: &conductor_store::AttemptRow,
    run_id: &RunId,
) -> Option<conductor_core::attempt::TerminalPhase> {
    let starting = Attempt::create(row.id.clone(), run_id.clone(), row.ordinal).starting();
    let (Some(pid), Some(start)) = (row.pid, row.pid_start_time) else {
        // No process was ever recorded, so there is nothing to classify beyond
        // "we do not know".
        return Some(starting.spawn_failed("no process identity was recorded"));
    };
    let active = starting.active(pid, start);
    Some(match row.state {
        AttemptState::Exited => active.exited(row.exit_code.unwrap_or(0)),
        AttemptState::Crashed => match (row.signal, row.exit_code) {
            (Some(signal), _) => active.signalled(signal),
            (None, Some(code)) => active.exited(code),
            // Recorded as crashed with neither a code nor a signal: the row
            // itself is the evidence, and inventing one would be worse.
            (None, None) => active.exited(1),
        },
        AttemptState::TimedOut => active.timed_out_wall(),
        AttemptState::Stale => active.stale(),
        _ => return None,
    })
}

/// Resolve every `INTENDED` effect of this run by re-checking the world.
fn resolve_effects(
    store: &mut Store,
    fence: &Fence,
    now_ms: i64,
    decisions: &mut Vec<RecoveryDecision>,
) -> Result<(), WorkerError> {
    let unresolved: Vec<_> = store
        .unresolved_effects()?
        .into_iter()
        .filter(|row| &row.run_id == fence.run_id())
        .collect();

    for row in unresolved {
        // §4.7's three answers, from the world. Until S5 this was a `bool` plus
        // a heuristic — "the target path exists but does not match ⇒
        // `AMBIGUOUS`" — which is right for a file and wrong for a commit: a
        // workspace directory exists whether or not the commit inside it was
        // made, so a run that crashed *before* committing would have been
        // declared ambiguous and stopped for a human. Each precondition now says
        // for itself which observations are decisive.
        match check_precondition(&row.precondition) {
            PreconditionAnswer::Held => {
                // It happened. Record the receipt and do **not** do it again.
                store.confirm_effect(
                    fence,
                    &row.operation_id,
                    "{\"resolved_by\":\"recovery\",\"precondition\":\"held\"}",
                    now_ms,
                )?;
                decisions.push(RecoveryDecision::EffectConfirmed {
                    operation_id: row.operation_id.to_string(),
                });
            }
            PreconditionAnswer::NotHeld => {
                store.fail_effect(
                    fence,
                    &row.operation_id,
                    "{\"resolved_by\":\"recovery\",\"precondition\":\"absent\"}",
                    now_ms,
                )?;
                decisions.push(RecoveryDecision::EffectNotDone {
                    operation_id: row.operation_id.to_string(),
                });
            }
            PreconditionAnswer::Indeterminate(why) => {
                let detail = format!("{why} ({})", row.kind.did_it_happen_question());
                store.mark_effect_ambiguous(fence, &row.operation_id, &detail, now_ms)?;
                store.record_finding(
                    fence,
                    &format!("f-{}-EFFECT_AMBIGUOUS", fence.run_id().as_str()),
                    "EFFECT_AMBIGUOUS",
                    "CRITICAL",
                    &detail,
                    now_ms,
                )?;
                decisions.push(RecoveryDecision::EffectAmbiguous {
                    operation_id: row.operation_id.to_string(),
                    detail,
                });
            }
        }
    }
    Ok(())
}

/// Read the baseline artifact an attempt wrote.
///
/// Tries the newest attempt first and walks back: a run that crashed during
/// attempt 3 still has attempt 1's baseline, and any of them describes the same
/// starting tree.
fn read_baseline(
    artifacts_root: &std::path::Path,
    run_id: &RunId,
    newest_ordinal: i64,
) -> Result<Option<Baseline>, WorkerError> {
    let root = ArtifactRoot::new(artifacts_root);
    for ordinal in (1..=newest_ordinal.max(1)).rev() {
        let path = root.attempt_dir(run_id, ordinal).join("baseline.json");
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(baseline) = serde_json::from_str::<Baseline>(&raw)
        {
            return Ok(Some(baseline));
        }
    }
    Ok(None)
}

/// Every attempt state recovery treats as in flight, for reporting.
pub fn in_flight_states() -> Vec<AttemptState> {
    AttemptState::ALL
        .iter()
        .copied()
        .filter(AttemptState::is_in_flight)
        .collect()
}
