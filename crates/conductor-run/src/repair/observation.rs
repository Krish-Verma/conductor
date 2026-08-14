//! What one attempt left behind, durably — master plan §4.6, schema v5.
//!
//! §4.6's loop-breakers read a *history*, and §4.7 kills and restarts the
//! process that holds it. So the history is not held: it is reconstructed, on
//! every decision, from rows the store wrote as each attempt finished.
//!
//! # The stored digest is never the answer
//!
//! `repair_observation` carries both the inputs §4.6 hashes and the digest the
//! writer derived from them. This module reads the **inputs** and recomputes,
//! every time. That is not caution about corruption so much as caution about
//! *change*: `fingerprint::normalize` is the load-bearing half of §4.6, and the
//! day it is improved, every digest already on disk becomes an artefact of the
//! old rule. A loop-breaker that compared old digests to new ones would silently
//! stop firing, which is the failure mode `fingerprint`'s own documentation
//! calls the silent one.
//!
//! The stored digest therefore exists for people — a packet quoting it, a human
//! reading the table — and `tests/repair_loop.rs` pins that it equals the
//! recomputed value at the moment it is written.

use conductor_core::RunId;
use conductor_store::{NewRepairObservation, Store, StoreError};

use super::breaker::{AttemptResult, RepairHistory};
use super::config::{RetryKind, retry_kind};
use super::failure::Failure;
use super::fingerprint::{Fingerprint, first_failing_assertion};
use crate::verify::runner::{CheckResult, VerificationReport};
use crate::worker::AttemptOutcomeRecord;

/// How much of a check log this module will read to find the first failing
/// assertion.
///
/// §4.5 forbids embedding logs, and a `cargo test` log can be megabytes. The
/// assertion §4.6 fingerprints is near the *start* of the failing output, so a
/// bounded prefix is enough — and a bound is what stops one pathological log
/// from deciding how much memory a repair decision costs.
pub const LOG_SCAN_BYTES: usize = 256 * 1024;

/// The four things an attempt that did not succeed can have been.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationKind {
    /// Verification returned `FAIL` (§4.5 → repair).
    Failed,
    /// §4.8's `NO_CHANGE`: the agent ran and the tree is identical to baseline.
    NoChange,
    /// The agent died before any verification result existed (row 2).
    Crashed,
    /// §4.7's infrastructure failure: `INCONCLUSIVE` or `VOID`.
    Infrastructure,
}

impl ObservationKind {
    /// The exact string persisted in `repair_observation.kind`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ObservationKind::Failed => "FAILED",
            ObservationKind::NoChange => "NO_CHANGE",
            ObservationKind::Crashed => "CRASHED",
            ObservationKind::Infrastructure => "INFRASTRUCTURE",
        }
    }

    /// Parse one. **Fails closed**: a kind this binary has never heard of is an
    /// error rather than a default, because every default here is a decision
    /// about somebody's budget.
    pub fn parse(text: &str) -> Option<ObservationKind> {
        match text {
            "FAILED" => Some(ObservationKind::Failed),
            "NO_CHANGE" => Some(ObservationKind::NoChange),
            "CRASHED" => Some(ObservationKind::Crashed),
            "INFRASTRUCTURE" => Some(ObservationKind::Infrastructure),
            _ => None,
        }
    }
}

/// One attempt, reduced to what §4.6 needs and what a packet quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// `attempt.id`.
    pub attempt_id: String,
    /// `attempt.ordinal`.
    pub ordinal: i64,
    /// What the attempt was.
    pub kind: ObservationKind,
    /// The ids of the checks that failed, sorted (§4.6 hashes `sorted(…)`).
    pub failing_checks: Vec<String>,
    /// The first failing assertion, as read from the check's log.
    pub assertion: String,
    /// The tree the checks observed.
    pub tree_hash: String,
}

impl Observation {
    /// §4.6's digest, **computed from this observation's own inputs**.
    ///
    /// Constructing a [`Failure`] is what does it: `Failure::new` computes the
    /// fingerprint rather than accepting one, so a value whose digest disagrees
    /// with its contents cannot exist.
    pub fn fingerprint(&self) -> Fingerprint {
        self.as_failure().fingerprint().clone()
    }

    /// This observation as §4.6's `Failure`, whatever kind it is.
    ///
    /// The non-`Failed` kinds have empty check sets and empty assertions, so
    /// their digests are all the same — which is correct and unused: [`Self::as_attempt_result`]
    /// never hands them to a loop-breaker.
    fn as_failure(&self) -> Failure {
        Failure::new(
            self.failing_checks.iter().cloned(),
            &self.assertion,
            &self.tree_hash,
        )
    }

    /// This observation as the thing [`super::breaker::decide`] reads.
    pub fn as_attempt_result(&self) -> AttemptResult {
        match self.kind {
            ObservationKind::Failed => AttemptResult::Failed(self.as_failure()),
            ObservationKind::NoChange => AttemptResult::NoChange,
            ObservationKind::Crashed => AttemptResult::Crashed,
            ObservationKind::Infrastructure => AttemptResult::Infrastructure,
        }
    }

    /// The row to write, digest included.
    pub fn as_row(&self) -> NewRepairObservation {
        NewRepairObservation {
            attempt_id: self.attempt_id.clone(),
            ordinal: self.ordinal,
            kind: self.kind.as_str().to_string(),
            failing_checks: self.failing_checks.clone(),
            assertion: self.assertion.clone(),
            tree_hash: self.tree_hash.clone(),
            fingerprint: self.fingerprint().as_str().to_string(),
        }
    }
}

/// Anything reading the durable history can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ObservationError {
    /// The store said no.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// A `kind` column held something this binary does not model.
    ///
    /// Fails closed. Guessing would mean guessing whether an attempt cost the
    /// task's budget.
    #[error(
        "repair_observation.kind holds {0:?}, which is not a kind this binary \
         writes; refusing to guess what it cost"
    )]
    UnknownKind(String),
}

/// Rebuild §4.6's history for a run from durable state.
///
/// This is the function that makes S6's bound survive a restart. Everything it
/// returns was written by a committed transaction; nothing is carried in memory
/// between attempts.
pub fn history_for_run(store: &Store, run_id: &RunId) -> Result<RepairHistory, ObservationError> {
    let mut history = RepairHistory::new();
    for row in store.repair_observations_for_run(run_id)? {
        let kind = ObservationKind::parse(&row.kind)
            .ok_or_else(|| ObservationError::UnknownKind(row.kind.clone()))?;
        let observation = Observation {
            attempt_id: row.attempt_id,
            ordinal: row.ordinal,
            kind,
            failing_checks: row.failing_checks,
            assertion: row.assertion,
            tree_hash: row.tree_hash,
        };
        history.record(observation.as_attempt_result());
    }
    Ok(history)
}

/// Every observation of a run, in attempt order — what a packet's
/// `do_not_retry` list is built from.
pub fn observations_for_run(
    store: &Store,
    run_id: &RunId,
) -> Result<Vec<Observation>, ObservationError> {
    store
        .repair_observations_for_run(run_id)?
        .into_iter()
        .map(|row| {
            let kind = ObservationKind::parse(&row.kind)
                .ok_or_else(|| ObservationError::UnknownKind(row.kind.clone()))?;
            Ok(Observation {
                attempt_id: row.attempt_id,
                ordinal: row.ordinal,
                kind,
                failing_checks: row.failing_checks,
                assertion: row.assertion,
                tree_hash: row.tree_hash,
            })
        })
        .collect()
}

/// What repair makes of one finished attempt.
///
/// `None` means repair has nothing to say about it — either the attempt
/// succeeded, or §4.8 routed the run to a person (`CONTRADICTED`,
/// `OUT_OF_SCOPE`, `POLICY_SENSITIVE`, `CORRUPT`), and repair does not get to
/// overrule that by counting it as a failed attempt worth retrying.
///
/// # The order of the questions is the design
///
/// Verification first, because §4.5 makes it authoritative and because it is the
/// only source that can distinguish §4.7's two retry kinds. Only when there is
/// no verification at all does the attempt's own terminal state get a say — and
/// then a dead agent is [`ObservationKind::Crashed`] rather than
/// [`ObservationKind::NoChange`], because acceptance row 2 requires a crash
/// before edits to be retried while §4.6's breaker 3 stops an empty edit at
/// once. Reading both as `NO_CHANGE` would make one of those two rows wrong.
pub fn observe(
    attempt: &AttemptOutcomeRecord,
    verification: Option<&VerificationReport>,
    ordinal: i64,
) -> Option<Observation> {
    let attempt_id = attempt.attempt_id.as_str().to_string();

    if let Some(report) = verification {
        return match retry_kind(report) {
            RetryKind::Work => {
                let failing: Vec<&CheckResult> = report
                    .results
                    .iter()
                    .filter(|r| r.outcome == conductor_core::VerificationOutcome::Fail)
                    .collect();
                // `first` in the schedule's order, which is §4.5's order:
                // required, then triggered conditionals, then invariants.
                let first = failing.first()?;
                let mut ids: Vec<String> = failing.iter().map(|r| r.check_id.clone()).collect();
                ids.sort();
                Some(Observation {
                    attempt_id,
                    ordinal,
                    kind: ObservationKind::Failed,
                    failing_checks: ids,
                    assertion: assertion_of(first),
                    tree_hash: first.tree_hash.clone(),
                })
            }
            RetryKind::Infrastructure => Some(Observation {
                attempt_id,
                ordinal,
                kind: ObservationKind::Infrastructure,
                failing_checks: Vec::new(),
                assertion: String::new(),
                tree_hash: report
                    .results
                    .first()
                    .map(|r| r.tree_hash.clone())
                    .unwrap_or_default(),
            }),
            // Everything passed and the run still stopped: that is row 16's
            // moved target branch or a disagreement about what to commit, and
            // neither is something another agent attempt would fix.
            RetryKind::None => None,
        };
    }

    // §4.8 already sent this to a person. Repair does not get a second opinion.
    if attempt.route.requires_human() {
        return None;
    }

    if matches!(
        attempt.attempt_state,
        conductor_core::AttemptState::Crashed
            | conductor_core::AttemptState::TimedOut
            | conductor_core::AttemptState::Stale
    ) {
        return Some(Observation {
            attempt_id,
            ordinal,
            kind: ObservationKind::Crashed,
            failing_checks: Vec::new(),
            assertion: String::new(),
            tree_hash: String::new(),
        });
    }

    if attempt.verdict == conductor_git::Verdict::NoChange {
        return Some(Observation {
            attempt_id,
            ordinal,
            kind: ObservationKind::NoChange,
            failing_checks: Vec::new(),
            assertion: String::new(),
            tree_hash: String::new(),
        });
    }

    None
}

/// Record one observation, fenced (§4.7).
pub fn record(
    store: &mut Store,
    fence: &conductor_core::Fence,
    observation: &Observation,
    now_ms: i64,
) -> Result<(), ObservationError> {
    store.record_repair_observation(fence, &observation.as_row(), now_ms)?;
    Ok(())
}

/// The first failing assertion of one check, from whatever evidence exists.
///
/// Two sources, and both are needed. A check that just ran carries a redacted
/// `excerpt`; a check served from §4.5's cache carries only a `log_path`,
/// because the cache stores the pointer and not the text — and repair loops are
/// exactly where §4.5 says the cache earns its keep, so the cached shape is the
/// common one here, not the exotic one.
///
/// The fallback is deliberately **deterministic**. A fingerprint built from
/// something that varies between runs of the same failure would make every
/// loop-breaker in §4.6 silently stop working.
fn assertion_of(result: &CheckResult) -> String {
    let text = result.excerpt.clone().or_else(|| {
        result
            .log_path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .map(|bytes| {
                let end = bytes.len().min(LOG_SCAN_BYTES);
                String::from_utf8_lossy(&bytes[..end]).into_owned()
            })
    });

    text.as_deref()
        .and_then(first_failing_assertion)
        .unwrap_or_else(|| {
            format!(
                "check {} failed with exit code {}",
                result.check_id,
                result
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )
        })
}
