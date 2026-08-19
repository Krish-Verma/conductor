//! The repair loop — master plan §4.6, and the slice's acceptance property.
//!
//! > **Acceptance property:** no configuration of the fake agent can produce
//! > more than `max_attempts` agent invocations. Asserted by counting spawns.
//!
//! # Two bounds, and only one of them is the safety property
//!
//! [`super::breaker::decide`] is the *informative* bound. It reads §4.6's
//! history and stops with a reason a person can act on: the same failure twice,
//! an oscillation, an empty edit, a budget spent. Everything about it is a
//! judgement — which failures are "the same", whether the loop "progressed" —
//! and judgements can be wrong.
//!
//! [`ceiling`] is the *safety* bound. It is arithmetic over the configuration
//! and a `COUNT(*)` over durable rows, it knows nothing about failures, and it is
//! checked immediately before every spawn **in addition to** `decide`, never
//! instead of it. If every loop-breaker in §4.6 were bypassed, mis-specified or
//! silently broken by a change to `normalize`, this is what still holds.
//!
//! That redundancy is deliberate and it is worth being explicit about what it
//! costs: with a correct `decide`, the ceiling is never the thing that stops a
//! run. Its value shows up in exactly the case §4.7 exists for — a crash between
//! the spawn and the observation, where the durable history under-counts the
//! invocations that actually happened and `decide` would happily authorise
//! another. `attempt` rows are written and committed **before** `spawn()`, so the
//! ceiling's count is never the one that is short.
//!
//! # Why the count is `attempt` rows and not a counter
//!
//! §4.7 kills and restarts the process running this loop. A counter in memory
//! resets; a `COUNT(*)` does not. `crate::worker::run_one_attempt` commits the
//! attempt row (`CREATED`) and then commits `STARTING`, both before it builds a
//! command — so a process that dies at any point after the agent exists has
//! already recorded that it existed. The count is therefore an over-approximation
//! by at most one in the one direction that is safe: it can say a spawn happened
//! when the spawn failed, and it can never say one did not happen when it did.

use std::path::PathBuf;

use conductor_agent::AgentAdapter;
use conductor_core::{AttemptOutcome, Fence, RunId, RunState, TaskState};
use conductor_store::{Store, StoreError};

use super::breaker::{Decision, StopReason, decide};
use super::config::{RepairConfig, SessionPolicy, session_for_attempt, session_id_for};
use super::observation::{self, Observation, ObservationError};
use super::packet::{self, RepairPacket};
use crate::vertical::{Vertical, VerticalConfig, VerticalOutcome, run_task_with_session};
use crate::worker::WorkerError;

/// The hard ceiling on agent invocations for one run.
///
/// ```text
/// ceiling = 1 + max_attempts + max_infra_retries
///           ^   ^              ^
///           |   |              row 8's "infra retry ×1", which §4.7 exempts
///           |   |              from the work budget and which therefore has
///           |   |              no other bound at all
///           |   §4.6's repairs
///           the initial attempt, which is not a repair
/// ```
///
/// With §4.6's printed defaults that is `1 + 2 + 1 = 4`.
///
/// **A function of configuration only.** Nothing an agent does — how it fails,
/// how often, how creatively — appears on the right-hand side. That is the
/// difference between this and every other limit in the slice: the others are
/// answers to "should we try again?", and this one is the answer to "how many
/// times may we possibly have tried?", which a person can compute from a YAML
/// file before any agent runs.
///
/// Saturating, because a configuration is user input and a ceiling that wrapped
/// to zero — or panicked — would be a safety bound defeated by arithmetic.
pub const fn ceiling(config: &RepairConfig) -> usize {
    1usize
        .saturating_add(config.max_attempts)
        .saturating_add(config.max_infra_retries)
}

/// Why repair handed the run to a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationReason {
    /// §4.6 said stop, and said why.
    Breaker(StopReason),
    /// [`ceiling`] was reached. Reported separately because it means something
    /// different: not "this task is beyond the agent" but "this run has had
    /// every invocation its configuration allows, whatever the reasons were".
    Ceiling {
        /// The configured ceiling.
        ceiling: usize,
        /// Agent invocations recorded for this run.
        invocations: usize,
    },
}

impl std::fmt::Display for EscalationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EscalationReason::Breaker(reason) => write!(f, "{reason:?}"),
            EscalationReason::Ceiling {
                ceiling,
                invocations,
            } => write!(
                f,
                "the run has had {invocations} agent invocation(s) and its \
                 configuration allows {ceiling}"
            ),
        }
    }
}

/// One agent attempt, and everything repair recorded about it.
#[derive(Debug)]
pub struct Attempted {
    /// Its ordinal.
    pub ordinal: i64,
    /// The packet it was given (§6.5).
    pub packet: RepairPacket,
    /// What repair made of it, when repair has anything to say.
    pub observation: Option<Observation>,
    /// Everything the vertical produced.
    pub vertical: Vertical,
}

/// What one pass of the loop did.
///
/// The two large variants are boxed: a `Step` is returned from every pass
/// including the ones that ran nothing, and an enum whose size is set by its
/// rarest member costs that on every call.
#[derive(Debug)]
pub enum Step {
    /// One agent attempt ran.
    Attempted(Box<Attempted>),
    /// The task reached `COMPLETE`.
    Complete(Box<Vertical>),
    /// Repair stopped and the run is with a person.
    Escalated {
        /// Why.
        reason: EscalationReason,
    },
    /// The run is in a state repair does not own — `BLOCKED`,
    /// `AWAITING_APPROVAL`, `AWAITING_REVIEW`, or already terminal.
    ///
    /// Deliberately not an error and deliberately not an escalation: §4.8 put
    /// the run there, and repair overruling that would be repair deciding a
    /// policy-sensitive change may be retried away.
    Handed {
        /// Where the run is.
        state: RunState,
        /// What that means.
        reason: String,
    },
}

/// How the whole loop ended.
#[derive(Debug)]
pub enum RepairOutcome {
    /// `COMPLETE`, after `invocations` agent invocations.
    Complete {
        /// How many times an agent was started for this run.
        invocations: usize,
        /// The final vertical.
        vertical: Box<Vertical>,
    },
    /// A person is needed.
    Escalated {
        /// Why.
        reason: EscalationReason,
        /// How many times an agent was started for this run.
        invocations: usize,
    },
    /// The run stopped somewhere repair does not own.
    Handed {
        /// Where.
        state: RunState,
        /// What that means.
        reason: String,
        /// How many times an agent was started for this run.
        invocations: usize,
    },
}

/// Anything the repair loop can fail with.
#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    /// The store said no.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// The durable history could not be read.
    #[error("repair history: {0}")]
    Observation(#[from] ObservationError),
    /// The attempt itself failed.
    #[error("attempt: {0}")]
    Worker(#[from] WorkerError),
    /// The task or its run is not where the loop needs it.
    #[error("{0}")]
    NotDriveable(String),
}

/// Drive one task to `COMPLETE`, to a person, or to a state repair does not own.
///
/// Terminates because [`repair_once`] either ends the loop or adds one durable
/// `attempt` row, and [`ceiling`] refuses to spawn past a fixed count of them.
/// The explicit iteration guard is not that argument's crutch — it is the
/// admission that a slice whose entire subject is "loops are bounded" should not
/// contain a loop whose bound is an argument.
pub fn drive(
    store: &mut Store,
    adapter: &dyn AgentAdapter,
    vertical: &VerticalConfig,
    config: &RepairConfig,
    observer: &mut dyn crate::vertical::VerticalObserver,
) -> Result<RepairOutcome, RepairError> {
    let guard = ceiling(config).saturating_add(2);
    // Read once, before anything runs. `active_run_for_task` is defined by the
    // partial index over *non-terminal* runs, so a run that reached `COMPLETE`
    // stops answering to it — and asking afterwards would turn success into "no
    // active run".
    let run_id = run_of(store, vertical)?;
    for _ in 0..guard {
        match repair_once(store, adapter, vertical, config, observer)? {
            Step::Attempted(_) => continue,
            Step::Complete(v) => {
                return Ok(RepairOutcome::Complete {
                    invocations: invocations(store, &run_id)?,
                    vertical: v,
                });
            }
            Step::Escalated { reason } => {
                return Ok(RepairOutcome::Escalated {
                    invocations: invocations(store, &run_id)?,
                    reason,
                });
            }
            Step::Handed { state, reason } => {
                return Ok(RepairOutcome::Handed {
                    invocations: invocations(store, &run_id)?,
                    state,
                    reason,
                });
            }
        }
    }
    Err(RepairError::NotDriveable(format!(
        "the repair loop ran {guard} times without terminating, which the \
         invocation ceiling of {} should have made impossible",
        ceiling(config)
    )))
}

/// One pass: check the bounds, run at most one attempt, record what it was.
///
/// The order is the safety property. The ceiling is consulted from durable state
/// **before** `decide`, and both before anything is re-opened for a claim, so
/// there is no path from "the loop decided to continue" to `spawn()` that skips
/// either.
pub fn repair_once(
    store: &mut Store,
    adapter: &dyn AgentAdapter,
    vertical: &VerticalConfig,
    config: &RepairConfig,
    observer: &mut dyn crate::vertical::VerticalObserver,
) -> Result<Step, RepairError> {
    let run_id = run_of(store, vertical)?;
    let task = store
        .task(&vertical.task_id)?
        .ok_or_else(|| RepairError::NotDriveable(format!("no task {}", vertical.task_id)))?;
    let run = store
        .run(&run_id)?
        .ok_or_else(|| RepairError::NotDriveable(format!("run {run_id} vanished")))?;
    let fence = Fence::new(run_id.clone(), run.lease_epoch);

    // ---- the hard ceiling, from durable state (§4.6's acceptance property) --
    let spawned = invocations(store, &run_id)?;
    let allowed = ceiling(config);
    if spawned >= allowed {
        let reason = EscalationReason::Ceiling {
            ceiling: allowed,
            invocations: spawned,
        };
        return escalate(store, &fence, vertical, run.state, reason);
    }

    // ---- §4.6's loop-breakers and budget -----------------------------------
    let history = observation::history_for_run(store, &run_id)?;
    if let Decision::Stop(reason) = decide(&history, config, task.attempt_budget) {
        return escalate(
            store,
            &fence,
            vertical,
            run.state,
            EscalationReason::Breaker(reason),
        );
    }

    // ---- the packet, built from durable state before anything moves --------
    let observations = observation::observations_for_run(store, &run_id)?;
    let next_ordinal = store.next_attempt_ordinal(&run_id)?;
    let workspace = run
        .workspace_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| vertical.workspaces_root.join(run_id.as_str()));
    let packet = packet::build(
        run_id.as_str(),
        next_ordinal,
        &observations,
        &history,
        &workspace,
        config,
        // Saturating even though the check above guarantees `spawned < allowed`.
        // The guarantee is the thing under test: an arithmetic panic here would
        // report a removed ceiling as a subtraction overflow rather than as the
        // unbounded loop it is, and in a release build with overflow checks off
        // it would wrap to a very large "budget remaining" and print that to the
        // agent.
        allowed.saturating_sub(spawned),
    );

    // ---- and it is what the agent is told (wired at S12) --------------------
    //
    // Until here, this packet was **built and never delivered**: it was returned
    // in `Attempted { packet, … }` for reporting, while the attempt itself was
    // launched with whatever the caller had configured — which for every attempt
    // of a run was the same thing. §6.5's `do_not_retry` list exists because
    // *"that last field is what stops attempt 2 from being attempt 1 again"*, and
    // attempt 2 had never seen it. Same class of defect as ADR-0017's: built,
    // unit-tested, described in a completion report, and not on the product path.
    //
    // Composed onto the implementation packet rather than sent alone, because
    // §6.5 says this packet *"adds only"* those six fields — a repairing agent
    // still needs the objective, the scope, the acceptance criteria and the
    // verification commands. A composition failure ends the repair rather than
    // falling back to the plain implementation packet: that fallback is exactly
    // "attempt 2 is attempt 1 again", silently.
    // ---- which of §6.5's three packets this attempt gets ---------------------
    //
    // Part 9 specifies all three between rows 2, 3 and 7, and the discriminator is
    // **how the previous attempt ended** — not what verification said about it:
    //
    // | row | previous attempt | next attempt is told |
    // |-----|------------------|----------------------|
    // | 2 | crashed, nothing survived | *"new attempt, **same packet**"* — the implementation packet |
    // | 3 | crashed, work survived | *"verify current tree; **continuation packet**"* |
    // | 7 | **exited**, verification failed | §4.6's repair packet |
    //
    // The verification result cannot be the discriminator, and that is the subtle
    // part: `observation::observe` gives a verification failure precedence over the
    // attempt's terminal state, so a crash whose surviving work then fails a check
    // is `ObservationKind::Failed` — indistinguishable from row 7 by kind alone.
    // What separates them is whether an agent ever *finished a turn*. An agent that
    // exited made choices that can be wrong, and §6.5's `do_not_retry` list is
    // about not repeating them. An agent that died made no choices worth avoiding;
    // its successor needs to know what is already in the tree, which is what §6.5's
    // continuation packet carries — together with the sentence saying the previous
    // agent's reasoning is gone.
    //
    // `attempt.outcome` is durable (§5.2's eight attempt states), so this survives
    // the restart §4.7 exists for.
    let previous = previous_attempt(store, &run_id)?;
    let unfinished = matches!(
        previous.map(|(_, outcome)| outcome),
        Some(AttemptOutcome::Crashed | AttemptOutcome::TimedOut | AttemptOutcome::Stale)
    );

    let chosen;
    let vertical = if observations.is_empty() && !unfinished {
        // Attempt 1: no history at all. The worker derives the implementation
        // packet, which is what an attempt that is not a repair should get.
        vertical
    } else if unfinished {
        // Rows 2 and 3. Measured, not assumed: `observe_run` re-observes the
        // workspace against the baseline the dead attempt stored.
        let (ordinal, _) = previous.expect("`unfinished` implies a previous attempt");
        let observed = crate::packet::continuation::observe_run(
            store,
            &run_id,
            &workspace,
            &vertical.artifacts_root,
            ordinal,
            &task_scope(store, &run_id)?,
            &vertical.sensitive,
        );
        if observed.is_empty() {
            // Row 2: nothing survived, so there is no observed reality to add and
            // §6.5's continuation packet would carry an empty half. The row says
            // *"same packet"*, and this is it.
            vertical
        } else {
            let continuation = crate::packet::continuation::build(store, &run_id, &observed)
                .map_err(|e| {
                    RepairError::NotDriveable(format!(
                        "§6.5's continuation packet cannot be built for run {run_id}: {e}"
                    ))
                })?;
            chosen = VerticalConfig {
                instructions: Some(continuation.to_yaml()),
                ..vertical.clone()
            };
            &chosen
        }
    } else {
        // Row 7. Composed onto the implementation packet, because §6.5 says this
        // packet *"adds only"* those six fields — a repairing agent still needs the
        // objective, the scope, the criteria and the verification commands. A
        // composition failure ends the repair rather than falling back to the plain
        // implementation packet: that fallback is "attempt 2 is attempt 1 again",
        // silently.
        let composed = crate::packet::repair::build(store, &run_id, &packet).map_err(|e| {
            RepairError::NotDriveable(format!(
                "§6.5's repair packet cannot be composed for run {run_id}: {e}"
            ))
        })?;
        chosen = VerticalConfig {
            instructions: Some(composed.to_yaml()),
            ..vertical.clone()
        };
        &chosen
    };

    // ---- §4.6's session policy ---------------------------------------------
    //
    // "a stuck agent's context *is* the problem, and resuming re-imports the
    // stuckness." The previous session id is looked up so that discarding it is
    // a decision rather than an absence.
    let policy = session_for_attempt(next_ordinal, config);
    let session = session_id_for(
        policy,
        adapter.capabilities().session_resume,
        previous_session(store, &run_id)?.as_deref(),
    );

    // ---- `REPAIRING → READY`, through the guarded path ---------------------
    match run.state {
        RunState::Repairing => {
            store.reopen_for_repair(
                &fence,
                &format!(
                    "repair attempt {next_ordinal}: {} left",
                    allowed.saturating_sub(spawned)
                ),
                now_ms(),
            )?;
            if store
                .task(&vertical.task_id)?
                .is_some_and(|t| t.state == TaskState::Repairing)
            {
                store.set_task_state(&vertical.task_id, TaskState::Ready)?;
            }
        }
        RunState::Ready | RunState::Pending => {}
        state => {
            return Ok(Step::Handed {
                state,
                reason: format!(
                    "the run is in {state}, which is not a state repair may take a \
                     run out of"
                ),
            });
        }
    }

    // ---- one attempt, through S5's machinery and no copy of it -------------
    let result = run_task_with_session(store, adapter, vertical, session.as_deref(), observer)?;

    if let VerticalOutcome::Complete { .. } = result.outcome {
        return Ok(Step::Complete(Box::new(result)));
    }

    let observed =
        observation::observe(&result.attempt, result.verification.as_ref(), next_ordinal);
    if let Some(observation) = &observed {
        // The epoch moved when the run was claimed, so the fence is re-read
        // rather than reused: a write carrying the pre-claim epoch is exactly
        // what §4.7's fencing rejects.
        let after = store.run(&run_id)?.ok_or_else(|| {
            RepairError::NotDriveable(format!("run {run_id} vanished mid-repair"))
        })?;
        let fence = Fence::new(run_id.clone(), after.lease_epoch);
        observation::record(store, &fence, observation, now_ms())?;
    }

    Ok(Step::Attempted(Box::new(Attempted {
        ordinal: next_ordinal,
        packet,
        observation: observed,
        vertical: result,
    })))
}

/// The task's declared scope, read from the row the run belongs to.
///
/// The same value [`crate::vertical`] builds for reconciliation, from the same
/// column, so the verdict this reports and the verdict the run was routed by
/// cannot disagree about what was in scope.
fn task_scope(store: &Store, run_id: &RunId) -> Result<conductor_git::Scope, RepairError> {
    let run = store
        .run(run_id)?
        .ok_or_else(|| RepairError::NotDriveable(format!("run {run_id} vanished")))?;
    let task_id = conductor_core::TaskId::new(run.task_id.clone())
        .map_err(|e| RepairError::NotDriveable(e.to_string()))?;
    let task = store
        .task(&task_id)?
        .ok_or_else(|| RepairError::NotDriveable(format!("no task {task_id}")))?;
    Ok(conductor_git::Scope::new(task.scope_globs))
}

/// The highest-ordinal attempt this run has, and how it ended.
///
/// `None` before the first attempt. An attempt row with no `outcome` yet — one
/// still in flight, or one whose worker died between `STARTING` and recording a
/// terminal state — is reported as [`AttemptOutcome::Stale`], because that is what
/// §5.2 means by `STALE`: *"we do not know"*, and unknown must not be recorded as
/// known. It routes to the same place a crash does, which is the safe direction:
/// the successor is told what is in the tree rather than what to avoid.
fn previous_attempt(
    store: &Store,
    run_id: &RunId,
) -> Result<Option<(i64, AttemptOutcome)>, RepairError> {
    Ok(store
        .attempts_for_run(run_id)?
        .into_iter()
        .max_by_key(|row| row.ordinal)
        .map(|row| (row.ordinal, row.outcome.unwrap_or(AttemptOutcome::Stale))))
}

/// How many times an agent has been started for this run — the durable count.
///
/// One `attempt` row per invocation, written before `spawn()`. See the module
/// documentation for why this is the number the ceiling is checked against.
pub fn invocations(store: &Store, run_id: &RunId) -> Result<usize, RepairError> {
    Ok(store.attempts_for_run(run_id)?.len())
}

/// Move the run and its task to `AWAITING_REVIEW`, recording why.
///
/// Only from `REPAIRING`, and through the guarded transition. A run that is
/// somewhere else is *handed* rather than escalated: §5.2 does not draw an edge
/// from every state to `AWAITING_REVIEW`, and repair inventing one would be a
/// second, weaker way to write `run.state`.
fn escalate(
    store: &mut Store,
    fence: &Fence,
    vertical: &VerticalConfig,
    state: RunState,
    reason: EscalationReason,
) -> Result<Step, RepairError> {
    if state != RunState::Repairing {
        return Ok(Step::Handed {
            state,
            reason: format!("{reason}, and the run is in {state} rather than REPAIRING"),
        });
    }
    store.escalate_from_repairing(fence, &reason.to_string(), now_ms())?;
    if store
        .task(&vertical.task_id)?
        .is_some_and(|t| t.state == TaskState::Repairing)
    {
        store.set_task_state(&vertical.task_id, TaskState::AwaitingReview)?;
    }
    Ok(Step::Escalated { reason })
}

/// The run this task is executing.
fn run_of(store: &Store, vertical: &VerticalConfig) -> Result<RunId, RepairError> {
    store
        .active_run_for_task(&vertical.task_id)?
        .ok_or_else(|| {
            RepairError::NotDriveable(format!("task {} has no active run", vertical.task_id))
        })
}

/// The session the previous attempt used, when the adapter assigned one.
fn previous_session(store: &Store, run_id: &RunId) -> Result<Option<String>, RepairError> {
    let session: Option<String> = store
        .conn()
        .query_row(
            "SELECT agent_session_id FROM attempt WHERE run_id = ?1
              ORDER BY ordinal DESC LIMIT 1",
            rusqlite::params![run_id.as_str()],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    Ok(session)
}

/// Whether an attempt with this ordinal starts clean — re-exported so callers
/// can report the decision without reaching into `config`.
pub fn session_policy(ordinal: i64, config: &RepairConfig) -> SessionPolicy {
    session_for_attempt(ordinal, config)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
