//! Adapter errors.

/// Anything an adapter can fail with.
///
/// The set is small because an adapter does very little: it cannot fail to
/// spawn (it does not spawn), cannot time out (it holds no clock) and cannot
/// fail to write (it writes nothing).
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// A stdout line was not JSON at all.
    ///
    /// Distinct from "a kind I do not model", which is `Ok(None)`: this one is
    /// evidence of a broken stream and becomes a finding.
    #[error("agent output line could not be parsed as JSON: {detail}")]
    MalformedLine {
        /// What the JSON parser said.
        detail: String,
        /// The offending line, truncated.
        line: String,
    },

    /// A report was produced and could not be read (acceptance row 5).
    #[error("the agent's report could not be parsed: {0}")]
    ReportUnparseable(String),

    /// The adapter cannot build a command from this input.
    #[error("cannot build an agent command: {0}")]
    Unusable(String),
}

/// Result alias for adapter operations.
pub type AgentResult<T> = Result<T, AgentError>;
