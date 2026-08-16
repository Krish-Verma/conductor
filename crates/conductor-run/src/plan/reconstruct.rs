//! §3.5's recovery path — rebuilding project truth after `conductor.db` is
//! gone.
//!
//! # The sentence this module implements
//!
//! §3.5: *"**Recovery from total local loss:** re-register the project → read
//! `.conductor/` → rebuild the task list from the approved plan → scan
//! `workspaces/` for `.conductor-run.json` descriptors and reconcile each
//! against git → read commit trailers to reconstruct which runs produced which
//! commits under which approvals."*
//!
//! This module is the **first three clauses**. The fourth and fifth are about
//! execution state — which workspaces exist, which runs produced which commits —
//! and they belong to the reconciler and to `recovery`, which already own
//! descriptor scanning and startup reconciliation. Folding them in here would
//! make one function that both rebuilds git truth and adjudicates live processes,
//! and those two fail in different ways: the first is deterministic and total,
//! the second is a judgement about the world.
//!
//! # Why this exists as a path at all
//!
//! Every individual step already had a function — [`ledger::register_project`],
//! [`ledger::register_plan_version`], [`decision::register_decisions`],
//! [`materialize::materialize`]. What did not exist was the *order*, and one
//! step that only recovery needs: [`ledger::adopt_approval`]. A caller that
//! assembled these by hand would have to know that approval must be restored
//! before materialisation (because §4.3's gate refuses an unapproved plan), that
//! supersession must run after every adoption (because it is relative to the
//! newest approved version), and that decisions must be registered against a
//! project row that already exists. Getting that order wrong fails in ways that
//! look like data loss.
//!
//! # What comes back, and what does not
//!
//! §3.5 draws the line, and both halves are asserted in
//! `tests/plan_reconstruct.rs`:
//!
//! * **Not lost** — project identity and configuration, every plan version,
//!   every approval that left a receipt, the task list rebuilt from the approved
//!   plan, every decision. Policy and verification definitions are files and are
//!   never in the store to begin with, so they are "not lost" by construction;
//!   the tests still assert they load to the same values, because a promise that
//!   is true by construction is the easiest kind to break by accident.
//! * **Lost, deliberately** — run and attempt history, timings, the event
//!   journal, the verification cache, pending approval requests and the grants
//!   that satisfy them, unresolved findings, lease state. None of it is
//!   reconstructed here, and a rebuild that resurrected a `RUNNING` run would be
//!   asserting the existence of a process that does not exist.

use std::collections::BTreeSet;
use std::path::Path;

use conductor_core::{PlanVersionId, PlanVersionState, ProjectId};
use conductor_store::{DecisionRow, ProjectRow, Store};

use super::validate::ValidatedPlan;
use super::{Approval, LedgerError, PLANS_DIR, ledger, materialize};
use crate::decision::{self, DecisionSyncError};
use crate::verify::profile;

/// Anything that stops `.conductor/` from being rebuilt into rows.
///
/// Every variant is a refusal. A partial reconstruction is left in place rather
/// than rolled back: the steps are individually idempotent, so re-running after
/// fixing the cause completes it, and unwinding would mean deleting rows that
/// are correct.
#[derive(Debug, thiserror::Error)]
pub enum ReconstructError {
    /// The ledger refused — an unreadable project file, a plan that does not
    /// validate, a receipt that does not match its document.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    /// A decision could not be read or its `supersedes` did not resolve.
    #[error(transparent)]
    Decision(#[from] DecisionSyncError),
    /// The task list could not be rebuilt from the approved plan.
    #[error(transparent)]
    Materialize(#[from] materialize::MaterializeError),
    /// `.conductor/verification.yaml` is present and unreadable.
    ///
    /// Refused rather than treated as an empty catalogue: §3.7 refuses *"any
    /// acceptance criterion not bound to at least one check"*, so an empty
    /// catalogue turns every bound criterion into a validation defect, and the
    /// reader would be sent to the plan for a fault in the verification file.
    #[error("the verification definitions at {path} cannot be read: {detail}")]
    Verification {
        /// The file, in the registered tree.
        path: std::path::PathBuf,
        /// What is wrong with it.
        detail: String,
    },
    /// `.conductor/plans/` could not be listed.
    #[error("{what} at {path}: {source}")]
    Io {
        /// What was being read.
        what: &'static str,
        /// The path.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

/// One plan version, as rebuilt.
#[derive(Debug)]
pub struct RebuiltVersion {
    /// `.conductor/plans/v<N>/`.
    pub version: u32,
    /// §5.1's `plan_version.id`.
    pub id: PlanVersionId,
    /// Where §5.2's machine has it after the rebuild.
    pub state: PlanVersionState,
    /// The validated document, kept so a caller can materialise without
    /// re-reading the file.
    pub plan: ValidatedPlan,
    /// The receipt that was adopted, when there was one.
    pub approval: Option<Approval>,
}

/// What one run of [`reconstruct`] restored.
#[derive(Debug)]
pub struct Reconstruction {
    /// §5.1's `project` row, re-registered from `.conductor/project.yaml`.
    pub project: ProjectRow,
    /// Every version found under `.conductor/plans/`, ascending.
    pub versions: Vec<RebuiltVersion>,
    /// Every decision the store holds for the project after the sync.
    pub decisions: Vec<DecisionRow>,
    /// The task list, rebuilt from the newest approved plan. `None` when no
    /// version has a receipt — there is then no approved plan to rebuild from,
    /// which is a state to report rather than an error.
    pub tasks: Option<materialize::Materialization>,
}

impl Reconstruction {
    /// The newest version left `APPROVED`, if any.
    pub fn approved_version(&self) -> Option<u32> {
        self.versions
            .iter()
            .filter(|v| v.state == PlanVersionState::Approved)
            .map(|v| v.version)
            .max()
    }

    /// The validated document for one version.
    pub fn plan_for(&self, version: u32) -> Option<&ValidatedPlan> {
        self.versions
            .iter()
            .find(|v| v.version == version)
            .map(|v| &v.plan)
    }
}

/// Rebuild everything §3.5 puts on the **Not lost** list, from `.conductor/`
/// alone.
///
/// `store` may be empty — that is the case this exists for. It may also already
/// hold some of these rows, because every step is idempotent and recovery is
/// something an operator may run twice.
///
/// # The order, and why it is this order
///
/// 1. **The project**, because every later step resolves the repository root
///    from the `project` row rather than from the `repo_root` argument. That is
///    §3.3's control 2 — approval is read from the *registered* tree — and
///    routing the rest through the row is what keeps this function from becoming
///    a way to point Conductor at a different checkout.
/// 2. **Every plan version, ascending**, each registered and then — if it has a
///    receipt — adopted. Ascending so that the supersession in step 3 sees a
///    complete picture.
/// 3. **Supersession**, once, relative to the newest adopted version. Running it
///    per-version would retire a version that a later iteration then approves.
/// 4. **Decisions**, which need the `project` row and nothing else.
/// 5. **The task list**, last, because §4.3's gate in
///    [`materialize::materialize`] refuses a plan that is not `APPROVED` — so it
///    cannot run before step 2 has restored the approval.
pub fn reconstruct(
    store: &mut Store,
    repo_root: &Path,
    now_ms: i64,
) -> Result<Reconstruction, ReconstructError> {
    let project = ledger::register_project(store, repo_root, now_ms)?;
    let project_id = project.id.clone();
    let root = std::path::PathBuf::from(&project.root_path);

    let catalogue = catalogue(&root)?;

    let mut versions = Vec::new();
    for version in discover_versions(&root)? {
        let registered =
            match ledger::register_plan_version(store, &project_id, version, &catalogue) {
                Ok(registered) => registered,
                // Idempotence: a version this store already holds is not re-written.
                // `AlreadyRegistered` carries both hashes, so a *different* document
                // at the same version still refuses.
                Err(LedgerError::AlreadyRegistered { .. }) => {
                    let id = ledger::plan_version_id(&project_id, version);
                    let plan = revalidate(&root, version, &catalogue)?;
                    let state = state_of(store, &id)?;
                    versions.push(RebuiltVersion {
                        version,
                        id,
                        state,
                        plan,
                        approval: None,
                    });
                    continue;
                }
                Err(other) => return Err(other.into()),
            };

        let id = registered.row.id.clone();
        let approval = if root.join(super::approved_path(version)).exists() {
            Some(ledger::adopt_approval(store, &project_id, version)?)
        } else {
            None
        };
        let state = state_of(store, &id)?;
        versions.push(RebuiltVersion {
            version,
            id,
            state,
            plan: registered.plan,
            approval,
        });
    }

    // Step 3: one supersession, relative to the newest version that came back
    // approved.
    let newest_approved = versions
        .iter()
        .filter(|v| v.state == PlanVersionState::Approved)
        .map(|v| v.version)
        .max();
    if let Some(newest) = newest_approved {
        ledger::supersede(store, &project_id, newest)?;
        for rebuilt in &mut versions {
            rebuilt.state = state_of(store, &rebuilt.id)?;
        }
    }

    let decisions = decision::register_decisions(store, &project_id)?;

    let tasks = match newest_approved {
        Some(newest) => {
            let plan = versions
                .iter()
                .find(|v| v.version == newest)
                .map(|v| &v.plan)
                .expect("the newest approved version is one this loop rebuilt");
            Some(materialize::materialize(
                store,
                &project_id,
                newest,
                plan,
                now_ms,
            )?)
        }
        None => None,
    };

    Ok(Reconstruction {
        project,
        versions,
        decisions,
        tasks,
    })
}

/// Every `v<N>` under `.conductor/plans/` that holds a plan document, ascending.
///
/// A directory without a `plan.yaml` is skipped rather than refused: §3.1's
/// layout puts the `APPROVED` sidecar beside the document, so a half-written
/// version directory is something an operator can have on disk, and refusing the
/// whole recovery over it would make one stray directory unrecoverable.
fn discover_versions(root: &Path) -> Result<Vec<u32>, ReconstructError> {
    let dir = root.join(PLANS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No plans directory is an uninitialised project, not a failure.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ReconstructError::Io {
                what: "the plans directory",
                path: dir,
                source,
            });
        }
    };

    let mut versions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ReconstructError::Io {
            what: "the plans directory",
            path: dir.clone(),
            source,
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(digits) = name.strip_prefix('v') else {
            continue;
        };
        let Ok(version) = digits.parse::<u32>() else {
            continue;
        };
        if root.join(super::plan_path(version)).is_file() {
            versions.push(version);
        }
    }
    versions.sort_unstable();
    Ok(versions)
}

/// §3.7's catalogue, assembled from `.conductor/verification.yaml` the way
/// §3.7's clarification 3 says a caller must.
///
/// A **missing** file yields an empty catalogue, which is the honest reading:
/// a project that defines no checks has no check ids, and any plan that binds a
/// criterion will then fail validation naming the check it wanted. A file that
/// is present and unreadable is a refusal — see [`ReconstructError::Verification`].
fn catalogue(root: &Path) -> Result<BTreeSet<String>, ReconstructError> {
    let path = root.join(".conductor/verification.yaml");
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let loaded = profile::load(&path).map_err(|error| ReconstructError::Verification {
        path: path.clone(),
        detail: error.to_string(),
    })?;
    Ok(super::check_ids(&loaded.profile))
}

/// Re-parse and re-validate one version's document, for the idempotent path
/// where the row already exists.
fn revalidate(
    root: &Path,
    version: u32,
    catalogue: &BTreeSet<String>,
) -> Result<ValidatedPlan, ReconstructError> {
    let path = root.join(super::plan_path(version));
    let text = std::fs::read_to_string(&path).map_err(|source| ReconstructError::Io {
        what: "the plan document",
        path: path.clone(),
        source,
    })?;
    let document = super::parse(&text).map_err(LedgerError::from)?;
    let validated =
        super::validate(&document, catalogue).map_err(|report| LedgerError::Refused {
            version,
            report: Box::new(report),
        })?;
    Ok(validated)
}

fn state_of(store: &Store, id: &PlanVersionId) -> Result<PlanVersionState, ReconstructError> {
    let row = store
        .plan_version(id)
        .map_err(|error| ReconstructError::Ledger(LedgerError::Store(error)))?
        .ok_or_else(|| {
            ReconstructError::Ledger(LedgerError::UnknownPlanVersion {
                id: id.clone(),
                project: ProjectId::new("unknown").expect("a literal id"),
            })
        })?;
    Ok(row.state)
}
