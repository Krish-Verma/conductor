//! Repair's configuration and the two retry kinds — master plan §4.6, §4.7.

use conductor_core::VerificationOutcome;

use crate::verify::runner::VerificationReport;

/// §4.6's `repair:` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairConfig {
    /// How many **repair** attempts a run may spend.
    pub max_attempts: usize,
    /// Stop the moment a repair reproduces the previous fingerprint.
    pub stop_on_identical_fingerprint: bool,
    /// After this many repairs, escalate to `AWAITING_REVIEW`.
    pub escalate_after: usize,
    /// The attempt ordinal from which the agent gets a fresh session.
    pub new_session_on_attempt: i64,
    /// How many **infrastructure** retries a run may spend (§4.7).
    ///
    /// Not in §4.6's printed `repair:` block, because §4.6 is about work
    /// retries. It is here because §4.7 gives infrastructure retries a bound
    /// ("backoff, does **not** consume budget") without a number, and
    /// acceptance row 8 supplies the number: "infra retry **×1**, no budget
    /// spent … human after 2". A bound with no configured value is a bound
    /// somebody hardcodes at the call site.
    ///
    /// It is also the third term of S6's invocation ceiling
    /// ([`crate::repair::driver::ceiling`]): infrastructure attempts cost no
    /// work budget, so without a term of their own they would be unbounded in
    /// the only count that matters — spawns.
    pub max_infra_retries: usize,
}

impl Default for RepairConfig {
    fn default() -> Self {
        RepairConfig {
            max_attempts: 2,
            stop_on_identical_fingerprint: true,
            escalate_after: 2,
            new_session_on_attempt: 2,
            max_infra_retries: 1,
        }
    }
}

/// §4.7's two retry kinds, "never conflated".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    /// The agent failed the criteria.
    Work,
    /// Something around the work broke.
    Infrastructure,
    /// Nothing to retry.
    None,
}

/// Which kind of retry, if any, a verification report calls for.
///
/// §4.5: "`FAIL` → repair; `INCONCLUSIVE` → bounded infra retry, then human."
/// §4.7: conflating the two "is how a broken API key silently exhausts a task's
/// budget".
///
/// `FAIL` outranks the rest, because a report holding both has real work to do
/// and retrying the infrastructure would leave the failing check failing.
/// `VOID` joins `INCONCLUSIVE` rather than `FAIL`: it says the tree moved under
/// the check, which is a statement about the run and never about the code, so
/// acceptance row 26's "re-run at the new tree" is a verification action and
/// must not cost the agent an attempt.
pub fn retry_kind(report: &VerificationReport) -> RetryKind {
    let mut infrastructure = false;
    for result in &report.results {
        match result.outcome {
            VerificationOutcome::Fail => return RetryKind::Work,
            VerificationOutcome::Inconclusive | VerificationOutcome::Void => infrastructure = true,
            VerificationOutcome::Pass => {}
        }
    }
    if infrastructure {
        RetryKind::Infrastructure
    } else {
        RetryKind::None
    }
}

/// Whether an attempt resumes the previous session or starts a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPolicy {
    /// Continue where the last attempt left off.
    Resume,
    /// Start clean.
    Fresh,
}

/// Whether the attempt with this ordinal resumes or starts clean.
///
/// §4.6: `new_session_on_attempt: 2` — "a stuck agent's context *is* the
/// problem, and resuming re-imports the stuckness". The comparison is `>=`
/// rather than `==` so that a third attempt, if a configuration ever allows
/// one, does not silently go back to resuming.
pub fn session_for_attempt(ordinal: i64, config: &RepairConfig) -> SessionPolicy {
    if ordinal >= config.new_session_on_attempt {
        SessionPolicy::Fresh
    } else {
        SessionPolicy::Resume
    }
}

/// The session id to hand the adapter, which is `None` more often than it looks.
///
/// Three ways to get nothing, and they are different facts: the policy is
/// `Fresh` and the previous session is being *deliberately* discarded; the
/// adapter declares `session_resume: false` (§6.1), so asking it to resume would
/// be Conductor requesting something it has been told is not there; or there is
/// no previous session because this is the first attempt.
pub fn session_id_for(
    policy: SessionPolicy,
    adapter_can_resume: bool,
    previous: Option<&str>,
) -> Option<String> {
    match policy {
        SessionPolicy::Fresh => None,
        SessionPolicy::Resume if !adapter_can_resume => None,
        SessionPolicy::Resume => previous.map(str::to_string),
    }
}
