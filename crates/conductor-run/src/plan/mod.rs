//! The plan document, its content hash, `plan validate`, and the ledger that
//! records both halves of §3.1 — master plan §3.1, §3.2, §3.3, §3.6, §3.7,
//! §5.1, §5.2 (slice S11).
//!
//! # What a plan is for
//!
//! §3.1 splits Conductor's state in two. Git holds *"what we agreed to do, and
//! what we are allowed to do"*; SQLite holds *"what actually happened, and what
//! is happening now"*. Four of this module's five submodules are the first half
//! — the files as they exist in `.conductor/`, before any of it becomes rows.
//!
//! §3.2 gives four independently sufficient reasons the plan lives in git: an
//! approved plan must survive loss of `conductor.db`, must be reviewable as a
//! diff by a human, must travel with the repository to another machine, and
//! must be readable without Conductor installed. Every one of those is a
//! statement about a *file*, which is why the parsing, hashing and validation
//! here take text and a parsed document and never touch the store.
//!
//! # The pieces
//!
//! * [`model`] — the document. Milestones containing slices containing tasks,
//!   each task carrying scope globs, dependencies and acceptance criteria bound
//!   to named checks (§3.6). No `deny_unknown_fields`, and no `state:` field.
//! * [`hash`] — [`content_hash`], over canonical *semantics* rather than file
//!   bytes, so that §3.6 holds: "reformatting does **not** invalidate approval;
//!   changing any field **does**".
//! * [`validate`] — §3.7's refusals, each naming the id it is about.
//! * [`project`] — `.conductor/project.yaml` (§3.1): identity, adapter, scope
//!   defaults, review cadence and §4.2's `execution_requirements`.
//! * [`ledger`] — the **only** place the git half and the SQLite half meet.
//!   Registration (§5.1's `project` row), plan versions through §5.2's machine,
//!   approval, supersession, and §3.3's controls 2 and 3.
//! * [`materialize`] — §5.1's `task` rows from a validated plan, §3.6's stable
//!   ids, and acceptance row 21's rule that a run in flight finishes under the
//!   plan version it started on.
//!
//! # Where a plan's state lives
//!
//! Not here, and not in any type this module defines. §5.1's
//! `plan_version.state` column is spelled once, by
//! [`conductor_core::PlanVersionState`], and [`ledger`] is what moves it —
//! through `conductor_store`'s legality table, so §5.2's machine is the only
//! thing that decides which transitions exist. A document cannot declare its
//! own state (§3.7's clarification 4), and [`model`]'s docs record why the
//! duplicate enum this module used to carry was deleted rather than kept.
//!
//! # What S11's scope line still does not contain
//!
//! * **The `.conductor/**` rejection rule (§3.3's control 1).** A change to
//!   `.conductor/` arriving on a run branch is rejected at reconciliation with
//!   a finding. Needs the reconciler and a run branch.
//! * **Decisions.** §5.1's `decision` table has a store API and no reader for
//!   `.conductor/decisions/*.md`.
//! * **§3.7's "scope globs matching no path"** and its warning on long YAML
//!   comments (§3.6). Both need something the pure core does not take: a
//!   working tree, and the file's comment tokens, which the parser discards.

pub mod hash;
pub mod ledger;
pub mod materialize;
pub mod model;
pub mod project;
pub mod reconstruct;
pub mod validate;

pub use hash::{CANONICAL_VERSION, PlanHash, canonical_bytes, content_hash};
pub use ledger::{
    Approval, LedgerError, RegisteredPlanVersion, adopt_approval, approve, plan_version_id,
    register_plan_version, register_project, repo_identity, supersede, verify_approval,
};
pub use materialize::{CarriedTask, Materialization, MaterializeError, materialize};
pub use model::{
    AcceptanceCriterion, Milestone, Plan, PlanDocument, PlanError, Slice, Task, TaskScope, load,
    parse,
};
pub use project::{PROJECT_CONFIG_PATH, Project, ProjectError};
pub use reconstruct::{RebuiltVersion, ReconstructError, Reconstruction};
pub use validate::{
    IdKind, ManualCriterion, PlanDefect, ValidatedPlan, ValidationReport, check_ids, validate,
};

/// Where a project's plans live, relative to the repository root — §3.1.
pub const PLANS_DIR: &str = ".conductor/plans";

/// The path of one plan version, relative to the repository root.
///
/// `.conductor/plans/v3/plan.yaml`. A function rather than a format string at
/// each call site, so that the one place §3.1's layout is encoded is the one
/// place it would have to change.
pub fn plan_path(version: u32) -> String {
    format!("{PLANS_DIR}/v{version}/plan.yaml")
}

/// The approval sidecar for one plan version — §3.1's `APPROVED` file.
///
/// It carries *"plan content hash · approver · timestamp · policy hash"*. This
/// slice's core neither writes nor reads it: §3.3 makes approval a three-control
/// problem (the file, the store, and the rule that a disagreement halts
/// execution rather than resyncing), and one of those three is a store.
pub fn approved_path(version: u32) -> String {
    format!("{PLANS_DIR}/v{version}/APPROVED")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_paths_are_the_layout_section_3_1_draws() {
        assert_eq!(plan_path(3), ".conductor/plans/v3/plan.yaml");
        assert_eq!(approved_path(3), ".conductor/plans/v3/APPROVED");
    }
}
