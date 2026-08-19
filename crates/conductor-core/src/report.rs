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

    // ----------------------------------------------------------------------
    // §6.5's review inputs — deferred until S13, which is their consumer
    // ----------------------------------------------------------------------
    //
    // §6.5's report table marked these five *"not in v1"* with a reason:
    // "Every one of them is a *review* input, and their only consumer is §6.5's
    // review packet, which **S13 owns**. Asking an agent to produce a field
    // nothing reads is exactly the no-op knob CLAUDE.md forbids."
    //
    // S13 builds that packet and reads all five, so the deferral is discharged
    // rather than forgotten. Every one is `#[serde(default)]`, so this is a
    // compatible extension and `$id` stays `agent-report.v1` — see
    // `schemas/agent-report.v1.json`.
    /// Which task the agent believes it worked on.
    ///
    /// A cross-check a human reads beside the task Conductor actually gave it.
    /// `Option` rather than `String`: "the agent did not say" and "the agent said
    /// the empty string" are different facts, and only the first is ordinary.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Commands the agent says it ran.
    ///
    /// Not verification — §4.5's checks are the ones Conductor runs itself, bound
    /// to a tree hash. These are what the *agent* claims, which is why they reach
    /// a human through a review packet rather than a completion criterion.
    #[serde(default)]
    pub commands_run: Vec<String>,
    /// Acceptance criterion ids the agent claims it satisfied.
    ///
    /// Evidence for the side-by-side, never a substitute for criterion 5's
    /// binding: a criterion is satisfied by a passing check, not by being named
    /// here.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Where the agent knowingly departed from the plan.
    ///
    /// The single most valuable field on this structure for a reviewer, and the
    /// reason §6.5 lists `deviations` in the review packet at all: a deviation the
    /// agent volunteers is one a human does not have to find in a diff.
    #[serde(default)]
    pub deviations: Vec<String>,
    /// What stopped the agent, in its own words.
    ///
    /// Distinct from a `FAILED` claim: an agent can finish `PARTIAL` and still
    /// have hit something a human can clear in a minute, and a blocker named here
    /// is the difference between a review that unblocks it and one that guesses.
    #[serde(default)]
    pub blockers: Vec<String>,
    /// Claims the agent makes but did not verify.
    ///
    /// §4.5's escape hatch (`manual: true`) forces a review boundary for exactly
    /// this class of statement. Carrying them explicitly means a reviewer sees
    /// "the agent asserts this and nothing checked it" rather than reading an
    /// unqualified summary.
    #[serde(default)]
    pub unverified_claims: Vec<String>,
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
