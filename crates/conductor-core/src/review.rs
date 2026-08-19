//! §5.2's review machine and §6.5's five decisions.
//!
//! [`crate::state`] decides which strings are legal in `review.state`; this
//! module decides which *moves* are legal, exactly as [`crate::task`] does for
//! [`TaskState`]. Keeping the two apart is what let S12 find that a plan-state
//! gate had no test: a legality table nothing consults is a table, not a gate.
//!
//! # Why the decision is a closed enum and not a string
//!
//! §6.5 lists the imported decision as `accept | repair | revise_plan | pause |
//! stop`. A human types that word by hand into a file, which makes it the single
//! most typo-exposed value in the system — and one of the five advances a task to
//! `COMPLETE`. §4.4's rule for an action nobody recognises is that it *fails
//! closed*, and the same rule has to hold here: an unrecognised decision must
//! never resolve to the most permissive known one. So parsing is total and
//! fallible, there is no `Default`, and [`ReviewDecision`] deliberately has no
//! `unwrap_or` in any caller.
//!
//! # Why `pause` has no target state
//!
//! §5.2 gives `AWAITING_REVIEW` four successors — `Complete`, `Repairing`,
//! `Superseded`, `Cancelled` — which is four labels for five decisions. The
//! missing one is `pause`, and it is missing because pausing is *not* a
//! transition: a human has looked and is not deciding yet. A self-transition is
//! refused everywhere in this codebase, so there is no state to write, and
//! [`ReviewDecision::task_target`] answers `None` rather than inventing one. What
//! changes is the review row, which is the thing that was previously unrecorded:
//! before S13 there was no way to distinguish "nobody has looked at this" from
//! "a human looked and wants it left alone".

use crate::state::{ReviewState, TaskState};

/// A review transition §5.2 does not draw.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a review cannot go from {from} to {to}; §5.2 draws PENDING → EXPORTED → DECIDED")]
pub struct ReviewTransitionError {
    /// The state the review is in.
    pub from: ReviewState,
    /// The state that was asked for.
    pub to: ReviewState,
}

impl ReviewState {
    /// The states §5.2 draws an edge to from this one.
    pub fn successors(&self) -> &'static [ReviewState] {
        match self {
            // `export` is the only edge out. A review that has not been exported
            // has no packet, and a decision is bound to a packet hash.
            ReviewState::Pending => &[ReviewState::Exported],
            // `import` is the only edge out.
            ReviewState::Exported => &[ReviewState::Decided],
            ReviewState::Decided => &[],
        }
    }

    /// Check a transition. `Ok(())` means §5.2 draws it.
    ///
    /// Self-transitions are refused. For a review that is stronger than the
    /// cosmetic reason [`TaskState::transition_to`] gives: a second export would
    /// mint a second packet hash for one review, and a decision could then be
    /// bound to whichever of the two suited whoever wrote it.
    pub fn transition_to(&self, to: ReviewState) -> Result<(), ReviewTransitionError> {
        if self.successors().contains(&to) {
            Ok(())
        } else {
            Err(ReviewTransitionError { from: *self, to })
        }
    }
}

/// What a human decided about a review packet — §6.5's imported decision.
///
/// The wire form is lower-case with an underscore (`revise_plan`), which is why
/// this is not a [`crate::state`] `state_enum!`: those persist
/// `SCREAMING_SNAKE_CASE`, and this value is typed by a person into a YAML file
/// that §6.5 spells in lower case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// The work is acceptable. §4.3's `REVIEW_ACCEPTANCE` authorizes it.
    Accept,
    /// Send it back for a bounded repair attempt. The repair budget is **not**
    /// reset — ADR-0009 makes the ceiling durable, and a review round trip is
    /// not a way to buy more attempts.
    Repair,
    /// The plan was wrong. A new version supersedes the affected tasks.
    RevisePlan,
    /// Seen, not decided. The only decision that moves no task.
    Pause,
    /// Abandon the task.
    Stop,
}

impl ReviewDecision {
    /// Every decision, in §6.5's declaration order.
    pub const ALL: &'static [ReviewDecision] = &[
        ReviewDecision::Accept,
        ReviewDecision::Repair,
        ReviewDecision::RevisePlan,
        ReviewDecision::Pause,
        ReviewDecision::Stop,
    ];

    /// The exact spelling §6.5 uses, and the one a human writes.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewDecision::Accept => "accept",
            ReviewDecision::Repair => "repair",
            ReviewDecision::RevisePlan => "revise_plan",
            ReviewDecision::Pause => "pause",
            ReviewDecision::Stop => "stop",
        }
    }

    /// The `task.state` this decision moves the task to, or `None` for `pause`.
    ///
    /// Every `Some` here is an edge §5.2 draws from `AWAITING_REVIEW`, and a test
    /// holds that correspondence rather than trusting this table.
    pub fn task_target(&self) -> Option<TaskState> {
        match self {
            ReviewDecision::Accept => Some(TaskState::Complete),
            ReviewDecision::Repair => Some(TaskState::Repairing),
            ReviewDecision::RevisePlan => Some(TaskState::Superseded),
            // See the module docs: pausing is not a transition.
            ReviewDecision::Pause => None,
            ReviewDecision::Stop => Some(TaskState::Cancelled),
        }
    }
}

impl std::fmt::Display for ReviewDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A decision string that is not one of §6.5's five.
///
/// Carries the value so the refusal can name it, and lists the five so the human
/// who mistyped one does not have to go and find the specification.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{value:?} is not a review decision; §6.5 defines exactly accept, repair, \
     revise_plan, pause and stop"
)]
pub struct ParseReviewDecisionError {
    /// The rejected value.
    pub value: String,
}

impl std::str::FromStr for ReviewDecision {
    type Err = ParseReviewDecisionError;

    /// Exact match only.
    ///
    /// No trimming and no case folding, deliberately. A decision file is
    /// machine-generated at export and hand-edited at import, so the value that
    /// arrives here is either one of five literals or something a human should be
    /// told about — and `ACCEPT` silently meaning `accept` is the beginning of
    /// `Accept ` and `accept;` meaning it too.
    fn from_str(value: &str) -> Result<ReviewDecision, ParseReviewDecisionError> {
        ReviewDecision::ALL
            .iter()
            .find(|decision| decision.as_str() == value)
            .copied()
            .ok_or_else(|| ParseReviewDecisionError {
                value: value.to_string(),
            })
    }
}

impl<'de> serde::Deserialize<'de> for ReviewDecision {
    /// Routed through [`FromStr`] so the YAML path and the CLI path cannot
    /// disagree about which five spellings are legal.
    fn deserialize<D>(deserializer: D) -> Result<ReviewDecision, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}
