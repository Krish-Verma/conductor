//! Agent adapters — master plan §6.1.
//!
//! > Conductor owns spawning, killing, timeouts and streaming. Adapters are
//! > **pure translation** — build argv+env, parse lines, classify exits. This
//! > makes every adapter testable against recorded JSONL fixtures with no
//! > process at all, which is what you want because agent output is the least
//! > stable thing in the system.
//!
//! Nothing in this crate spawns a process, opens a socket, or reads a clock.
//! [`AgentAdapter::command`] *builds* a command; `conductor-run` runs it. The
//! separation is what makes `tests/adapter.rs` able to test every adapter
//! behaviour against a program path that does not exist.
//!
//! **No `deny_unknown_fields` on anything an agent produces** (§2.2). Agent CLIs
//! change fields between patch releases; a strict parser turns a cosmetic
//! upstream change into a failed run.

pub mod codex;
pub mod error;
pub mod event;
pub mod fake;
pub mod scenario;

use std::collections::BTreeMap;
use std::path::PathBuf;

use conductor_core::{AgentReport, AttemptOutcome, RunId, TaskId};

pub use error::{AgentError, AgentResult};
pub use event::AgentEvent;

/// What an adapter is told before an attempt starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartInput {
    /// The run.
    pub run_id: RunId,
    /// The task the run is executing.
    pub task_id: TaskId,
    /// `attempt.ordinal`.
    pub attempt_ordinal: i64,
    /// The run workspace. Becomes the child's working directory.
    pub workspace: PathBuf,
    /// Where the agent should write its structured report.
    pub report_path: PathBuf,
    /// A session id Conductor assigned, for adapters that accept one (§6.1's
    /// `conductor_assigned_session_id` capability).
    pub session_id: Option<String>,
    /// The **complete** environment the child will run with.
    ///
    /// An allowlist, not additions to the parent's environment (§4.9: "Not a
    /// denylist — a denylist misses the next variable name"). An adapter may add
    /// its own variables and nothing else.
    pub env: BTreeMap<String, String>,
}

/// What an adapter is told when resuming a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeInput {
    /// The same information a fresh start would carry.
    pub start: StartInput,
    /// The session to resume.
    pub session_id: String,
}

/// A command to run. Building one performs no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCommand {
    /// The program.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// The complete environment (§4.9).
    pub env: BTreeMap<String, String>,
    /// The working directory — always the run workspace.
    pub cwd: PathBuf,
}

/// Everything an attempt produced, as the supervisor collected it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunOutputs {
    /// Every line the agent wrote to stdout, in order.
    pub stdout_lines: Vec<String>,
    /// The contents of the report file, when one was written.
    pub report_json: Option<String>,
}

/// §6.1's functional capabilities.
///
/// Security capabilities live in
/// [`ExecutionCapabilities`](conductor_core::ExecutionCapabilities) (§4.2).
/// Conflating them hides the distinction that matters: what an agent *can do*
/// is a feature question, what it is *prevented from doing* is a measured
/// property of a launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FunctionalCapabilities {
    /// Conductor can assign the session id before the process starts.
    pub conductor_assigned_session_id: bool,
    /// The agent can resume a previous session.
    pub session_resume: bool,
    /// The agent's runtime enforces the report schema itself.
    pub schema_enforced_final_output: bool,
    /// The agent emits a JSONL event stream.
    pub streaming_events: bool,
    /// The agent can be made to ignore ambient user configuration.
    pub hermetic_config: bool,
    /// The agent enforces a spend cap.
    pub spend_cap: bool,
}

/// The adapter interface — §6.1, exactly.
pub trait AgentAdapter {
    /// A stable identifier, stored in `attempt.adapter`.
    fn id(&self) -> &str;

    /// What this agent can do functionally.
    fn capabilities(&self) -> FunctionalCapabilities;

    /// Build the command. **Does not spawn.**
    fn command(&self, input: &StartInput) -> AgentResult<AgentCommand>;

    /// Translate one line of the agent's stdout into **zero or more** events.
    ///
    /// An empty vector means "nothing Conductor models" — a blank line, or an
    /// event kind this binary has never heard of. `Err` means the line was not
    /// JSON at all, which the supervisor records as a finding without stopping
    /// the stream.
    ///
    /// # Why a `Vec` and not an `Option` (widened at S10)
    ///
    /// The signature was `Option<AgentEvent>` — one line, at most one event —
    /// until the first real adapter met a line that carries several. Codex's
    /// `file_change` item holds an **array** of changes, so an `Option` could
    /// report the first path and silently drop the rest.
    ///
    /// S10's own instruction is what settles it: *"If any scenario needs
    /// adapter-specific handling, that is a design smell to fix in the
    /// interface, not the adapter."* Making the Codex adapter pick a
    /// representative change, or flatten several into one, would have been the
    /// adapter absorbing a shape the interface refused to express — and the
    /// event stream would have understated what the agent did, on every
    /// multi-file edit, forever.
    ///
    /// Nothing about correctness depended on it: §4.8 reconciles against git,
    /// not against this stream. That is precisely why it would never have been
    /// caught by a failing test — which is the argument for fixing it now
    /// rather than when a second adapter has been built on the assumption.
    fn parse_event(&self, line: &str) -> AgentResult<Vec<AgentEvent>>;

    /// Find the structured report, if the agent produced one.
    ///
    /// `Ok(None)` is a normal outcome (row 4: "exit 0, no report"). `Err` means
    /// a report was produced and could not be read, which is row 5.
    fn extract_report(&self, out: &RunOutputs) -> AgentResult<Option<AgentReport>>;

    /// Classify an observed exit — §6.4.
    fn classify_exit(&self, code: Option<i32>, sig: Option<i32>) -> AttemptOutcome;

    /// Build a resume command, for adapters that can resume.
    fn resume_command(&self, input: &ResumeInput) -> Option<AgentCommand>;
}
