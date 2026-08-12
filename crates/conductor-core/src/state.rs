//! State enums for the `TEXT` state columns the store persists.
//!
//! Sources: master plan §5.2 (state machines) and Part 5.1 (column comments).
//! Transition rules are **not** here — they are S3/S5. This module only decides
//! which strings are legal in which column, and how they map to Rust.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A state column held a string that is not a member of its enum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{type_name} has no variant {value:?}")]
pub struct ParseStateError {
    /// The enum that rejected the value.
    pub type_name: &'static str,
    /// The rejected value.
    pub value: String,
}

macro_rules! state_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $( $variant:ident => $text:literal ),+ $(,)? }
        terminal: [ $( $terminal:ident ),* $(,)? ]
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "SCREAMING_SNAKE_CASE")]
        pub enum $name {
            $(
                #[doc = concat!("`", $text, "`")]
                $variant,
            )+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[ $( $name::$variant ),+ ];

            /// The exact string persisted in the database.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $( $name::$variant => $text ),+
                }
            }

            /// Terminal states, per master plan §5.2.
            pub fn is_terminal(&self) -> bool {
                matches!(self, $( $name::$terminal )|* )
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ParseStateError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $text => Ok($name::$variant), )+
                    other => Err(ParseStateError {
                        type_name: stringify!($name),
                        value: other.to_string(),
                    }),
                }
            }
        }
    };
}

state_enum! {
    /// `task.state` — master plan §5.2, "Task (12 states)".
    TaskState {
        Pending => "PENDING",
        Ready => "READY",
        Running => "RUNNING",
        Reconciling => "RECONCILING",
        AwaitingApproval => "AWAITING_APPROVAL",
        Verifying => "VERIFYING",
        Blocked => "BLOCKED",
        AwaitingReview => "AWAITING_REVIEW",
        Repairing => "REPAIRING",
        Complete => "COMPLETE",
        Cancelled => "CANCELLED",
        Superseded => "SUPERSEDED",
    }
    terminal: [Complete, Cancelled, Superseded]
}

state_enum! {
    /// `run.state`.
    ///
    /// §5.2 says the run state "mirrors its task", which would make this
    /// identical to [`TaskState`]. It is not, and the difference is forced by
    /// the schema and the claim statement, not chosen here:
    ///
    /// * `RECOVERING` appears in the §4.7 claim predicate
    ///   (`state IN ('READY','RECOVERING')`) but is **not** one of the twelve
    ///   task states. Without it the claim's own SQL could never match half of
    ///   what it selects for. Reported as a master-plan discrepancy in the S1
    ///   completion report; resolving it is the plan owner's call, not S1's.
    /// * The terminal set is fixed by the partial index
    ///   `ix_run_one_active_per_task ... WHERE state NOT IN
    ///   ('COMPLETE','CANCELLED','SUPERSEDED')`.
    RunState {
        Pending => "PENDING",
        Ready => "READY",
        Recovering => "RECOVERING",
        Running => "RUNNING",
        Reconciling => "RECONCILING",
        AwaitingApproval => "AWAITING_APPROVAL",
        Verifying => "VERIFYING",
        Blocked => "BLOCKED",
        AwaitingReview => "AWAITING_REVIEW",
        Repairing => "REPAIRING",
        Complete => "COMPLETE",
        Cancelled => "CANCELLED",
        Superseded => "SUPERSEDED",
    }
    terminal: [Complete, Cancelled, Superseded]
}

state_enum! {
    /// `plan_version.state` — Part 5.1 column comment and §5.2 "Plan (5 states)".
    PlanVersionState {
        Draft => "DRAFT",
        Validated => "VALIDATED",
        AwaitingApproval => "AWAITING_APPROVAL",
        Approved => "APPROVED",
        Superseded => "SUPERSEDED",
    }
    terminal: [Superseded]
}

state_enum! {
    /// `attempt.outcome` — Part 5.1 column comment.
    ///
    /// The attempt *state* diagram in §5.2 is not represented here because the
    /// `attempt` table has no `state` column; only `outcome` is persisted.
    AttemptOutcome {
        Exited => "EXITED",
        Crashed => "CRASHED",
        TimedOut => "TIMED_OUT",
        Stale => "STALE",
        Reconciled => "RECONCILED",
    }
    terminal: [Reconciled]
}

impl RunState {
    /// True while the run still occupies its task's single active-run slot,
    /// i.e. exactly the predicate of `ix_run_one_active_per_task`.
    pub fn occupies_active_slot(&self) -> bool {
        !self.is_terminal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_has_twelve_variants() {
        assert_eq!(TaskState::ALL.len(), 12);
    }

    #[test]
    fn run_state_covers_the_claim_predicate() {
        assert_eq!(RunState::Ready.as_str(), "READY");
        assert_eq!(RunState::Recovering.as_str(), "RECOVERING");
        assert_eq!(RunState::Running.as_str(), "RUNNING");
    }

    #[test]
    fn text_round_trips_for_every_variant() {
        for s in TaskState::ALL {
            assert_eq!(&s.as_str().parse::<TaskState>().expect("parse"), s);
        }
        for s in RunState::ALL {
            assert_eq!(&s.as_str().parse::<RunState>().expect("parse"), s);
        }
        for s in PlanVersionState::ALL {
            assert_eq!(&s.as_str().parse::<PlanVersionState>().expect("parse"), s);
        }
        for s in AttemptOutcome::ALL {
            assert_eq!(&s.as_str().parse::<AttemptOutcome>().expect("parse"), s);
        }
    }

    #[test]
    fn serde_agrees_with_the_persisted_text() {
        // Two encodings of the same thing must not be allowed to drift: the
        // database sees as_str(), a JSON packet sees serde.
        for s in RunState::ALL {
            let json = serde_json::to_string(s).expect("serialize");
            assert_eq!(json, format!("\"{}\"", s.as_str()));
        }
        for s in TaskState::ALL {
            let json = serde_json::to_string(s).expect("serialize");
            assert_eq!(json, format!("\"{}\"", s.as_str()));
        }
        for s in AttemptOutcome::ALL {
            let json = serde_json::to_string(s).expect("serialize");
            assert_eq!(json, format!("\"{}\"", s.as_str()));
        }
    }

    #[test]
    fn unknown_state_is_rejected_not_defaulted() {
        let err = "NOT_A_STATE".parse::<RunState>().unwrap_err();
        assert_eq!(err.type_name, "RunState");
        assert_eq!(err.value, "NOT_A_STATE");
    }

    #[test]
    fn terminal_sets_match_the_partial_index_predicate() {
        let terminal: Vec<&str> = RunState::ALL
            .iter()
            .filter(|s| s.is_terminal())
            .map(|s| s.as_str())
            .collect();
        assert_eq!(terminal, vec!["COMPLETE", "CANCELLED", "SUPERSEDED"]);
        assert!(RunState::Running.occupies_active_slot());
        assert!(!RunState::Complete.occupies_active_slot());
    }
}
