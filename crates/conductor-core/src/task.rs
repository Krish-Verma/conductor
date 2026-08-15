//! §5.2's task machine, and S5's minimal task spec.
//!
//! # The legality table is the task's half of one guarantee
//!
//! §5.2 lists two invalid transitions: `RUNNING → COMPLETE`, and "any `→
//! COMPLETE` without verification bound to the final tree hash". S3 made the
//! first unrepresentable for a **run** — [`RunState::leave_running`] takes a
//! [`TerminalAttempt`] and returns one destination, so there is no second
//! argument to pass — and S4 preserved it by giving `ReconciledRoute::Complete`
//! a payload only the completion gate can mint.
//!
//! Neither covers the `task` row. A task's state is a second place the same lie
//! can be written, and it is written by a different statement. So the table
//! below refuses `RUNNING → COMPLETE` explicitly, and `conductor-store` routes
//! every task-state write through it.
//!
//! The two guarantees are different in kind and both are wanted: the run's is
//! *unrepresentable*, the task's is *refused at runtime*. The run's is stronger;
//! the task's covers a column the run's type cannot see.
//!
//! [`RunState::leave_running`]: crate::RunState::leave_running
//! [`TerminalAttempt`]: crate::TerminalAttempt
//!
//! # Where this table departs from §5.2's diagram, and why
//!
//! Three places. Each is recorded here rather than resolved silently, because
//! CLAUDE.md forbids changing architecture quietly and because a legality table
//! is exactly the kind of artefact whose gaps are invisible once written.
//!
//! 1. **`BLOCKED` has no outgoing edge in the diagram**, yet §5.2's terminal set
//!    is `COMPLETE`/`CANCELLED`/`SUPERSEDED`. Taken literally, a blocked task is
//!    a non-terminal state nothing can leave — a trap. Only the human decisions
//!    the diagram's own bottom row offers are permitted: `CANCELLED` and
//!    `SUPERSEDED`.
//! 2. **`REPAIRING → RECONCILING` is drawn, and it cannot work.** §4.6's repair
//!    is an agent invocation, and §5.2's only route to one is
//!    `READY ──claim+eligibility──► RUNNING`; §4.7's claim additionally
//!    *preserves* `RECONCILING` instead of setting `RUNNING`, so a run pushed
//!    from `REPAIRING` to `RECONCILING` would be re-reconciled without any agent
//!    ever running. S3's completion report already recorded the working edge as
//!    `REPAIRING → READY`, owned by S6. That is the edge here.
//! 3. **`AWAITING_APPROVAL` has one drawn exit, `(granted) → READY`.** A denial
//!    then has nowhere to go, which is the permanently-stranded shape S3 found
//!    at a kill point. `AWAITING_APPROVAL → AWAITING_REVIEW` is permitted so a
//!    denied request reaches a human instead of a dead end. **S8 owns
//!    approvals** and should revisit it with the approval machine in hand.
//!
//! `→ CANCELLED` is permitted from every non-terminal state, because §5.2's
//! authority line gives it to a human without qualification. `→ SUPERSEDED` is
//! **not**: acceptance row 21 requires a run in flight under plan v3 to "finish
//! under v3", so a task that has started work cannot be superseded out from
//! under itself.

use serde::{Deserialize, Serialize};

use crate::ids::{IdError, TaskId};
use crate::state::TaskState;

/// A transition §5.2 does not draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TransitionError {
    /// Where the task is.
    pub from: TaskState,
    /// Where something tried to put it.
    pub to: TaskState,
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} → {} is not a transition the task machine has (§5.2)",
            self.from, self.to
        )
    }
}

impl std::error::Error for TransitionError {}

impl TaskState {
    /// The states this one may move to, per §5.2 and the module docs above.
    pub fn successors(&self) -> &'static [TaskState] {
        use TaskState::*;
        match self {
            Pending => &[Ready, Cancelled, Superseded],
            // `READY → BLOCKED` is §5.2's fifth correction, forced by S9.
            //
            // The diagram labels this edge `READY ──claim+eligibility──►
            // RUNNING`, which names two gates and draws only the outcome where
            // both pass. §4.2's gate exists precisely to *refuse*, and
            // acceptance row 30 says a refusal leaves the attempt unstarted in
            // `BLOCKED` — a destination this table could not reach from
            // `READY`. Until S9 the contradiction was invisible because nothing
            // called the gate: it was a pure function with no call site, which
            // is exactly why the master plan scored row 30 `NOT RUN` rather
            // than `PASS`.
            //
            // `RUNNING → BLOCKED` is deliberately **not** added instead. §4.8's
            // "every exit from `RUNNING` passes through reconciliation" is the
            // invariant that makes an agent's self-report non-authoritative,
            // and a run that never launched an agent has nothing to reconcile.
            // Refusing before `RUNNING` keeps both statements true.
            Ready => &[Running, Blocked, Cancelled, Superseded],
            // §4.8: "Every exit from RUNNING passes through it — success,
            // crash, timeout, cancel." Reconciliation, or a human stopping the
            // whole thing. There is no third door, and COMPLETE is emphatically
            // not one.
            Running => &[Reconciling, Cancelled],
            // `REPAIRING` is not in §5.2's fan-out from `RECONCILING`, and §4.8
            // requires it: `NO_CHANGE` means "attempt failed to act → repair or
            // review". S3 already routes that verdict to `REPAIRING`, so the
            // edge exists in the product and was missing only from the drawing.
            //
            // `COMPLETE` is deliberately **not** here. §5.2 puts it downstream
            // of `VERIFYING`, and §4.5's criterion 1 — "PASS at the current tree
            // hash" — is decided there. A run whose results are all cache hits
            // still passes through `VERIFYING`; the lookup is what makes that
            // cheap, not a reason to skip the state.
            Reconciling => &[
                AwaitingApproval,
                Verifying,
                Blocked,
                AwaitingReview,
                Repairing,
                Cancelled,
            ],
            // `AWAITING_APPROVAL → RECONCILING` is §5.2's sixth correction,
            // forced by S9 wiring acceptance rows 12 and 13.
            //
            // The diagram's `(granted)` arrow points back at `READY`, and
            // `READY` re-runs the agent — which **destroys the approved work**.
            // `ensure_workspace` re-captures the baseline from the workspace as
            // it now stands, so the very change a human just authorised becomes
            // part of the new baseline, the next attempt reconciles as
            // `NO_CHANGE`, and the approval authorised nothing. This is the
            // same failure the `REPAIRING → RECONCILING` correction found from
            // the other direction, and `resume_task`'s own documentation
            // already describes the mechanism.
            //
            // `RECONCILING` is the destination that works, and it is not a new
            // mechanism: the claim predicate already accepts `RECONCILING` and
            // deliberately preserves it, and §4.7's recovery path reconciles
            // against the **stored baseline artifact** rather than re-capturing
            // one. So a granted run rejoins exactly the path a crashed-then-
            // resumed run takes, with no second agent invocation, and the work
            // the human approved is the work that gets verified.
            //
            // `READY` is kept as well: a denial or a plan revision may legitimately
            // want a fresh attempt, and S13's review outcomes will need it.
            AwaitingApproval => &[Ready, Reconciling, AwaitingReview, Cancelled],
            // Row 8: an INCONCLUSIVE check retried and still undecided ends at
            // AWAITING_REVIEW. Row 24's policy-over-green-tests route is
            // deliberately absent — S7 owns it and will have to add it here,
            // which is the point.
            Verifying => &[Complete, Repairing, AwaitingReview, Cancelled],
            Blocked => &[Cancelled, Superseded],
            // The diagram's four labels: accept · repair · revise · stop.
            AwaitingReview => &[Complete, Repairing, Superseded, Cancelled],
            // §4.6: "escalate_after: 2 → AWAITING_REVIEW". READY rather than
            // RECONCILING — see the module docs.
            Repairing => &[Ready, Complete, AwaitingReview, Cancelled],
            Complete | Cancelled | Superseded => &[],
        }
    }

    /// Check a transition. `Ok(())` means §5.2 draws it.
    ///
    /// A self-transition is always refused: writing the state a task is already
    /// in looks like progress and is not, and the store's task update would
    /// report a row changed when nothing happened.
    pub fn transition_to(&self, to: TaskState) -> Result<(), TransitionError> {
        if self.successors().contains(&to) {
            Ok(())
        } else {
            Err(TransitionError { from: *self, to })
        }
    }
}

/// Why a task spec is unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskSpecError {
    /// `id` is not a usable identifier.
    #[error("task id: {0}")]
    Id(#[from] IdError),
    /// `objective` is blank.
    #[error("the task has no objective, so nothing tells the agent what to do")]
    NoObjective,
    /// `scope` is empty.
    #[error(
        "the task declares no scope globs; every change would be out of scope, \
         and a reader cannot tell that from 'everything is permitted'"
    )]
    EmptyScope,
    /// `verification_profile` is blank.
    #[error(
        "the task names no verification profile; §4.5 makes verification \
         authoritative, and a task with no checks completes on the agent's word"
    )]
    NoVerificationProfile,
    /// `attempt_budget` is not positive.
    #[error("the attempt budget must be at least 1")]
    ZeroAttemptBudget,
}

/// S5's minimal task-spec file — **not** the plan ledger.
///
/// § Part 8's S5 scope says "minimal task-spec file (not yet the plan ledger)".
/// S11 replaces this entirely with `.conductor/plans/vN/plan.yaml`, so it
/// carries exactly what a run needs and nothing that would have to be migrated:
/// who the task is, what it is for, what it may touch, what proves it, and how
/// many tries it gets.
///
/// Deliberately absent: which adapter runs it (§3.1 puts that in
/// `project.yaml`), plan version (S11), policy (S7), approvals (S8),
/// dependencies and acceptance-criterion bindings (S11 — and inventing a
/// half-version of those here is exactly what would have to be unpicked).
///
/// **No `deny_unknown_fields`.** A spec written for a later Conductor must still
/// load; the fields it does not know about are the later Conductor's business.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    /// `task.id` — `T-0012`.
    pub id: String,
    /// What the task is for. Reaches the agent, and the reviewer.
    #[serde(default)]
    pub objective: String,
    /// `task.scope_globs` (§4.8's scope argument).
    #[serde(default)]
    pub scope: Vec<String>,
    /// `task.verification_profile` — a path, relative to the repository root.
    #[serde(default)]
    pub verification_profile: String,
    /// `task.attempt_budget`. Part 5.1's column defaults to 3.
    #[serde(default = "default_attempt_budget")]
    pub attempt_budget: i64,
}

fn default_attempt_budget() -> i64 {
    3
}

/// A [`TaskSpec`] that has been checked.
///
/// Separate type, private fields: everything downstream takes this rather than
/// the raw spec, so "was it validated?" is answered by the type rather than by
/// remembering to call a function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidatedTaskSpec {
    id: TaskId,
    objective: String,
    scope: Vec<String>,
    verification_profile: String,
    attempt_budget: i64,
}

impl TaskSpec {
    /// Check the spec. The only way to obtain a [`ValidatedTaskSpec`].
    pub fn validate(&self) -> Result<ValidatedTaskSpec, TaskSpecError> {
        let id = TaskId::new(self.id.trim())?;
        if self.objective.trim().is_empty() {
            return Err(TaskSpecError::NoObjective);
        }
        if self.scope.iter().all(|glob| glob.trim().is_empty()) {
            return Err(TaskSpecError::EmptyScope);
        }
        if self.verification_profile.trim().is_empty() {
            return Err(TaskSpecError::NoVerificationProfile);
        }
        if self.attempt_budget < 1 {
            return Err(TaskSpecError::ZeroAttemptBudget);
        }
        Ok(ValidatedTaskSpec {
            id,
            objective: self.objective.trim().to_string(),
            scope: self.scope.clone(),
            verification_profile: self.verification_profile.trim().to_string(),
            attempt_budget: self.attempt_budget,
        })
    }
}

impl ValidatedTaskSpec {
    /// `task.id`.
    pub fn id(&self) -> &TaskId {
        &self.id
    }

    /// What the task is for.
    pub fn objective(&self) -> &str {
        &self.objective
    }

    /// `task.scope_globs`.
    pub fn scope(&self) -> &[String] {
        &self.scope
    }

    /// Where the verification profile lives, relative to the repository root.
    pub fn verification_profile(&self) -> &str {
        &self.verification_profile
    }

    /// How many attempts the task gets.
    pub fn attempt_budget(&self) -> i64 {
        self.attempt_budget
    }
}
