//! The append-only evidence log's typed surface.
//!
//! `event` is evidence, not event sourcing (Part 5.1): state is never replayed
//! from it. Kinds are added by the slice that emits them: S1 added
//! `RUN_CLAIMED`, S3 adds the supervision and recovery kinds.

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
    /// A lease lapsed and the run was forced to `RECONCILING` (§5.2 restart).
    LeaseExpired,
    /// The lease was extended by its owner, the child having been observed alive.
    LeaseRenewed,
    /// An attempt row was opened.
    AttemptCreated,
    /// A process was spawned and its identity recorded.
    AttemptStarted,
    /// An attempt reached a terminal outcome.
    AttemptFinished,
    /// Conductor looked at the repository and classified the attempt (§4.8).
    AttemptReconciled,
    /// A run moved state.
    RunStateChanged,
    /// A side effect was declared before being performed (§4.7).
    EffectIntended,
    /// A side effect's receipt was recorded.
    EffectConfirmed,
    /// A side effect's precondition could not be decided. The run halts.
    EffectAmbiguous,
    /// Startup recovery adopted, staled or blocked a run.
    RecoveryDecision,
    /// A finding was raised. Findings never auto-resolve (§4.8).
    FindingRaised,
    /// A **human** resolved a finding — §4.8's decision path, S13.
    ///
    /// Its own kind rather than a second [`EventKind::FindingRaised`] carrying
    /// `resolved: true`, because §4.8's rule is that findings never auto-resolve:
    /// the journal has to make "raised" and "a person answered this" different
    /// events, or an auditor counting `FINDING_RAISED` rows cannot tell one
    /// finding answered from two findings raised.
    FindingResolved,
    /// A review moved state — §5.2's `PENDING → EXPORTED → DECIDED`, S13.
    ///
    /// Not [`EventKind::RunStateChanged`]: a review's states are not run states,
    /// and overloading one kind for two machines means a reader has to inspect the
    /// payload to know which machine moved. None of `PENDING`, `EXPORTED` or
    /// `DECIDED` is a spelling any [`RunState`](crate::RunState) uses, so the
    /// ambiguity would be silent rather than caught.
    ReviewStateChanged,
    /// A verification check produced a result bound to a tree hash (§4.5).
    VerificationRecorded,
}

impl EventKind {
    /// Every variant, in declaration order.
    pub const ALL: &'static [EventKind] = &[
        EventKind::RunClaimed,
        EventKind::LeaseExpired,
        EventKind::LeaseRenewed,
        EventKind::AttemptCreated,
        EventKind::AttemptStarted,
        EventKind::AttemptFinished,
        EventKind::AttemptReconciled,
        EventKind::RunStateChanged,
        EventKind::EffectIntended,
        EventKind::EffectConfirmed,
        EventKind::EffectAmbiguous,
        EventKind::RecoveryDecision,
        EventKind::FindingRaised,
        EventKind::FindingResolved,
        EventKind::ReviewStateChanged,
        EventKind::VerificationRecorded,
    ];

    /// The exact string persisted in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::RunClaimed => "RUN_CLAIMED",
            EventKind::LeaseExpired => "LEASE_EXPIRED",
            EventKind::LeaseRenewed => "LEASE_RENEWED",
            EventKind::AttemptCreated => "ATTEMPT_CREATED",
            EventKind::AttemptStarted => "ATTEMPT_STARTED",
            EventKind::AttemptFinished => "ATTEMPT_FINISHED",
            EventKind::AttemptReconciled => "ATTEMPT_RECONCILED",
            EventKind::RunStateChanged => "RUN_STATE_CHANGED",
            EventKind::EffectIntended => "EFFECT_INTENDED",
            EventKind::EffectConfirmed => "EFFECT_CONFIRMED",
            EventKind::EffectAmbiguous => "EFFECT_AMBIGUOUS",
            EventKind::RecoveryDecision => "RECOVERY_DECISION",
            EventKind::FindingRaised => "FINDING_RAISED",
            EventKind::FindingResolved => "FINDING_RESOLVED",
            EventKind::ReviewStateChanged => "REVIEW_STATE_CHANGED",
            EventKind::VerificationRecorded => "VERIFICATION_RECORDED",
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
        EventKind::ALL
            .iter()
            .copied()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| ParseStateError {
                type_name: "EventKind",
                value: s.to_string(),
            })
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
    fn every_kind_round_trips_and_serde_agrees_with_the_column() {
        for kind in EventKind::ALL {
            assert_eq!(&kind.as_str().parse::<EventKind>().expect("parse"), kind);
            assert_eq!(
                serde_json::to_string(kind).expect("serialize"),
                format!("\"{}\"", kind.as_str())
            );
        }
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
