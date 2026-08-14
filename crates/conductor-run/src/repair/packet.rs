//! The repair packet — master plan §6.5.
//!
//! > **Repair packet** adds only: failing check IDs · the failure fingerprint ·
//! > a bounded log excerpt (first failing assertion + 40 lines, never the full
//! > log) · the diff of what the previous attempt changed · attempt ordinal and
//! > remaining budget · an explicit `do_not_retry` list of approaches already
//! > tried. That last field is what stops attempt 2 from being attempt 1 again.
//!
//! "Adds only" is the whole specification: everything else the agent needs is
//! the implementation packet, which S12 owns. This module builds the six fields
//! §6.5 names and nothing beyond them.
//!
//! # Every field comes from durable state
//!
//! Not from the `Vertical` the previous attempt returned. A packet assembled
//! from a value held in memory would be a packet a restart cannot rebuild, and
//! §4.7's restart is the case that decides whether repair is bounded at all. The
//! observations are rows; the diff is read out of the workspace; the budget is
//! arithmetic over the two.
//!
//! # Bounded, because §6.5 says bounded
//!
//! §6.5: "Verification logs never embedded; failing *excerpts* only." Both the
//! excerpt and the diff are capped here, and each says when it was truncated —
//! a silently shortened diff is worse than a short one, because the agent
//! cannot tell that it is looking at part of the picture.
//!
//! Serialization is field-ordered and free of timestamps, per §6.6's
//! determinism requirement: "Sorted keys, LF, no timestamps inside hashed
//! content."

use std::path::Path;

use serde::Serialize;

use super::breaker::RepairHistory;
use super::config::{RepairConfig, SessionPolicy};
use super::observation::{Observation, ObservationKind};

/// §6.5's "first failing assertion + 40 lines".
pub const EXCERPT_LINES: usize = 40;

/// How many lines of unified diff the packet carries.
///
/// §6.5 gives no number for the diff, only for the log. This one is chosen for
/// the same reason the log has one: the packet is context an agent pays for, and
/// an unbounded field turns one large refactor into a packet nothing can read.
pub const DIFF_LINES: usize = 400;

/// A failure the previous attempts already produced.
///
/// §6.5: "an explicit `do_not_retry` list of approaches already tried. That last
/// field is what stops attempt 2 from being attempt 1 again."
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlreadyTried {
    /// Which attempt produced it.
    pub attempt_ordinal: i64,
    /// What that attempt was — a crash and an empty edit are different lessons.
    pub outcome: String,
    /// §4.6's digest, recomputed from the stored inputs.
    pub fingerprint: String,
    /// Which checks failed.
    pub failing_checks: Vec<String>,
    /// The assertion, so the agent can see the failure and not only its hash.
    pub assertion: String,
}

/// What is left before a person is called.
///
/// Both numbers are counted **as the packet is built**, so the attempt about to
/// start is included in them: `repairs: 2` means "this repair and one more".
/// The other convention — counting what remains *after* this attempt — would
/// print `0` to the agent that still has one try left, which is the number most
/// likely to change how it behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RemainingBudget {
    /// Repairs left under §4.6's `max_attempts`, this one included.
    pub repairs: usize,
    /// Agent invocations left under S6's hard ceiling, this one included.
    ///
    /// Reported alongside `repairs` rather than instead of it because they
    /// answer different questions: `repairs` is how many more times the agent
    /// may be *wrong*, and this is how many more times it may be *started*.
    pub invocations: usize,
}

/// The bounded log excerpt §6.5 permits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Excerpt {
    /// The first failing assertion, exactly as §4.6 fingerprints it.
    pub first_failing_assertion: String,
    /// At most [`EXCERPT_LINES`] lines of context.
    pub lines: Vec<String>,
    /// Whether anything was dropped to fit.
    pub truncated: bool,
}

/// What the previous attempt did to the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviousDiff {
    /// The paths reconciliation observed as changed — read from git, never
    /// from the agent's report (§4.8).
    pub changed_paths: Vec<String>,
    /// At most [`DIFF_LINES`] lines of unified diff.
    pub unified: Vec<String>,
    /// Whether anything was dropped to fit.
    pub truncated: bool,
}

/// §6.5's repair packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairPacket {
    /// The run.
    pub run_id: String,
    /// The attempt this packet is *for* — the one about to start.
    pub attempt_ordinal: i64,
    /// The check ids that failed last time.
    pub failing_checks: Vec<String>,
    /// §4.6's fingerprint of the last failure, when the last attempt had one.
    pub fingerprint: Option<String>,
    /// The bounded excerpt.
    pub excerpt: Option<Excerpt>,
    /// What the previous attempt changed.
    pub previous_diff: PreviousDiff,
    /// What is left.
    pub remaining_budget: RemainingBudget,
    /// Approaches already tried, oldest first.
    pub do_not_retry: Vec<AlreadyTried>,
    /// Whether this attempt resumes the previous session or starts clean.
    ///
    /// §4.6: `new_session_on_attempt: 2` — "a stuck agent's context *is* the
    /// problem, and resuming re-imports the stuckness. The repair packet's
    /// `do_not_retry` list carries forward what matters." The two clauses are
    /// one design: the packet is what makes discarding the context safe, so the
    /// packet is where the decision is recorded.
    pub fresh_session: bool,
}

/// Build the packet for the attempt with ordinal `next_ordinal`.
///
/// `observations` is the durable history, oldest first; `workspace` is the run
/// clone the previous attempt edited; `invocations_left` is what S6's ceiling
/// still allows.
pub fn build(
    run_id: &str,
    next_ordinal: i64,
    observations: &[Observation],
    history: &RepairHistory,
    workspace: &Path,
    config: &RepairConfig,
    invocations_left: usize,
) -> RepairPacket {
    let last_failure = observations
        .iter()
        .rev()
        .find(|o| o.kind == ObservationKind::Failed);

    let excerpt = last_failure.map(|observation| {
        let lines: Vec<String> = observation
            .assertion
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let truncated = lines.len() > EXCERPT_LINES;
        Excerpt {
            first_failing_assertion: observation.assertion.clone(),
            lines: lines.into_iter().take(EXCERPT_LINES).collect(),
            truncated,
        }
    });

    let do_not_retry = observations
        .iter()
        .map(|observation| AlreadyTried {
            attempt_ordinal: observation.ordinal,
            outcome: observation.kind.as_str().to_string(),
            fingerprint: observation.fingerprint().as_str().to_string(),
            failing_checks: observation.failing_checks.clone(),
            assertion: observation.assertion.clone(),
        })
        .collect();

    RepairPacket {
        run_id: run_id.to_string(),
        attempt_ordinal: next_ordinal,
        failing_checks: last_failure
            .map(|o| o.failing_checks.clone())
            .unwrap_or_default(),
        fingerprint: last_failure.map(|o| o.fingerprint().as_str().to_string()),
        excerpt,
        previous_diff: previous_diff(workspace),
        remaining_budget: RemainingBudget {
            repairs: config.max_attempts.saturating_sub(history.repairs_used()),
            invocations: invocations_left,
        },
        do_not_retry,
        fresh_session: matches!(
            super::config::session_for_attempt(next_ordinal, config),
            SessionPolicy::Fresh
        ),
    }
}

/// The previous attempt's diff, read out of the workspace and capped.
///
/// Two readings, because neither alone is the change set. `git status
/// --porcelain -uall` names every path the attempt touched, **including files it
/// created** — which are untracked, and therefore invisible to `git diff`. The
/// diff then supplies the content for the paths git already tracks. Listing the
/// created files without their contents is the honest shape: §6.5 caps the
/// packet, and a new file's whole body is the least bounded thing in it.
///
/// A git failure yields an empty field and `truncated: true` rather than an
/// error: a packet that cannot show the diff is still a packet worth sending,
/// and the flag says the agent is looking at part of the picture.
fn previous_diff(workspace: &Path) -> PreviousDiff {
    let paths = match conductor_git::run_git(
        workspace,
        &["status", "--porcelain", "--untracked-files=all"],
    ) {
        Ok(output) if output.ok() => {
            let text = output.stdout_lossy().into_owned();
            let mut paths: Vec<String> = text
                .lines()
                .filter_map(|line| line.get(3..).map(str::to_string))
                .collect();
            paths.sort();
            paths.dedup();
            paths
        }
        _ => Vec::new(),
    };

    let Ok(output) = conductor_git::run_git(workspace, &["diff", "--unified=3"]) else {
        return PreviousDiff {
            changed_paths: paths,
            unified: Vec::new(),
            truncated: true,
        };
    };
    if !output.ok() {
        return PreviousDiff {
            changed_paths: paths,
            unified: Vec::new(),
            truncated: true,
        };
    }

    let text = output.stdout_lossy().into_owned();
    let all: Vec<&str> = text.lines().collect();
    let truncated = all.len() > DIFF_LINES;
    PreviousDiff {
        changed_paths: paths,
        unified: all
            .into_iter()
            .take(DIFF_LINES)
            .map(str::to_string)
            .collect(),
        truncated,
    }
}
