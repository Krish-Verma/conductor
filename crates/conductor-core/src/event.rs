//! The append-only evidence log's typed surface.
//!
//! `event` is evidence, not event sourcing (Part 5.1): state is never replayed
//! from it. S1 emits exactly one kind — `RUN_CLAIMED`, written inside the claim
//! transaction — so exactly one kind is defined here. Kinds are added by the
//! slice that emits them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ids::RunId;
use crate::state::ParseStateError;

/// `event.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventKind {
    /// A worker took ownership of a run (§4.7).
    RunClaimed,
}

impl EventKind {
    /// Every variant, in declaration order.
    pub const ALL: &'static [EventKind] = &[EventKind::RunClaimed];

    /// The exact string persisted in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::RunClaimed => "RUN_CLAIMED",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventKind {
    type Err = ParseStateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "RUN_CLAIMED" => Ok(EventKind::RunClaimed),
            other => Err(ParseStateError {
                type_name: "EventKind",
                value: other.to_string(),
            }),
        }
    }
}

/// `event.payload` for [`EventKind::RunClaimed`].
///
/// The fencing epoch is recorded in the evidence log at claim time so that a
/// disputed ownership question has an answer that does not depend on the
/// current value of `run.lease_epoch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunClaimedPayload {
    /// The run that was claimed.
    pub run_id: RunId,
    /// `run.lease_owner` as set by the claim.
    pub lease_owner: String,
    /// `run.lease_epoch` after the claim — the fencing token.
    pub lease_epoch: i64,
    /// `run.lease_expires_at` as set by the claim, epoch milliseconds.
    pub lease_expires_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips() {
        assert_eq!(EventKind::RunClaimed.as_str(), "RUN_CLAIMED");
        assert_eq!(
            "RUN_CLAIMED".parse::<EventKind>().expect("parse"),
            EventKind::RunClaimed
        );
        assert!("NOPE".parse::<EventKind>().is_err());
        assert_eq!(
            serde_json::to_string(&EventKind::RunClaimed).expect("serialize"),
            "\"RUN_CLAIMED\""
        );
    }

    #[test]
    fn payload_round_trips_through_json() {
        let payload = RunClaimedPayload {
            run_id: RunId::new("r-0041").expect("valid"),
            lease_owner: "worker-3-pid8812".to_string(),
            lease_epoch: 1,
            lease_expires_at: 1_760_000_060_000,
        };
        let json = serde_json::to_string(&payload).expect("serialize");
        let back: RunClaimedPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, payload);
    }
}
