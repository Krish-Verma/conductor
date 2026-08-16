//! Turning a validated plan into §5.1's `task` rows — master plan §3.1, §3.6,
//! §4.2, §5.1, §5.2, acceptance row 21.
//!
//! # The rows are an index, not a second plan
//!
//! §3.1 keeps git authoritative for *"what we agreed to do"*, and §5.1's own
//! schema comment says as much of the plan tables: *"index over
//! `.conductor/plans/vN/` ; git is authoritative"*. So everything this module
//! writes has to be **derivable from the document alone** — no wall clock in a
//! payload, no key order that depends on how a `HashMap` happened to iterate,
//! no field invented here that the plan does not say. Drop the database and
//! re-run [`materialize`] and the same bytes come back.
//!
//! That is why the JSON columns are built by hand rather than by
//! `serde_json::to_string` over the model types: a derive emits struct fields
//! in *declaration* order, which is a property of a Rust file rather than of
//! the plan, so renaming or reordering a field in [`super::model`] would
//! silently change every stored payload. [`CanonicalCriterion`] fixes the order
//! alphabetically and the tests assert the bytes, following the same discipline
//! [`super::hash`] applies to the content hash: *"keys sorted […] no
//! timestamps"*.
//!
//! **Element order is deliberately not sorted**, for [`super::hash`]'s reason:
//! a sequence is content. `depends_on: [A, B]` and `depends_on: [B, A]` are the
//! same set and a different document, and §3.7's forward-dependency rule makes
//! declaration order semantically constrained. Sorting would make an author's
//! reordering invisible in the index while remaining visible in the hash — two
//! representations that disagree, which is exactly what §3.6 forbids.
//!
//! # `NULL` is a different fact from `[]`
//!
//! `task.declared_actions`, `task.depends_on` and `task.acceptance_criteria`
//! are nullable, and `conductor_store`'s `SCHEMA_V8` spells out why: `NULL`
//! means *no plan document has ever been read for this row* — every task
//! created before S11 — and `'[]'` means *a plan was read and it declares
//! none*. Materialisation therefore always writes `Some(…)`, including for an
//! empty list. The alternative, leaving `NULL` when there is nothing to say,
//! would make "this task authorises no action" indistinguishable from "nobody
//! has ever asked", and the two lead to opposite decisions at an approval gate.
//!
//! `execution_requirements` is **not** in that group and is left `NULL` when
//! neither the task nor the project declares a block. §4.2's absent value
//! already has a settled meaning — `enforce::launch::requirements_for` reads it
//! as "nothing is gated" — so writing an empty block to say the same thing
//! would add a second spelling of one fact.
//!
//! # Acceptance row 21 is decided by asking §5.2, not by a list
//!
//! Row 21: *"Plan revision mid-flight · approve v4 during a v3 run · run keeps
//! `plan_version=3` · finish under v3; new tasks under v4."* Materialising vN
//! supersedes the tasks earlier versions left behind — except the ones that are
//! still being worked.
//!
//! Two independent things can make a task "still being worked", and this module
//! asks both rather than deciding either:
//!
//! * **A non-terminal run**, answered by `Store::active_run_for_task`, which
//!   derives the terminal set from `RunState::is_terminal` rather than
//!   hardcoding it.
//! * **§5.2 itself.** `conductor_core`'s task machine already refuses
//!   `RUNNING → SUPERSEDED`, and its own docs say why: *"acceptance row 21
//!   requires a run in flight under plan v3 to 'finish under v3', so a task
//!   that has started work cannot be superseded out from under itself."* The
//!   predicate here is therefore `state.transition_to(SUPERSEDED).is_ok()` —
//!   the machine's answer, not a copy of it. A copy would be a second place row
//!   21 is encoded, and the first time the two disagreed one of them would be
//!   wrong silently.
//!
//! A task the rules keep is **left completely alone**: same `plan_version_id`,
//! same state, same payloads. That is what "finish under v3" means, and it is
//! also why re-materialising cannot rewrite an in-flight task's acceptance
//! criteria out from under the run that is being judged against them.
//!
//! # The approval gate lives here, and only here
//!
//! §4.3's table gives a plan approval exactly one thing to authorize: *"a plan
//! version becoming authoritative"*. This function is that moment — it is where
//! a document stops being a file and becomes rows a run can be claimed from —
//! so an unapproved plan version is refused here
//! ([`MaterializeError::NotApproved`]) rather than at launch.
//!
//! **One gate, at the moment the document becomes work, rather than two that
//! can drift.** A second check at launch would have to re-derive which plan
//! version a task belongs to and re-read its state, and the first time the two
//! disagreed the weaker one would win silently. It also would not help: by then
//! the row already exists, is already claimable, and acceptance row 21's
//! supersession has already run against it. Gating at creation means an
//! unapproved plan never produces a claimable task at all.
//!
//! §4.2's execution-containment gate is a different question asked at a
//! different time — measured host capability against a task's requirement
//! vector — and it will never ask whether a human approved the plan.
//!
//! # §3.6's stable ids decide what "the same task" is
//!
//! §3.6: *"assigned once, never reused. A revision that preserves a task's
//! meaning preserves its ID; a task whose meaning changes gets a new ID and the
//! old is `SUPERSEDED`."* So an id vN re-declares is **the row that already
//! exists**, and materialisation neither re-creates it (which would collide on
//! the primary key) nor supersedes it (which would retire a task the current
//! plan still asks for). Nothing here diffs prose to second-guess that: the
//! author signals a change of meaning by allocating a new id, and a materializer
//! that tried to detect it from `objective:` would be inventing an authority
//! §3.6 gives to the human.
//!
//! # What this module deliberately does not do
//!
//! * **Store `forbidden_globs`.** §5.1's `task` table has exactly one glob
//!   column, so only the resolved *allow* list reaches a row. `.conductor/**` —
//!   §6.5's example entry — is already refused unconditionally at
//!   reconciliation under §3.3, so the plan's forbid list is documentation for
//!   the reader rather than the enforcement, and adding a column for it is not
//!   this slice's to add. [`resolved_scope`] still resolves it, so there is one
//!   answer to "what is this task's scope?" when a column does appear.
//! * **Move an existing row to a later plan version.** There is no such write
//!   in `conductor_store`, and row 21 does not ask for one: it asks that new
//!   tasks land under v4 and in-flight ones stay on v3.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use conductor_core::{PlanVersionId, PlanVersionState, ProjectId, RunId, TaskId, TaskState};
use conductor_store::{NewTask, PlanVersionRow, ProjectRow, Store, TaskRow};
use serde::Serialize;

use super::ledger::plan_version_id;
use super::model::{AcceptanceCriterion, Slice, Task, TaskScope};
use super::project::{self, ProjectError};
use super::validate::ValidatedPlan;

/// Anything that stops a plan from becoming rows.
///
/// Every variant is a refusal, and every refusal happens **before the first
/// write** — see [`materialize`]'s ordering. A materialisation that failed
/// halfway would leave a task set that is neither the old plan nor the new one,
/// and no reader could tell which.
#[derive(Debug, thiserror::Error)]
pub enum MaterializeError {
    /// `.conductor/project.yaml` could not be read from the registered tree.
    #[error(transparent)]
    Project(#[from] ProjectError),
    /// The store said no.
    #[error(transparent)]
    Store(#[from] conductor_store::StoreError),
    /// An id read out of the plan is blank.
    ///
    /// §3.7 already refuses a blank task id, so a [`ValidatedPlan`] cannot
    /// carry one and this is unreachable through [`materialize`]. Kept because
    /// the alternative at the call site is `expect`, and an id silently
    /// defaulted is a row addressed to nothing.
    #[error(transparent)]
    Id(#[from] conductor_core::IdError),
    /// A payload could not be encoded.
    ///
    /// **Unreachable by construction, and kept anyway.** Everything encoded
    /// here is strings, booleans and sequences of them; `serde_json` fails on
    /// non-string map keys and non-finite floats, neither of which a plan can
    /// produce. It exists so that a field added later cannot turn a
    /// serialization failure into a silently truncated column.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Nothing has been registered under this project id.
    #[error(
        "no project {id} is registered; materialisation reads §4.2's project-level \
         execution_requirements out of the registered repository's working tree \
         (§3.3 control 2), and there is no registered tree to read"
    )]
    UnknownProject {
        /// The id asked for.
        id: ProjectId,
    },
    /// No `plan_version` row exists for this version.
    #[error(
        "no plan version {id} is registered for project {project}; §5.1 makes \
         task.plan_version_id a foreign key into plan_version, so the ledger has \
         to record the version before its tasks exist — run plan registration first"
    )]
    UnknownPlanVersion {
        /// The plan version id that was derived.
        id: PlanVersionId,
        /// The project it would belong to.
        project: ProjectId,
    },
    /// No human has approved this plan version, so it is not authoritative.
    ///
    /// §4.3's table gives a plan approval exactly one thing to authorize — *"a
    /// plan version becoming authoritative"* — and this is the moment it
    /// becomes so. Without this refusal `plan approve` would authorize nothing
    /// observable, because a `VALIDATED` plan would turn into claimable work
    /// just as readily as an `APPROVED` one.
    #[error(
        "plan version {id} is {state}, not APPROVED; §4.3 makes a plan approval \
         the authority for \"a plan version becoming authoritative\" and \
         materialisation is that moment, so its tasks may not become claimable \
         work first — run `conductor plan approve {id}`"
    )]
    NotApproved {
        /// The plan version.
        id: PlanVersionId,
        /// Where §5.2's machine actually has it.
        state: PlanVersionState,
    },
    /// The caller named one version and handed over another version's document.
    #[error(
        "asked to materialise {plan_version} but the document declares version \
         {declared}, not {requested}; §5.1's task.plan_version_id, supersession \
         and §3.4's `Conductor-Plan: v{requested}@…` trailer each need one answer \
         to \"which version is this?\""
    )]
    VersionMismatch {
        /// The plan version being materialised.
        plan_version: PlanVersionId,
        /// What the document says.
        declared: u32,
        /// What the caller asked for.
        requested: u32,
    },
    /// A task id this plan declares already names a row belonging to a
    /// different project.
    ///
    /// §5.1 makes `task.id` a primary key over the whole table, so two projects
    /// in one store can reach for the same id. Adopting the row would re-point
    /// somebody else's work at this plan; superseding it would retire work this
    /// project has no authority over. Neither is recoverable, so this refuses.
    #[error(
        "task {task} is already registered under plan version {plan_version}, \
         which does not belong to project {project}; a task id is assigned once \
         and never reused (§3.6), so this is two plans claiming one identity — \
         rename the task in whichever plan is newer"
    )]
    ForeignTask {
        /// The contested id.
        task: TaskId,
        /// The plan version that already holds it.
        plan_version: String,
        /// The project that was materialising.
        project: ProjectId,
    },
}

/// What one materialisation did — and, as importantly, what it did **not** do.
///
/// Three lists rather than one count, because the three outcomes answer
/// different questions and a caller that could not tell them apart could not
/// report acceptance row 21 at all. "Nothing was superseded" and "everything
/// was carried" are the same total and opposite facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Materialization {
    /// The plan version the new rows belong to.
    pub plan_version_id: PlanVersionId,
    /// One row per plan task that did not already have one, in the plan's
    /// declaration order.
    pub created: Vec<TaskRow>,
    /// Rows this materialisation deliberately left untouched, and where they
    /// stayed. Row 21's evidence.
    pub carried: Vec<CarriedTask>,
    /// Rows from earlier versions moved to `SUPERSEDED`, by id.
    pub superseded: Vec<TaskId>,
}

/// A task row a materialisation left exactly as it found it.
///
/// Carries *why* as data rather than as a comment: [`active_run`] is `Some`
/// when acceptance row 21's clause applied, and `None` when the row was kept
/// either because the current plan still declares it (§3.6's stable id) or
/// because §5.2 has no `→ SUPERSEDED` edge out of [`state`]. All three are
/// "left alone", and an operator asking "why is this still on v1?" needs to be
/// able to tell them apart.
///
/// [`active_run`]: CarriedTask::active_run
/// [`state`]: CarriedTask::state
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedTask {
    /// The task.
    pub id: TaskId,
    /// The plan version it stays on — row 21's *"finish under v3"*.
    pub plan_version_id: String,
    /// The state it stays in.
    pub state: TaskState,
    /// The non-terminal run holding it, when that is the reason.
    pub active_run: Option<RunId>,
}

/// Write §5.1's `task` rows for one validated plan version.
///
/// # Why a [`ValidatedPlan`] and not a [`Plan`](super::model::Plan)
///
/// §3.7's refusals are what make the rows meaningful: without them a task can
/// depend on an id that does not exist, name a check `verification.yaml` never
/// defines, or carry an acceptance criterion bound to nothing — the last being
/// *"the mechanism by which a task reaches `COMPLETE` on an agent's word"*.
/// Materialising such a plan would put all of that in the store as fact. Taking
/// [`ValidatedPlan`], which only [`super::validate`] mints, makes "has this been
/// validated?" a question the type system answers rather than one this function
/// has to ask and a caller has to be trusted about.
///
/// The document is the one [`super::ledger::register_plan_version`] hands back
/// with the row, so it is the document that was hashed into
/// `plan_version.content_hash`. Re-reading the file here would open exactly the
/// window §3.3 exists to close.
///
/// # The order of operations, and what a crash leaves behind
///
/// 1. **Everything that can refuse, refuses first** — the version cross-check,
///    the project, the plan version row, §4.3's approval gate,
///    `.conductor/project.yaml`, and the per-task classification including
///    [`MaterializeError::ForeignTask`]. No row has been written at this point,
///    so a refusal leaves the store exactly as it was. The approval gate sits
///    ahead of every filesystem read, so an unapproved plan costs nothing and
///    touches nothing.
/// 2. **Supersession is written before creation.** Deliberately, and it is the
///    fail-closed order: a crash in between leaves the old tasks retired and
///    the new ones missing, which stalls. The other order would leave both the
///    old and the new task set live at once, and the old ones are runnable —
///    a revision that was supposed to replace work would instead double it.
///    Re-running completes either way, because this function is idempotent.
///
/// `now_ms` reaches `task.created_at` and nothing else. It is deliberately not
/// part of any payload: see this module's docs on why the rows must be
/// rebuildable.
pub fn materialize(
    store: &mut Store,
    project_id: &ProjectId,
    version: u32,
    plan: &ValidatedPlan,
    now_ms: i64,
) -> Result<Materialization, MaterializeError> {
    let id = plan_version_id(project_id, version);
    if plan.plan().version != version {
        return Err(MaterializeError::VersionMismatch {
            plan_version: id,
            declared: plan.plan().version,
            requested: version,
        });
    }
    let project = require_project(store, project_id)?;
    let plan_version = require_plan_version(store, project_id, &id)?;
    // §4.3's authority, checked before anything is read from disk: an
    // unapproved plan is not a document Conductor turns into work.
    if plan_version.state != PlanVersionState::Approved {
        return Err(MaterializeError::NotApproved {
            id,
            state: plan_version.state,
        });
    }
    // §3.3 control 2's shape again: the tree is the registered one, found
    // through the `project` row, and there is no parameter that could offer a
    // run workspace instead.
    let config = project::load(Path::new(&project.root_path))?;

    // Every plan version this project has, so that a task row can be told from
    // one belonging to a project that merely shares the store.
    let mine: BTreeMap<String, i64> = store
        .plan_versions_for_project(project_id)?
        .into_iter()
        .map(|row| (row.id.as_str().to_string(), row.version))
        .collect();

    // -- pass 1: classify what the document declares -------------------------
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let mut to_create: Vec<(TaskId, &Slice, &Task)> = Vec::new();
    let mut carried: Vec<CarriedTask> = Vec::new();
    for (slice, task) in tasks_with_slices(plan) {
        let task_id = TaskId::new(task.id.trim())?;
        declared.insert(task_id.as_str().to_string());
        let Some(row) = store.task(&task_id)? else {
            to_create.push((task_id, slice, task));
            continue;
        };
        if !mine.contains_key(&row.plan_version_id) {
            return Err(MaterializeError::ForeignTask {
                task: task_id,
                plan_version: row.plan_version_id,
                project: project_id.clone(),
            });
        }
        carried.push(CarriedTask {
            active_run: store.active_run_for_task(&task_id)?,
            id: task_id,
            plan_version_id: row.plan_version_id,
            state: row.state,
        });
    }

    // -- pass 2: decide what earlier versions leave behind -------------------
    let mut to_supersede: Vec<TaskId> = Vec::new();
    for row in store.tasks(None)? {
        // A task of some other project in the same store is not this plan's to
        // retire; one this plan still declares was handled above.
        let Some(row_version) = mine.get(&row.plan_version_id) else {
            continue;
        };
        if *row_version >= i64::from(version) || declared.contains(row.id.as_str()) {
            continue;
        }
        let active_run = store.active_run_for_task(&row.id)?;
        // Row 21, then §5.2. Both are asked rather than assumed — see the
        // module docs.
        if active_run.is_some() || row.state.transition_to(TaskState::Superseded).is_err() {
            carried.push(CarriedTask {
                id: row.id,
                plan_version_id: row.plan_version_id,
                state: row.state,
                active_run,
            });
            continue;
        }
        to_supersede.push(row.id);
    }

    // -- pass 3: write, retirements first ------------------------------------
    for task_id in &to_supersede {
        store.set_task_state(task_id, TaskState::Superseded)?;
    }

    let mut created = Vec::new();
    for (task_id, slice, task) in to_create {
        let row = store.create_task(
            &NewTask {
                id: task_id.clone(),
                plan_version_id: plan_version.id.as_str().to_string(),
                slice_id: slice.id.clone(),
                scope_globs: resolved_scope(&task.scope, &config.scope_defaults).allowed_globs,
                verification_profile: task.verification_profile.clone(),
                attempt_budget: task.attempt_budget,
            },
            now_ms,
        )?;
        store.set_declared_actions(&task_id, Some(&canonical_strings(&task.actions)?))?;
        store.set_depends_on(&task_id, Some(&canonical_strings(&task.depends_on)?))?;
        store.set_acceptance_criteria(
            &task_id,
            Some(&canonical_criteria(&task.acceptance_criteria)?),
        )?;
        // §4.2's *"or per-task override"*: the task's own block wins, the
        // project's is the fallback, and neither is `None` — which §4.2 already
        // reads as "nothing is gated".
        store.set_execution_requirements(
            &task_id,
            task.execution_requirements
                .as_deref()
                .or(config.execution_requirements.as_deref()),
        )?;
        created.push(row);
    }

    Ok(Materialization {
        plan_version_id: plan_version.id,
        created,
        carried,
        superseded: to_supersede,
    })
}

/// Every task with the slice that contains it, in declaration order.
///
/// Walked structurally rather than through `Plan::tasks()` plus
/// `Plan::slice_of()`, because the second returns an `Option` that this walk
/// makes unrepresentable: a task reached through its slice always has one, and
/// §5.1's `slice_id` is `NOT NULL`, so the alternative would be an `expect` on
/// a case the data model cannot produce.
fn tasks_with_slices(plan: &ValidatedPlan) -> impl Iterator<Item = (&Slice, &Task)> {
    plan.plan()
        .milestones
        .iter()
        .flat_map(|milestone| milestone.slices.iter())
        .flat_map(|slice| slice.tasks.iter().map(move |task| (slice, task)))
}

/// The scope a task actually runs under — §3.1's *"scope defaults"* applied.
///
/// # Per field, not wholesale
///
/// Each list falls back independently: a task that declares `allowed_globs` and
/// no `forbidden_globs` keeps its own allow list **and** inherits the project's
/// forbid list. The wholesale reading — the task's whole `scope:` block
/// replaces the project's the moment it declares any part of it — is more
/// predictable, and it is the wrong trade here: §6.5's own example puts
/// `.conductor/**` in the project's `forbidden_globs`, so wholesale would mean
/// that naming a single allowed glob silently discards the project's forbid
/// list. An operator who writes a project-level forbid rule means it for every
/// task, not only for the tasks that declare no scope at all.
///
/// # An empty list means "declares none"
///
/// [`TaskScope`]'s fields are `Vec<String>` with `#[serde(default)]`, so an
/// absent key and an explicitly empty one are the same value and cannot be told
/// apart without changing the plan model. That is recorded rather than worked
/// around: an empty allow list is not a way to say "deny everything" — it is
/// how a task says nothing, and §3.1 is what says what nothing means.
///
/// # Only the allow list reaches a column
///
/// §5.1's `task` table has exactly one glob column (`scope_globs`, the allow
/// list). The forbid half is resolved here and tested here so that there is one
/// answer to "what is this task's scope?", but it has nowhere durable to go
/// yet; §3.3 already refuses `.conductor/**` at reconciliation unconditionally,
/// so nothing is weaker for it in the meantime.
fn resolved_scope(task: &TaskScope, defaults: &TaskScope) -> TaskScope {
    fn inherit(declared: &[String], default: &[String]) -> Vec<String> {
        if declared.is_empty() {
            default.to_vec()
        } else {
            declared.to_vec()
        }
    }
    TaskScope {
        allowed_globs: inherit(&task.allowed_globs, &defaults.allowed_globs),
        forbidden_globs: inherit(&task.forbidden_globs, &defaults.forbidden_globs),
    }
}

/// One acceptance criterion, in the column's canonical shape.
///
/// **The field order is the encoding.** `serde_json` emits struct fields in
/// declaration order, so declaring them alphabetically — `id`, `manual`,
/// `statement`, `verified_by` — is what makes the payload's keys sorted, and
/// the round-trip test asserts the exact bytes so that a future reorder is
/// caught rather than absorbed.
///
/// A separate type from [`AcceptanceCriterion`] for exactly that reason: the
/// model's field order is chosen for a human reading the struct, and coupling
/// the stored bytes to it would mean a cosmetic edit in [`super::model`]
/// rewrote every row in every database.
///
/// Every field is written, including the defaults. A criterion omitted because
/// it happened to be `false` would make `manual: false` and "an older Conductor
/// that did not know about `manual`" the same payload.
#[derive(Debug, Serialize)]
struct CanonicalCriterion<'a> {
    id: &'a str,
    manual: bool,
    statement: &'a str,
    verified_by: &'a [String],
}

/// A string list as the column holds it — declaration order, never sorted.
fn canonical_strings(values: &[String]) -> Result<String, MaterializeError> {
    Ok(serde_json::to_string(values)?)
}

/// The acceptance criteria as the column holds them.
fn canonical_criteria(criteria: &[AcceptanceCriterion]) -> Result<String, MaterializeError> {
    let canonical: Vec<CanonicalCriterion<'_>> = criteria
        .iter()
        .map(|criterion| CanonicalCriterion {
            id: &criterion.id,
            manual: criterion.manual,
            statement: &criterion.statement,
            verified_by: &criterion.verified_by,
        })
        .collect();
    Ok(serde_json::to_string(&canonical)?)
}

/// The registered project, or a refusal naming the id.
fn require_project(store: &Store, id: &ProjectId) -> Result<ProjectRow, MaterializeError> {
    store
        .project(id)?
        .ok_or_else(|| MaterializeError::UnknownProject { id: id.clone() })
}

/// The plan version's row, or a refusal naming it.
fn require_plan_version(
    store: &Store,
    project: &ProjectId,
    id: &PlanVersionId,
) -> Result<PlanVersionRow, MaterializeError> {
    store
        .plan_version(id)?
        .ok_or_else(|| MaterializeError::UnknownPlanVersion {
            id: id.clone(),
            project: project.clone(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_criterions_canonical_payload_has_its_keys_in_sorted_order() {
        let criteria = vec![AcceptanceCriterion {
            id: "AC-1".to_string(),
            statement: "It holds.".to_string(),
            verified_by: vec!["unit-tests".to_string()],
            manual: false,
        }];
        assert_eq!(
            canonical_criteria(&criteria).expect("encode"),
            r#"[{"id":"AC-1","manual":false,"statement":"It holds.","verified_by":["unit-tests"]}]"#
        );
    }

    #[test]
    fn a_declared_nothing_encodes_as_an_empty_list_and_not_as_nothing() {
        // Ruling 4's distinction, at the encoder: the caller writes `Some` of
        // this, so `NULL` stays available to mean "never materialized".
        assert_eq!(canonical_strings(&[]).expect("encode"), "[]");
        assert_eq!(canonical_criteria(&[]).expect("encode"), "[]");
    }

    /// The choice [`resolved_scope`] documents, asserted rather than described.
    ///
    /// The third case is the one that decides between per-field and wholesale,
    /// and it is why per-field was chosen: under wholesale, `forbidden_globs`
    /// would come back empty and the project's `.conductor/**` rule would have
    /// been discarded by a task that merely named an allow list.
    #[test]
    fn scope_defaults_are_inherited_per_field_so_an_allow_list_cannot_discard_a_forbid_list() {
        let defaults = TaskScope {
            allowed_globs: vec!["crates/**".to_string()],
            forbidden_globs: vec![".conductor/**".to_string()],
        };

        // Declares nothing: both halves come from the project.
        let inherited = resolved_scope(&TaskScope::default(), &defaults);
        assert_eq!(inherited, defaults);

        // Declares both: neither half is touched.
        let own = TaskScope {
            allowed_globs: vec!["docs/**".to_string()],
            forbidden_globs: vec!["docs/secret/**".to_string()],
        };
        assert_eq!(resolved_scope(&own, &defaults), own);

        // Declares only an allow list — the case the doc comment decides.
        let partial = TaskScope {
            allowed_globs: vec!["docs/**".to_string()],
            forbidden_globs: Vec::new(),
        };
        assert_eq!(
            resolved_scope(&partial, &defaults),
            TaskScope {
                allowed_globs: vec!["docs/**".to_string()],
                forbidden_globs: vec![".conductor/**".to_string()],
            },
            "the project's forbid list survives a task that only names an allow list"
        );
    }

    #[test]
    fn element_order_survives_encoding_because_a_sequence_is_content() {
        let forwards = vec!["T-0001".to_string(), "T-0002".to_string()];
        let backwards = vec!["T-0002".to_string(), "T-0001".to_string()];
        assert_ne!(
            canonical_strings(&forwards).expect("encode"),
            canonical_strings(&backwards).expect("encode")
        );
    }
}
