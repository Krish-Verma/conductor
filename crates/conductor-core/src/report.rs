//! What an agent says it did — master plan §4.8 and §6.1.
//!
//! **Evidence only, never authority.** §4.8's whole purpose is that the report
//! is one input to a classification whose other input is the repository, and
//! when they disagree, git wins. The type lives in the pure core because the
//! adapter layer produces it and reconciliation consumes it; putting it in
//! either would make the other depend on a crate it has no other business with.
//!
//! No `deny_unknown_fields`, ever (§2.2). This is a structure an agent
//! produced.

use serde::{Deserialize, Serialize};

/// What an agent said it did. Evidence only — never authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReport {
    /// The agent's own claim about the attempt.
    pub claim: ReportClaim,
    /// Paths the agent says it modified.
    #[serde(default)]
    pub files_touched: Vec<String>,
    /// The agent's prose summary, when it gave one.
    #[serde(default)]
    pub summary: String,
}

/// An agent's claim about its attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReportClaim {
    /// The agent says the task is done.
    Complete,
    /// The agent says it made partial progress.
    Partial,
    /// The agent says it failed.
    Failed,
}
