//! §4.2's eligibility gate, at the call site it was written for.
//!
//! ```text
//! before launching an attempt:
//!     caps = probe_cache.get(adapter, launcher, host)      // None if stale/absent
//!     if any required dimension > caps dimension:
//!         refuse to launch unattended
//!         emit the dimension, the requirement, and the measured value
//!         offer: attended mode | different adapter | a sandbox launcher
//! ```
//!
//! S7 wrote the decision — [`crate::policy::eligibility::check`], a pure
//! function with a full precedence matrix and a positive control. S9 owns the
//! sentence "before launching an attempt", and this module is that sentence and
//! nothing else. It re-decides nothing: it gathers the two inputs, calls S7's
//! function, and turns a refusal into the durable state acceptance row 30 asks
//! for.
//!
//! # Why the gate runs before the claim
//!
//! §5.2 draws `READY ──claim+eligibility──► RUNNING` — one edge, two gates. The
//! claim statement moves the run to `RUNNING` atomically, so a gate that ran
//! *after* claiming would have to walk a launched run back, and §4.8's "every
//! exit from `RUNNING` passes through reconciliation" would then need an
//! exception for runs that never ran anything. Refusing first keeps that
//! invariant literally true and gives row 30 exactly what it asks for: the
//! attempt never starts, and nothing was cloned, spawned or recorded on its
//! behalf.
//!
//! # Where the requirement comes from, and why not from the caller
//!
//! From `task.execution_requirements` — a durable column (schema v7). §4.2 puts
//! the vector in `.conductor/project.yaml` "or a per-task override" and S11 owns
//! that file; until it exists the task row is the only durable home for it.
//!
//! It is deliberately **not** a field on `VerticalConfig`. A requirement a
//! caller passes is a requirement a caller can omit, and every future launch
//! path would have to remember. Reading it from the row the run already resolves
//! means a new call site inherits the gate rather than having to opt into it.
//!
//! The probe *key* is a config field, because it describes the host and adapter
//! rather than the task — and getting it wrong fails safe: an unrecognised key
//! misses the cache, a miss yields `fail_closed()`, and every requirement above
//! `None` then refuses.
//!
//! # What this does not yet enforce
//!
//! §4.3's **binding rule** — "a task whose policy can produce an approval gate
//! may not run unattended below tier A" — is decided by
//! [`crate::approval::gate::unattended_requirements`] and is still not wired.
//! It needs the set of actions a task may perform, and no such declaration
//! exists on a task at S9; inventing one here would be guessing at the schema
//! S11 owns. The function, its tests and this note are the honest state: the
//! rule is decided and not reachable. **S11 owns wiring it**, and until then the
//! binding rule must not be scored as enforced.

use conductor_core::RunId;
use conductor_store::Store;

use crate::containment::cache::{ProbeKey, lookup};
use crate::policy::eligibility::{Eligibility, ExecutionRequirements, OFFERS, check};

/// Why a launch was refused, in the words row 30 asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// One line per unmet dimension: what was required, what was measured.
    pub detail: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Anything the gate itself can fail with.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// The store said no.
    #[error("store: {0}")]
    Store(#[from] conductor_store::StoreError),
    /// The probe cache could not be read.
    #[error("probe cache: {0}")]
    Cache(String),
    /// The task's requirement vector cannot be read.
    ///
    /// A refusal, never a default. Treating an unparseable requirement as "no
    /// requirements" would mean a typo silently disables the gate, and an
    /// operator who wrote a requirement would believe it was being enforced.
    #[error(
        "task {task}'s execution_requirements cannot be read, so the launch is \
         refused rather than run ungated: {detail}"
    )]
    UnreadableRequirements {
        /// The task.
        task: String,
        /// The parser's complaint.
        detail: String,
    },
}

/// Decide whether this task may launch on this host.
///
/// `Ok(None)` means proceed. `Ok(Some(refusal))` means row 30: refuse, name the
/// dimension. `Err` means the gate could not decide, which is also a refusal —
/// callers must not treat it as permission.
pub fn gate(
    store: &Store,
    task_id: &conductor_core::TaskId,
    probe_key: &ProbeKey,
) -> Result<Option<Refusal>, GateError> {
    let requirements = requirements_for(store, task_id)?;

    // §4.2: "It does not rank adapters and does not choose between eligible
    // options." An empty vector compares nothing and proceeds — which is what
    // keeps an unprobed host usable for work that gates on nothing, rather than
    // turning the gate into an outage.
    if requirements.is_empty() {
        return Ok(None);
    }

    let found = lookup(store.conn(), probe_key).map_err(|e| GateError::Cache(e.to_string()))?;

    match check(&found, &requirements) {
        Eligibility::Eligible { .. } => Ok(None),
        Eligibility::Refused {
            shortfalls, probe, ..
        } => {
            let mut detail = String::new();
            for shortfall in &shortfalls {
                detail.push_str(&shortfall.to_string());
                detail.push_str("; ");
            }
            detail.push_str(&format!("probe: {probe:?}"));
            for offer in OFFERS {
                detail.push_str("; offer: ");
                detail.push_str(offer);
            }
            Ok(Some(Refusal { detail }))
        }
    }
}

/// The task's §4.2 vector, read from the row and parsed by the gate's own
/// parser.
///
/// The column holds §4.2's YAML **block** — the `execution_requirements:` key
/// and its mapping — exactly as it appears in `.conductor/project.yaml`. One
/// format, one parser: a bare mapping accepted here would be a second dialect
/// that S11's file could not use.
///
/// # The empty-but-present case is a refusal
///
/// `ExecutionRequirements::parse_yaml` returns an **empty vector** for a
/// document that does not contain the `execution_requirements` key — which is
/// correct for a `project.yaml` that simply does not mention the block, and
/// dangerous here. Somebody who wrote requirements into this column and
/// mis-nested them by two spaces would get silence: no error, no requirement,
/// and a gate that compares nothing and proceeds. They would believe the
/// dimension was being enforced.
///
/// So a column that holds something but yields nothing is treated as unreadable
/// rather than as "no requirements". The distinction that matters is
/// *absent* (`NULL` — nothing gated, deliberately) versus *present and
/// meaningless* (somebody meant something and it did not survive).
fn requirements_for(
    store: &Store,
    task_id: &conductor_core::TaskId,
) -> Result<ExecutionRequirements, GateError> {
    let Some(yaml) = store.execution_requirements(task_id)? else {
        return Ok(ExecutionRequirements::new());
    };
    if yaml.trim().is_empty() {
        return Ok(ExecutionRequirements::new());
    }

    let parsed = ExecutionRequirements::parse_yaml(&yaml).map_err(|e| {
        GateError::UnreadableRequirements {
            task: task_id.as_str().to_string(),
            detail: e.to_string(),
        }
    })?;
    if parsed.is_empty() {
        return Err(GateError::UnreadableRequirements {
            task: task_id.as_str().to_string(),
            detail: format!(
                "the column holds {} bytes of YAML but no `execution_requirements` \
                 mapping was found in it, so nothing would be gated; §4.2's block \
                 is `execution_requirements:` followed by dimension: enforcement \
                 pairs",
                yaml.trim().len()
            ),
        });
    }
    Ok(parsed)
}

/// The measurement key for this adapter on this host.
///
/// §4.2 keys the cache on `(adapter_version, launcher_version, os_version)`
/// precisely because "sandbox behaviour changes with OS and CLI versions, and a
/// hardcoded table would silently become a lie after an upgrade".
///
/// An adapter whose version cannot be read yields `"unknown"`, which is a
/// **different key** from any version the probe suite has measured — so the
/// lookup misses, the capabilities are `fail_closed()`, and any task with a real
/// requirement is refused. That is the intended direction: a version Conductor
/// cannot establish is not a version it may assume is safe.
///
/// `launcher` is `"none"` at S9 because no launcher is wired yet. S10 and S15
/// supply real ones, and because the key changes, a measurement taken without a
/// launcher cannot be mistaken for one taken with it.
pub fn probe_key_for(adapter_id: &str, host: &crate::containment::probe::Host) -> ProbeKey {
    let adapter_version = host
        .tools
        .get(adapter_id)
        .map(|tool| tool.version.clone())
        .unwrap_or_else(|| "unknown".to_string());
    ProbeKey::new(
        adapter_id,
        adapter_version,
        "none",
        "n/a",
        host.os_version.clone(),
    )
}

/// A stable finding id for a refusal, so a re-attempted launch does not stack
/// duplicates.
pub fn finding_id(run_id: &RunId) -> String {
    format!("f-{}-INELIGIBLE_EXECUTION_MODE", run_id.as_str())
}
