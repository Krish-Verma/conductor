//! Parsed agent events.
//!
//! Every real adapter is "a process launcher writing JSONL to stdout" (§6.1),
//! so the parsed form is a small closed enum plus an escape hatch. The escape
//! hatch matters more than the enum: an event kind Conductor does not model is
//! the normal state of affairs against a CLI that changes between patch
//! releases, and it must never be an error.

use serde::{Deserialize, Serialize};

use conductor_core::AgentReport;

/// One thing an agent said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEvent {
    /// The agent started and identified its session.
    Started {
        /// The session id, when the agent assigns one itself.
        session_id: Option<String>,
        /// What the agent says it is doing.
        detail: String,
    },
    /// The agent wrote a file.
    FileWritten {
        /// Repository-relative path, as the agent reported it.
        path: String,
    },
    /// The agent deleted a file.
    FileDeleted {
        /// Repository-relative path, as the agent reported it.
        path: String,
    },
    /// The agent ran a command.
    CommandRun {
        /// The command, as the agent reported it.
        command: String,
    },
    /// A named point in a scenario.
    ///
    /// The fake agent's synchronisation primitive: a test reads until it sees
    /// the checkpoint it wants and kills exactly there. This is what lets the
    /// crash matrix be deterministic instead of a race against a sleep.
    Checkpoint {
        /// The checkpoint's name.
        name: String,
    },
    /// The agent delivered its report on stdout.
    Report {
        /// The report.
        report: AgentReport,
    },
    /// The agent reported an error condition.
    Error {
        /// What the agent said went wrong.
        message: String,
        /// Whether the agent classified it as infrastructure (§6.4: auth and
        /// rate-limit errors are infrastructure and consume no budget).
        infrastructure: bool,
    },
    /// The agent attempted to reach Conductor's control socket.
    ///
    /// Acceptance row 28. Recorded as an event rather than inferred from a
    /// syscall trace, because under an unsandboxed launcher the attempt is all
    /// Conductor can observe.
    ControlSocketAttempt {
        /// The socket path it tried.
        path: String,
        /// Whether the connection succeeded.
        connected: bool,
    },
}

impl AgentEvent {
    /// Whether this event, on its own, means the agent touched the tree.
    ///
    /// Evidence only. §4.8 decides what actually changed by looking at git; this
    /// is for cross-checking the agent's account against it.
    pub fn claims_a_write(&self) -> bool {
        matches!(
            self,
            AgentEvent::FileWritten { .. } | AgentEvent::FileDeleted { .. }
        )
    }
}
