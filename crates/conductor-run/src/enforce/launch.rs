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
//! # §4.3's binding rule, and the declaration that made it wirable (S11)
//!
//! > **Binding rule:** a task whose policy can produce an approval gate **may
//! > not run unattended** below tier A. Enforced by §4.2's eligibility check,
//! > not by documentation.
//!
//! [`crate::approval::gate::unattended_requirements`] decided that at S9 and
//! nothing called it, because the rule needs the set of actions a task may
//! perform and no task could declare one. S11's plan document can
//! ([`crate::plan::model::Task::actions`]), and materialization writes it to
//! `task.declared_actions`, so [`gate`] now derives the rule's vector and
//! [`crate::approval::gate::merge`]s it with the task's own before comparing.
//! The merge takes the **stronger** demand per dimension, so a task cannot talk
//! `control_surface: hard` down to `audit_only` by declaring a weaker
//! requirement of its own.
//!
//! ## The rule has two operands, and the policy is not the caller's to supply
//!
//! *"a task whose **policy** can produce an approval gate"* — answering that
//! needs the declaration **and** a policy. [`gate`] resolves the policy itself,
//! `active_run_for_task` → [`crate::policy::pinned_for_run`], the same path
//! [`super::policy_gate`] takes for the same reason (§4.4, acceptance row 23: a
//! run is judged by the snapshot it is pinned to for its entire life).
//!
//! It is deliberately **not** a parameter. A policy a caller passes is a policy
//! a caller can pass *wrongly*, and the wrong answer it produces — a §4.3
//! verdict about a different set of rules than the run is judged by — looks
//! exactly like the right one. There is no parameter to get wrong, so there is
//! nothing to keep in sync.
//!
//! ## `NULL` and `'[]'` are two different facts
//!
//! Schema v8 keeps `task.declared_actions` nullable on purpose, and this is the
//! call site where the distinction decides something:
//!
//! * **`NULL`** — no plan document has ever been read for this task. Every row
//!   written before schema v8, and everything `create_task` still writes. The
//!   rule does not apply and the gate keeps its S9 behaviour. Gating these would
//!   retroactively change the meaning of every existing task row, and would
//!   enforce §4.3 against tasks whose declaration nobody was ever asked for.
//! * **`'[]'`** — a plan document was read and declared zero actions. The rule
//!   *applies* and answers "no gate is possible", which is why reaching the
//!   policy still has to succeed: a rule that cannot read its second operand is
//!   undecided, and undecided is a refusal, never an empty policy. An empty
//!   policy allows everything.
//! * **Anything that does not decode** — a refusal, on
//!   [`GateError::UnreadableRequirements`]'s reasoning applied to the sibling
//!   column: the one task whose declaration went wrong must not be the one task
//!   the rule stops applying to.

use conductor_core::RunId;
use conductor_store::Store;

use crate::approval::gate::{merge, unattended_requirements};
use crate::containment::cache::{ProbeKey, lookup};
use crate::policy::eligibility::{Eligibility, ExecutionRequirements, OFFERS, check};
use crate::policy::model::{Action, ResolvedPolicy};

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
    /// The actions the task declares cannot be read.
    ///
    /// §4.3's binding rule asks what a task may do; a `declared_actions` column
    /// that does not decode into a list of §4.4 names leaves that unknown.
    /// Reading it as "no actions" would disable the rule for precisely the task
    /// whose declaration went wrong — [`GateError::UnreadableRequirements`]'s
    /// argument, applied to the other column §4.2's gate now reads.
    #[error(
        "task {task}'s declared_actions cannot be read, so §4.3's binding rule \
         cannot be decided and the launch is refused rather than run ungated: \
         {detail}"
    )]
    UnreadableActions {
        /// The task.
        task: String,
        /// The decoder's complaint.
        detail: String,
    },
    /// The policy §4.3's rule asks about cannot be reached.
    ///
    /// The rule is *"a task whose **policy** can produce an approval gate"*, and
    /// the policy is the one the run is pinned to — never one on disk, and never
    /// one a caller supplies. When it cannot be resolved the rule has no second
    /// operand, and an undecided rule is a refusal.
    ///
    /// Specifically **not** an empty policy: an empty policy can gate nothing,
    /// so treating a snapshot Conductor cannot decode as one would convert "we
    /// cannot tell what the rules are" into "there are no rules" on the exact
    /// path that exists to enforce them. [`super::policy_gate`] refuses the same
    /// substitution for the same reason.
    #[error(
        "task {task} was materialized from a plan, but the policy its run is \
         pinned to cannot be read, so §4.3's binding rule cannot be decided and \
         the launch is refused rather than run ungated: {detail}"
    )]
    UnreadablePolicy {
        /// The task.
        task: String,
        /// Why the policy could not be reached.
        detail: String,
    },
}

/// Decide whether this task may launch on this host.
///
/// `Ok(None)` means proceed. `Ok(Some(refusal))` means row 30: refuse, name the
/// dimension. `Err` means the gate could not decide, which is also a refusal —
/// callers must not treat it as permission.
///
/// Two sources feed the comparison and neither can weaken the other: the task's
/// own §4.2 vector, and §4.3's binding rule derived from what the task declares
/// it may do under the policy its run is pinned to. They are combined by
/// [`merge`], which takes the stronger demand per dimension.
pub fn gate(
    store: &Store,
    task_id: &conductor_core::TaskId,
    probe_key: &ProbeKey,
) -> Result<Option<Refusal>, GateError> {
    let requirements = merge(
        &requirements_for(store, task_id)?,
        &binding_rule_for(store, task_id)?,
    );

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

/// §4.3's binding rule for this task, as an §4.2 requirement vector.
///
/// `control_surface: hard` when the policy the run is pinned to can produce an
/// approval gate for anything the task declares; an empty vector otherwise,
/// which compares nothing and proceeds.
///
/// The decision itself is not made here — it is
/// [`crate::approval::gate::unattended_requirements`], written at S9 with a full
/// account of what "can produce a gate" means (including ADR-0010's capped
/// deny). This function's whole job is to establish its two inputs, and to
/// refuse when it cannot. See the module docs for why `NULL` and `'[]'` take
/// different paths through it.
///
/// # An out-of-taxonomy name is not a special case here
///
/// [`Action::parse`] is infallible: a name §4.4 has never heard of becomes
/// [`Action::Unknown`], which evaluation floors at `deny`. A deny is not
/// approvable, so an unknown action cannot produce an approval gate and
/// contributes nothing to this vector — which is the safe direction and not a
/// hole, because the action it names is refused outright rather than gated.
fn binding_rule_for(
    store: &Store,
    task_id: &conductor_core::TaskId,
) -> Result<ExecutionRequirements, GateError> {
    // `NULL`: no plan document has ever been read for this task, so there is no
    // declaration for the rule to apply to. Pre-S11 behaviour, deliberately
    // preserved — see the module docs.
    let Some(json) = store.declared_actions(task_id)? else {
        return Ok(ExecutionRequirements::new());
    };

    let names: Vec<String> =
        serde_json::from_str(&json).map_err(|error| GateError::UnreadableActions {
            task: task_id.as_str().to_string(),
            detail: format!(
                "the column holds {} bytes but does not decode as the JSON array \
                 of §4.4 action names the plan materializer writes: {error}",
                json.trim().len()
            ),
        })?;
    let actions: Vec<Action> = names.iter().map(|name| Action::parse(name)).collect();

    Ok(unattended_requirements(
        &pinned_policy(store, task_id)?,
        &actions,
    ))
}

/// The policy §4.3's rule asks about: the one the task's active run is pinned
/// to.
///
/// Resolved from the store, never from `.conductor/policy.yaml` and never from a
/// caller — the same discipline [`crate::policy::pinned_for_run`] exists to
/// enforce, so an edit to the file mid-run cannot change the answer.
///
/// A task with no active run has no pinned policy, and there is no fallback to
/// invent one. That is a refusal for the same reason an undecodable snapshot is:
/// the rule needs a policy, and any policy Conductor substitutes here is a
/// policy the run is not being judged by.
fn pinned_policy(
    store: &Store,
    task_id: &conductor_core::TaskId,
) -> Result<ResolvedPolicy, GateError> {
    let Some(run_id) = store.active_run_for_task(task_id)? else {
        return Err(GateError::UnreadablePolicy {
            task: task_id.as_str().to_string(),
            detail: "the task has no active run, so there is no pinned policy \
                     snapshot to decide the rule against"
                .to_string(),
        });
    };
    crate::policy::pinned_for_run(store.conn(), &run_id)
        .map(|pinned| pinned.policy)
        .map_err(|error| GateError::UnreadablePolicy {
            task: task_id.as_str().to_string(),
            detail: format!("run {run_id}: {error}"),
        })
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
