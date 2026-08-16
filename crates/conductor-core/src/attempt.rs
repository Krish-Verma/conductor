//! The attempt lifecycle — master plan §5.2, "Attempt (8 states)".
//!
//! ```text
//! CREATED → STARTING → ACTIVE ─┬─► EXITED     ─┐
//!                              ├─► CRASHED    ─┤
//!                              ├─► TIMED_OUT  ─┼─► RECONCILED (terminal)
//!                              └─► STALE      ─┘
//! ```
//!
//! **Why this is a typestate and not an enum with a `transition()` function.**
//! §5.2 requires that `RECONCILING` be "mandatory and unskippable — enforced in
//! the type system, not by convention", and that `RUNNING → COMPLETE` be
//! invalid. A runtime check enforces that only on the paths somebody remembered
//! to route through it. Here, `Attempt<Active>` simply has no `reconciled()`
//! method to call and no way to synthesise the [`TerminalAttempt`] that a run
//! needs in order to leave `RUNNING`: the illegal transition is not rejected, it
//! is unwritable.
//!
//! **`STALE` is not `CRASHED`.** `CRASHED` means an exit was observed and it was
//! not zero. `STALE` means we do not know what happened — the process is gone
//! and nobody saw it go. The [`Termination`] a `STALE` attempt carries therefore
//! has no exit code and no signal, and there is no method anywhere that supplies
//! one afterwards.

use std::fmt;
use std::marker::PhantomData;

use crate::ids::{AttemptId, RunId};
use crate::state::{AttemptOutcome, state_enum};

state_enum! {
    /// `attempt.state` — the eight states of §5.2, persisted from schema v2.
    ///
    /// Schema v1 had only `attempt.outcome`, which cannot express `CREATED`,
    /// `STARTING` or `ACTIVE`; a supervisor could therefore not record that an
    /// attempt was in flight, which is precisely what startup recovery must
    /// read. Migration 2 adds the column.
    AttemptState {
        Created => "CREATED",
        Starting => "STARTING",
        Active => "ACTIVE",
        Exited => "EXITED",
        Crashed => "CRASHED",
        TimedOut => "TIMED_OUT",
        Stale => "STALE",
        Reconciled => "RECONCILED",
    }
    terminal: [Reconciled]
}

impl AttemptState {
    /// Whether an attempt in this state may still have a live process.
    ///
    /// This is what startup recovery reads: an attempt found in one of these
    /// states was in flight when the supervisor died.
    pub fn is_in_flight(&self) -> bool {
        matches!(
            self,
            AttemptState::Created | AttemptState::Starting | AttemptState::Active
        )
    }

    /// Whether this state is one of the four terminal *outcomes* — the states
    /// from which the only move left is `RECONCILED`.
    pub fn is_terminal_outcome(&self) -> bool {
        matches!(
            self,
            AttemptState::Exited
                | AttemptState::Crashed
                | AttemptState::TimedOut
                | AttemptState::Stale
        )
    }
}

/// The phase markers. One zero-sized type per state.
pub mod phase {
    /// The attempt row exists; nothing has been spawned.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Created;
    /// Conductor is about to spawn, or is spawning.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Starting;
    /// A process exists and Conductor knows its pid.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Active;
    /// The process exited and the exit was observed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Exited;
    /// A nonzero exit or a fatal signal was observed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Crashed;
    /// Conductor killed the process for exceeding a budget.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TimedOut;
    /// The process is gone and no exit was observed. **We do not know.**
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Stale;
    /// Conductor has looked at the repository. Terminal.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Reconciled;
}

/// What a process looked like when it was spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawn {
    /// The child's process id.
    pub pid: i32,
    /// The child's start time in microseconds since the epoch, or `None` when
    /// the kernel would not say.
    ///
    /// Recorded because a pid alone is not an identity: pids are recycled, and
    /// §4.7 step 3 requires "alive **and** start-time matches" before a recorded
    /// pid may be believed to be the same process.
    ///
    /// `Option`, not a sentinel. The rejected alternative was `0` for "could not
    /// read it", which reads back as a start time like any other and — because
    /// `0` was also the value that asked `probe` to skip the check — turned "the
    /// child was never identified" into "adopt whatever holds this pid". Absence
    /// has to stay absent all the way to the column, so that every reader is
    /// forced to decide what to do about it.
    pub pid_start_time: Option<i64>,
}

/// How an attempt ended.
///
/// Constructed only by the transition methods below, which is what keeps a
/// `STALE` attempt from acquiring an exit code it never had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Termination {
    spawn: Option<Spawn>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    timeout_reason: Option<&'static str>,
    outcome: AttemptOutcome,
    detail: Option<String>,
}

/// One state of the machine.
pub trait Phase {
    /// The persisted `attempt.state` for this phase.
    const STATE: AttemptState;
    /// What an attempt in this phase carries.
    type Data;
}

/// A phase from which the only remaining move is `RECONCILED`.
pub trait Terminal: Phase {
    /// The persisted `attempt.outcome` for this phase.
    const OUTCOME: AttemptOutcome;
}

macro_rules! phase_impl {
    ($marker:ty, $state:expr, $data:ty) => {
        impl Phase for $marker {
            const STATE: AttemptState = $state;
            type Data = $data;
        }
    };
    ($marker:ty, $state:expr, $data:ty, terminal $outcome:expr) => {
        phase_impl!($marker, $state, $data);
        impl Terminal for $marker {
            const OUTCOME: AttemptOutcome = $outcome;
        }
    };
}

phase_impl!(phase::Created, AttemptState::Created, ());
phase_impl!(phase::Starting, AttemptState::Starting, ());
phase_impl!(phase::Active, AttemptState::Active, Spawn);
phase_impl!(
    phase::Exited,
    AttemptState::Exited,
    Termination,
    terminal AttemptOutcome::Exited
);
phase_impl!(
    phase::Crashed,
    AttemptState::Crashed,
    Termination,
    terminal AttemptOutcome::Crashed
);
phase_impl!(
    phase::TimedOut,
    AttemptState::TimedOut,
    Termination,
    terminal AttemptOutcome::TimedOut
);
phase_impl!(
    phase::Stale,
    AttemptState::Stale,
    Termination,
    terminal AttemptOutcome::Stale
);
phase_impl!(phase::Reconciled, AttemptState::Reconciled, Termination);

/// One attempt, in one phase of §5.2's machine.
///
/// Illegal transitions do not compile:
///
/// ```compile_fail
/// # use conductor_core::attempt::Attempt;
/// # use conductor_core::{AttemptId, RunId};
/// let a = Attempt::create(AttemptId::new("a-1").unwrap(), RunId::new("r-1").unwrap(), 1);
/// // CREATED has no process, so it cannot become ACTIVE.
/// let _ = a.active(1, Some(1));
/// ```
///
/// ```compile_fail
/// # use conductor_core::attempt::Attempt;
/// # use conductor_core::{AttemptId, RunId};
/// let a = Attempt::create(AttemptId::new("a-1").unwrap(), RunId::new("r-1").unwrap(), 1)
///     .starting()
///     .active(1, Some(1));
/// // §5.2: RECONCILING is unskippable. ACTIVE cannot reach RECONCILED.
/// let _ = a.reconciled();
/// ```
///
/// ```compile_fail
/// # use conductor_core::attempt::Attempt;
/// # use conductor_core::{AttemptId, RunId};
/// let a = Attempt::create(AttemptId::new("a-1").unwrap(), RunId::new("r-1").unwrap(), 1)
///     .starting()
///     .active(1, Some(1));
/// // …nor may a run leave RUNNING on the evidence of an attempt still running.
/// let _ = a.evidence();
/// ```
///
/// **Non-vacuity control.** The three failures above must fail on the
/// transition, not on a typo, a moved import or a private constructor. This
/// compiles, and it exercises every path, name and argument the three use:
///
/// ```
/// # use conductor_core::attempt::{Attempt, AttemptState};
/// # use conductor_core::{AttemptId, RunId, RunState};
/// let a = Attempt::create(AttemptId::new("a-1").unwrap(), RunId::new("r-1").unwrap(), 1);
/// let a = a.starting().active(1, Some(1));
/// assert_eq!(a.state(), AttemptState::Active);
/// let a = a.exited(0);                       // ACTIVE → terminal, legally
/// let evidence = a.evidence();               // evidence() exists on a terminal
/// assert_eq!(RunState::leave_running(&evidence), RunState::Reconciling);
/// assert_eq!(a.reconciled().state(), AttemptState::Reconciled);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt<P: Phase> {
    id: AttemptId,
    run_id: RunId,
    ordinal: i64,
    data: P::Data,
    marker: PhantomData<P>,
}

impl<P: Phase> Attempt<P> {
    /// The attempt's id.
    pub fn id(&self) -> &AttemptId {
        &self.id
    }

    /// The run this attempt belongs to.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// `attempt.ordinal` — 1-based, unique within the run.
    pub fn ordinal(&self) -> i64 {
        self.ordinal
    }

    /// The persisted `attempt.state` for the current phase.
    pub fn state(&self) -> AttemptState {
        P::STATE
    }

    fn move_to<Q: Phase>(self, data: Q::Data) -> Attempt<Q> {
        Attempt {
            id: self.id,
            run_id: self.run_id,
            ordinal: self.ordinal,
            data,
            marker: PhantomData,
        }
    }
}

impl Attempt<phase::Created> {
    /// Record a new attempt. Nothing has been spawned.
    pub fn create(id: AttemptId, run_id: RunId, ordinal: i64) -> Self {
        Attempt {
            id,
            run_id,
            ordinal,
            data: (),
            marker: PhantomData,
        }
    }

    /// `CREATED → STARTING`: Conductor is about to spawn.
    pub fn starting(self) -> Attempt<phase::Starting> {
        self.move_to::<phase::Starting>(())
    }
}

impl Attempt<phase::Starting> {
    /// `STARTING → ACTIVE`: a process exists and this is its identity.
    ///
    /// `pid_start_time` is `None` when the process exists but its start time
    /// could not be read. The attempt is still `ACTIVE` — a child is running and
    /// pretending otherwise would lose it — but recovery will refuse to adopt it
    /// later, because half of §4.7 step 3's question has no answer.
    pub fn active(self, pid: i32, pid_start_time: Option<i64>) -> Attempt<phase::Active> {
        self.move_to::<phase::Active>(Spawn {
            pid,
            pid_start_time,
        })
    }

    /// `STARTING → STALE`: the spawn itself failed, so there is no process to
    /// classify.
    ///
    /// Deliberately **not** `CRASHED`. Nothing of the agent ever ran, so an exit
    /// code would be an invention; §4.7 classifies a failed spawn as
    /// infrastructure, and §5.2 reserves `CRASHED` for an observed nonzero exit.
    pub fn spawn_failed(self, detail: impl Into<String>) -> TerminalPhase {
        TerminalPhase::Stale(self.move_to::<phase::Stale>(Termination {
            spawn: None,
            exit_code: None,
            signal: None,
            timeout_reason: None,
            outcome: AttemptOutcome::Stale,
            detail: Some(detail.into()),
        }))
    }
}

impl Attempt<phase::Active> {
    /// The child's pid.
    pub fn pid(&self) -> i32 {
        self.data.pid
    }

    /// The child's start time, microseconds since the epoch, or `None` when it
    /// could not be read.
    pub fn pid_start_time(&self) -> Option<i64> {
        self.data.pid_start_time
    }

    /// What was spawned.
    pub fn spawn(&self) -> Spawn {
        self.data
    }

    fn terminate<Q>(
        self,
        exit_code: Option<i32>,
        signal: Option<i32>,
        timeout_reason: Option<&'static str>,
        detail: Option<String>,
    ) -> Attempt<Q>
    where
        Q: Terminal + Phase<Data = Termination>,
    {
        let spawn = Some(self.data);
        self.move_to::<Q>(Termination {
            spawn,
            exit_code,
            signal,
            timeout_reason,
            outcome: Q::OUTCOME,
            detail,
        })
    }

    /// The process exited and the code was observed.
    ///
    /// `EXITED` for zero, `CRASHED` otherwise (§6.4).
    pub fn exited(self, code: i32) -> TerminalPhase {
        if code == 0 {
            TerminalPhase::Exited(self.terminate::<phase::Exited>(Some(0), None, None, None))
        } else {
            TerminalPhase::Crashed(self.terminate::<phase::Crashed>(Some(code), None, None, None))
        }
    }

    /// The process died on a signal (§6.4: `SIGKILL`/`SIGSEGV` → `CRASHED`).
    pub fn signalled(self, signal: i32) -> TerminalPhase {
        TerminalPhase::Crashed(self.terminate::<phase::Crashed>(None, Some(signal), None, None))
    }

    /// The wall-clock budget was exceeded and Conductor killed the process.
    pub fn timed_out_wall(self) -> TerminalPhase {
        TerminalPhase::TimedOut(self.terminate::<phase::TimedOut>(
            None,
            None,
            Some(TIMEOUT_WALL_CLOCK),
            None,
        ))
    }

    /// No output for `idle_timeout` (§6.4: `TIMED_OUT`, `reason=stall`).
    pub fn timed_out_idle(self) -> TerminalPhase {
        TerminalPhase::TimedOut(self.terminate::<phase::TimedOut>(
            None,
            None,
            Some(TIMEOUT_STALL),
            None,
        ))
    }

    /// The process produced nothing at all within the startup budget.
    ///
    /// Distinct from a stall because M29 makes "slow to first byte" a different
    /// diagnosis from "went quiet mid-run" on this host.
    pub fn timed_out_startup(self) -> TerminalPhase {
        TerminalPhase::TimedOut(self.terminate::<phase::TimedOut>(
            None,
            None,
            Some(TIMEOUT_NO_STARTUP),
            None,
        ))
    }

    /// The process is gone and no exit was observed.
    ///
    /// No exit code and no signal are recorded, because none were seen. This is
    /// the whole content of `STALE ≠ CRASHED`.
    pub fn stale(self) -> TerminalPhase {
        TerminalPhase::Stale(self.terminate::<phase::Stale>(None, None, None, None))
    }
}

/// `reason=wall_clock` — the run's wall-clock budget was exceeded.
pub const TIMEOUT_WALL_CLOCK: &str = "wall_clock";
/// `reason=stall` — no output for `idle_timeout` (§6.4).
pub const TIMEOUT_STALL: &str = "stall";
/// `reason=no_startup` — the process produced no output at all within the
/// startup budget. Kept distinct from `stall` because M29 makes "slow to
/// produce a first byte" a *different* diagnosis from "went quiet mid-run".
pub const TIMEOUT_NO_STARTUP: &str = "no_startup";

/// An attempt that reached one of the four terminal outcomes.
///
/// The supervisor learns how a process ended at runtime, so it needs one type
/// to hold "whichever of the four it was". This does **not** weaken the
/// typestate: the only way to obtain a `TerminalPhase` is a transition from
/// `Attempt<Active>` or `Attempt<Starting>`, and `Attempt<Active>` still has no
/// `reconciled()` and no `evidence()` of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalPhase {
    /// Exit code 0 was observed.
    Exited(Attempt<phase::Exited>),
    /// A nonzero exit or a fatal signal was observed.
    Crashed(Attempt<phase::Crashed>),
    /// Conductor killed the process for exceeding a budget.
    TimedOut(Attempt<phase::TimedOut>),
    /// The process is gone and nobody saw it go.
    Stale(Attempt<phase::Stale>),
}

macro_rules! terminal_dispatch {
    ($self:expr, $a:ident => $body:expr) => {
        match $self {
            TerminalPhase::Exited($a) => $body,
            TerminalPhase::Crashed($a) => $body,
            TerminalPhase::TimedOut($a) => $body,
            TerminalPhase::Stale($a) => $body,
        }
    };
}

impl TerminalPhase {
    /// The persisted `attempt.state`.
    pub fn state(&self) -> AttemptState {
        match self {
            TerminalPhase::Exited(_) => AttemptState::Exited,
            TerminalPhase::Crashed(_) => AttemptState::Crashed,
            TerminalPhase::TimedOut(_) => AttemptState::TimedOut,
            TerminalPhase::Stale(_) => AttemptState::Stale,
        }
    }

    /// The persisted `attempt.outcome`.
    pub fn outcome(&self) -> AttemptOutcome {
        terminal_dispatch!(self, a => a.outcome())
    }

    /// The attempt's id.
    pub fn id(&self) -> &AttemptId {
        terminal_dispatch!(self, a => a.id())
    }

    /// The run this attempt belongs to.
    pub fn run_id(&self) -> &RunId {
        terminal_dispatch!(self, a => a.run_id())
    }

    /// `attempt.ordinal`.
    pub fn ordinal(&self) -> i64 {
        terminal_dispatch!(self, a => a.ordinal())
    }

    /// The observed exit code, when one was observed.
    pub fn exit_code(&self) -> Option<i32> {
        terminal_dispatch!(self, a => a.exit_code())
    }

    /// The fatal signal, when there was one.
    pub fn signal(&self) -> Option<i32> {
        terminal_dispatch!(self, a => a.signal())
    }

    /// `reason=` for a timeout.
    pub fn timeout_reason(&self) -> Option<&'static str> {
        terminal_dispatch!(self, a => a.timeout_reason())
    }

    /// What was spawned, when anything was.
    pub fn spawn(&self) -> Option<Spawn> {
        terminal_dispatch!(self, a => a.spawn())
    }

    /// Free-text evidence, e.g. why a spawn failed.
    pub fn detail(&self) -> Option<&str> {
        terminal_dispatch!(self, a => a.detail())
    }

    /// Evidence that this attempt reached a terminal outcome — the only thing
    /// that lets a run leave `RUNNING`.
    pub fn evidence(&self) -> TerminalAttempt {
        terminal_dispatch!(self, a => a.evidence())
    }

    /// The last transition: Conductor has looked at the repository.
    pub fn reconciled(self) -> Attempt<phase::Reconciled> {
        terminal_dispatch!(self, a => a.reconciled())
    }
}

impl<P> Attempt<P>
where
    P: Phase<Data = Termination>,
{
    /// The recorded exit code, when one was observed.
    pub fn exit_code(&self) -> Option<i32> {
        self.data.exit_code
    }

    /// The recorded signal, when the process died on one.
    pub fn signal(&self) -> Option<i32> {
        self.data.signal
    }

    /// `reason=` for a timeout: [`TIMEOUT_WALL_CLOCK`] or [`TIMEOUT_STALL`].
    pub fn timeout_reason(&self) -> Option<&'static str> {
        self.data.timeout_reason
    }

    /// The persisted `attempt.outcome`.
    ///
    /// For a `RECONCILED` attempt this stays the terminal classification the
    /// attempt actually had — `RECONCILED` lives in `attempt.state` from schema
    /// v2, and overwriting the outcome with it would destroy the one fact
    /// reconciliation exists to act on.
    pub fn outcome(&self) -> AttemptOutcome {
        self.data.outcome
    }

    /// What was spawned, when anything was.
    pub fn spawn(&self) -> Option<Spawn> {
        self.data.spawn
    }

    /// Free-text evidence, e.g. why a spawn failed.
    pub fn detail(&self) -> Option<&str> {
        self.data.detail.as_deref()
    }
}

impl<P> Attempt<P>
where
    P: Terminal + Phase<Data = Termination>,
{
    /// Evidence that this attempt reached a terminal outcome.
    ///
    /// The only way to obtain a [`TerminalAttempt`], and therefore the only way
    /// a run can leave `RUNNING` (§5.2: "`RUNNING → RECONCILING` requires a
    /// terminal attempt").
    pub fn evidence(&self) -> TerminalAttempt {
        TerminalAttempt {
            attempt_id: self.id.clone(),
            outcome: P::OUTCOME,
        }
    }

    /// The last transition: Conductor has looked at the repository.
    ///
    /// Available only from a terminal phase, which is what makes
    /// `ACTIVE → RECONCILED` unwritable.
    pub fn reconciled(self) -> Attempt<phase::Reconciled> {
        let data = self.data.clone();
        self.move_to::<phase::Reconciled>(data)
    }
}

/// Proof that some attempt reached a terminal outcome.
///
/// Fields are private and there is no public constructor: the only source is
/// [`Attempt::evidence`] on a terminal phase. A caller cannot fabricate one to
/// advance a run whose agent is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalAttempt {
    attempt_id: AttemptId,
    outcome: AttemptOutcome,
}

impl TerminalAttempt {
    /// Which attempt this is evidence about.
    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    /// How that attempt ended.
    pub fn outcome(&self) -> AttemptOutcome {
        self.outcome
    }
}

impl fmt::Display for TerminalAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ended {}", self.attempt_id, self.outcome)
    }
}
