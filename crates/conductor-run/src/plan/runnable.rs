//! Which plan version authorizes a task to run — master plan §3.3, §4.3, §5.2
//! and acceptance row 21.
//!
//! # The question this module exists to answer
//!
//! §7.1 calls `conductor task run <task-id>` *"the core verb"*, and every other
//! part of the design assumes the task it claims came from somewhere
//! authoritative: §4.3's approval gate, §4.2's per-task `execution_requirements`,
//! §4.5's acceptance criteria, §5.2's dependency edge, §3.4's `Conductor-Plan`
//! trailer and row 21's version pinning all read facts that only a **plan** can
//! supply.
//!
//! S12 found that nothing on the product path asked this question. `task run`
//! read the S5-era `.conductor/task.yaml` and wrote a `plan_version` row at
//! `version 0`, `state DRAFT`, whose only purpose was to satisfy a foreign key.
//! Everything above therefore compared against `NULL` on every real run. This
//! module is the missing question, asked once, in one place, so that the CLI
//! spends its own code on exit codes and messages rather than on the rule.
//!
//! # Why the document is not re-validated here
//!
//! §3.7's refusals ran once, at [`ledger::register_plan_version`], and
//! [`ledger::approve`] only moves a version that passed them. What can change
//! afterwards is the *document*, and [`ledger::verify_approval`] is what notices:
//! it re-hashes the file and compares against the hash the store recorded at
//! approval. A document that still hashes the same is still the validated one, so
//! re-running §3.7 would only be able to reach a different answer if the
//! validator itself changed — and that is a Conductor upgrade, not a plan edit.
//!
//! This is also §5.2's restart clause becoming reachable: *"re-hash on load; a
//! mismatch on an `APPROVED` plan is a hard error, cleared by re-running
//! `conductor plan approve <version>`"*. Before this module, no product path
//! loaded an approved plan, so the clause described a check nothing performed.
//!
//! # Row 21 is the one case where a non-`APPROVED` version still runs
//!
//! > | 21 | Plan revision mid-flight | approve v4 during a v3 run | run keeps
//! > `plan_version=3` | finish under v3; new tasks under v4 |
//!
//! Approving v4 marks v3 `SUPERSEDED` (§5.2: *"by a later `APPROVED`"*), and
//! [`materialize`](super::materialize) deliberately **carries** a task with a
//! non-terminal run rather than retiring it. So a task pinned to a superseded
//! version and holding an active run must still be allowed to finish — that is
//! literally what the row asks for. It is the only exception, it is conditioned
//! on the active run rather than on the state alone, and no receipt is checked
//! for it: a superseded version's `APPROVED` sidecar may legitimately be gone,
//! and the run was pinned when the version *was* authoritative.

use std::path::{Path, PathBuf};

use conductor_core::{PlanVersionId, PlanVersionState, ProjectId, RunId, TaskId, TaskState};
use conductor_store::{PlanVersionRow, ProjectRow, Store, TaskRow};

use super::model::{self, Plan};
use super::{ledger, project};
use ledger::{Approval, LedgerError};

/// Why a task may not run.
#[derive(Debug, thiserror::Error)]
pub enum RunnableError {
    /// `.conductor/project.yaml` could not be read as a project (§3.1).
    #[error(transparent)]
    Project(#[from] project::ProjectError),
    /// The store could not be read.
    #[error(transparent)]
    Store(#[from] conductor_store::StoreError),
    /// The plan document could not be parsed.
    #[error(transparent)]
    Plan(#[from] model::PlanError),
    /// The ledger refused — including §5.2's restart clause, when the document
    /// no longer hashes to what was approved.
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    /// A path could not be resolved.
    #[error("cannot resolve {}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// An id in the config or the store is not a well-formed Conductor id.
    #[error("{what} {value:?} is not a usable id: {detail}")]
    MalformedId {
        /// Which id.
        what: &'static str,
        /// Its value.
        value: String,
        /// The id type's own complaint.
        detail: String,
    },
    /// Nothing in the store knows this project.
    ///
    /// §3.2 makes `.conductor/` authoritative and the store disposable, so this
    /// is not "the project does not exist" — it is "this machine has not learned
    /// about it yet", which has two cures and the message names both.
    #[error(
        "project {project} is not registered in this store, so there is no \
         approved plan to run a task from; approve one with `conductor plan \
         approve <version>`, or rebuild project truth from .conductor/ with \
         `conductor recover`"
    )]
    UnregisteredProject {
        /// The id `.conductor/project.yaml` declares.
        project: ProjectId,
    },
    /// The repository offered is not the one the project is registered at.
    ///
    /// §3.3's control 2 — *"Conductor reads plan approval **only** from the
    /// registered repository's working tree, never from a run branch"* — applied
    /// to the verb that starts runs. A run clone is a second checkout declaring
    /// the same project id, and without this it could present its own edited
    /// plan as the project's.
    #[error(
        "project {project} is registered at {registered} and this run was \
         offered {offered}; §3.3 reads project truth only from the registered \
         working tree, so a second checkout is refused rather than followed"
    )]
    ForeignTree {
        /// The project.
        project: ProjectId,
        /// Where it is registered.
        registered: String,
        /// What was offered.
        offered: String,
    },
    /// No task row exists, so no approved plan has ever declared this task.
    ///
    /// The plan versions and their states travel with the refusal because they
    /// are the answer to the operator's next question. "No such task" against a
    /// project whose only plan version is `VALIDATED` means *approve it*, and an
    /// error that omits the state sends the reader to look for a typo instead.
    #[error(
        "no task {task} exists; §5.2 materializes a plan's task list when a \
         human approves the version, and this project's plan versions are: \
         {versions}"
    )]
    NoSuchTask {
        /// The id that was asked for.
        task: TaskId,
        /// `v1 VALIDATED, v2 DRAFT`, or `(none registered)`.
        versions: String,
    },
    /// The task's plan version is not authoritative, and row 21's exception does
    /// not apply.
    #[error(
        "task {task} belongs to plan v{version}, which is {state}; §5.2 gives \
         APPROVED only to a human at the control socket, so run `conductor plan \
         approve {version}` before running its tasks"
    )]
    NotAuthoritative {
        /// The task.
        task: TaskId,
        /// The version it belongs to.
        version: i64,
        /// The state that version is in.
        state: PlanVersionState,
    },
    /// The task's plan version belongs to another project in the same store.
    #[error(
        "task {task} belongs to plan version {plan_version}, which is not \
         {project}'s; one store holds many projects and a task is never \
         borrowed across them"
    )]
    ForeignPlanVersion {
        /// The task.
        task: TaskId,
        /// The plan version it points at.
        plan_version: String,
        /// The project asking.
        project: ProjectId,
    },
    /// The task has already finished, been cancelled, or been superseded.
    ///
    /// §5.2 makes `COMPLETE`, `CANCELLED` and `SUPERSEDED` terminal, and a
    /// terminal state has no outgoing edge. Refused **here**, before a run row
    /// exists, rather than at the transition: `active_run_for_task` returns
    /// `None` for a task whose run is terminal, so the caller would create a
    /// second `READY` run and only then fail — leaving the row behind. A refusal
    /// that has already written a row is not a refusal.
    #[error(
        "task {task} is {state}, which §5.2 makes terminal — it has no outgoing \
         edge, so there is nothing left to run. A task that must be done again \
         is a task in a new plan version, not a second run of a finished one"
    )]
    Terminal {
        /// The task.
        task: TaskId,
        /// The terminal state it is in.
        state: TaskState,
    },
    /// The approved document no longer declares the task its row came from.
    #[error(
        "task {task} has a row from plan v{version} but that document no longer \
         declares it; the row and the plan disagree, and §3.3 halts on a \
         disagreement rather than resyncing it"
    )]
    TaskNotInDocument {
        /// The task.
        task: TaskId,
        /// The version whose document was read.
        version: u32,
    },
}

/// The authority a run may be created under.
#[derive(Debug, Clone)]
pub struct Runnable {
    /// The registered project (§3.3 control 2's anchor).
    pub project: ProjectRow,
    /// The plan version the task is pinned to — row 21's `plan_version=3`.
    pub plan_version: PlanVersionRow,
    /// The task row S11's materializer wrote.
    pub task: TaskRow,
    /// The document that version was approved as.
    pub plan: Plan,
    /// The receipt §3.3 control 3 checked, or `None` for row 21's carried task.
    pub approval: Option<Approval>,
    /// A non-terminal run already holding this task, if any.
    pub active_run: Option<RunId>,
}

impl Runnable {
    /// The task exactly as the approved document declares it.
    ///
    /// Read from the document rather than reassembled from the row: the row
    /// holds the four columns the gates need and not the objective, the
    /// rationale or the milestone, and a second copy of those would be a second
    /// thing to keep in agreement.
    pub fn declared(&self) -> Result<&model::Task, RunnableError> {
        self.plan
            .tasks()
            .find(|task| task.id.trim() == self.task.id.as_str())
            .ok_or_else(|| RunnableError::TaskNotInDocument {
                task: self.task.id.clone(),
                version: self.version(),
            })
    }

    /// The plan version number.
    pub fn version(&self) -> u32 {
        u32::try_from(self.plan_version.version).unwrap_or(0)
    }

    /// Where the verification profile the task names lives.
    ///
    /// §4.5's clarification 3 left this open — *"§5.1 makes
    /// `verification_profile` a per-task path while this section names a single
    /// `verification.yaml`"* — and S12 settles it as a **path** relative to the
    /// repository root, because `.conductor/verification.yaml` holds one profile
    /// and there is no second one for a name to select between. A blank value
    /// falls back to §3.1's location rather than to the repository root, which
    /// is what an empty path would otherwise mean.
    pub fn profile_path(&self) -> PathBuf {
        let root = Path::new(&self.project.root_path);
        if self.task.verification_profile.trim().is_empty() {
            root.join(super::VERIFICATION_CONFIG_PATH)
        } else {
            root.join(self.task.verification_profile.trim())
        }
    }
}

/// Resolve the plan version that authorizes `task_id`, or say why none does.
///
/// `repo_root` is the tree the caller is in. It is compared against the
/// registered `project.root_path` rather than trusted, for §3.3 control 2's
/// reason.
pub fn resolve(
    store: &Store,
    repo_root: &Path,
    task_id: &TaskId,
) -> Result<Runnable, RunnableError> {
    let offered = repo_root
        .canonicalize()
        .map_err(|source| RunnableError::Io {
            path: repo_root.to_path_buf(),
            source,
        })?;
    let config = project::load(&offered)?;
    let project_id =
        ProjectId::new(config.id.clone()).map_err(|err| RunnableError::MalformedId {
            what: "the project id in .conductor/project.yaml",
            value: config.id.clone(),
            detail: err.to_string(),
        })?;

    let project =
        store
            .project(&project_id)?
            .ok_or_else(|| RunnableError::UnregisteredProject {
                project: project_id.clone(),
            })?;
    if Path::new(&project.root_path) != offered.as_path() {
        return Err(RunnableError::ForeignTree {
            project: project_id,
            registered: project.root_path.clone(),
            offered: offered.display().to_string(),
        });
    }

    let versions = store.plan_versions_for_project(&project_id)?;
    let Some(task) = store.task(task_id)? else {
        return Err(RunnableError::NoSuchTask {
            task: task_id.clone(),
            versions: describe(&versions),
        });
    };

    let plan_version_id = PlanVersionId::new(task.plan_version_id.clone()).map_err(|err| {
        RunnableError::MalformedId {
            what: "task.plan_version_id",
            value: task.plan_version_id.clone(),
            detail: err.to_string(),
        }
    })?;
    let plan_version = versions
        .iter()
        .find(|row| row.id == plan_version_id)
        .cloned()
        .ok_or_else(|| RunnableError::ForeignPlanVersion {
            task: task_id.clone(),
            plan_version: task.plan_version_id.clone(),
            project: project_id.clone(),
        })?;

    // Asked before the approval gate, because it is the cheaper and more
    // specific answer: a `SUPERSEDED` task under a `SUPERSEDED` plan version
    // should say "this task was retired", not "approve the plan first".
    if task.state.is_terminal() {
        return Err(RunnableError::Terminal {
            task: task_id.clone(),
            state: task.state,
        });
    }

    let active_run = store.active_run_for_task(task_id)?;
    let version = u32::try_from(plan_version.version).unwrap_or(0);

    // The one gate. `APPROVED` is checked against its receipt; `SUPERSEDED` is
    // row 21's carried task and is conditioned on the active run, never on the
    // state alone; everything else is a refusal naming the state.
    let approval = match plan_version.state {
        PlanVersionState::Approved => Some(ledger::verify_approval(store, &project_id, version)?),
        PlanVersionState::Superseded if active_run.is_some() => None,
        state => {
            return Err(RunnableError::NotAuthoritative {
                task: task_id.clone(),
                version: plan_version.version,
                state,
            });
        }
    };

    let path = offered.join(super::plan_path(version));
    let plan = model::load(&path)?;

    let runnable = Runnable {
        project,
        plan_version,
        task,
        plan,
        approval,
        active_run,
    };
    // Asked here so a caller never holds a `Runnable` whose document has lost
    // the task: `declared()` is the only way to reach the objective, and a
    // refusal at the moment of resolution names the disagreement instead of
    // surfacing later as a packet that would not build.
    runnable.declared()?;
    Ok(runnable)
}

/// `v1 VALIDATED, v2 DRAFT` — what the operator needs to know next.
fn describe(versions: &[PlanVersionRow]) -> String {
    if versions.is_empty() {
        return "(none registered)".to_string();
    }
    let mut described: Vec<(i64, String)> = versions
        .iter()
        .map(|row| {
            (
                row.version,
                format!("v{} {}", row.version, row.state.as_str()),
            )
        })
        .collect();
    described.sort_by_key(|(version, _)| *version);
    described
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join(", ")
}
