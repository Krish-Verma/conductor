//! The `task` row — Part 5.1's table, §5.2's machine.
//!
//! Every state write goes through [`conductor_core::TaskState::transition_to`].
//! That is the point of this module existing rather than the call sites writing
//! `UPDATE task SET state = …` directly: §5.2's "**Invalid:** `RUNNING →
//! COMPLETE`" has to be refused by whatever actually writes the column, and a
//! rule enforced at each call site is a rule that is missing from the next one.
//!
//! **Unfenced, deliberately.** A `task` is not a run: it has no lease, no epoch
//! and no owner, because §4.7 gives leases to runs ("a `run` in a claimable
//! state *is* the job") and the unique partial index
//! `ix_run_one_active_per_task` already allows only one active run per task. The
//! run's fence is therefore what serialises task-state writes in practice, and
//! adding a second, weaker exclusion mechanism here would be the "two mechanisms
//! for one problem" §5.2 rejects elsewhere.

use conductor_core::{EventKind, RunId, TaskId, TaskState};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};
use crate::tx::with_immediate;

/// One `task` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskRow {
    /// `task.id`.
    pub id: TaskId,
    /// `task.plan_version_id`.
    pub plan_version_id: String,
    /// `task.slice_id`.
    pub slice_id: String,
    /// `task.state`.
    pub state: TaskState,
    /// `task.scope_globs`, decoded from JSON.
    pub scope_globs: Vec<String>,
    /// `task.verification_profile`.
    pub verification_profile: String,
    /// `task.attempt_budget`.
    pub attempt_budget: i64,
    /// `task.created_at`.
    pub created_at: i64,
}

/// What creating a task needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    /// `task.id`.
    pub id: TaskId,
    /// The plan version the task belongs to. S11 owns real plan versions.
    pub plan_version_id: String,
    /// `task.slice_id`.
    pub slice_id: String,
    /// Scope globs (§4.8).
    pub scope_globs: Vec<String>,
    /// The verification profile's path.
    pub verification_profile: String,
    /// How many attempts the task gets.
    pub attempt_budget: i64,
}

/// What creating a run needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    /// `run.id`.
    pub id: RunId,
    /// The task this run executes.
    pub task_id: TaskId,
    /// A `policy_snapshot.hash` that already exists.
    pub policy_hash: String,
    /// The commit the workspace is cloned from.
    pub base_commit: String,
    /// §4.1's `conductor/<task-id>/<run-id>`.
    pub run_branch: String,
    /// The branch the run branch will be fetched into (§4.1's "target ref").
    pub target_branch: String,
}

/// Create a task in `PENDING`.
pub fn create_task(conn: &mut Connection, task: &NewTask, now_ms: i64) -> StoreResult<TaskRow> {
    let globs = serde_json::to_string(&task.scope_globs)?;
    with_immediate(conn, |tx| {
        // Plain INSERT, never `OR IGNORE`: a second task claiming an existing id
        // is a mistake somewhere upstream, and silently reusing the first one
        // would give two pieces of work a single identity.
        tx.execute(
            "INSERT INTO task
               (id, plan_version_id, slice_id, state, scope_globs,
                verification_profile, attempt_budget, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                task.id.as_str(),
                task.plan_version_id,
                task.slice_id,
                TaskState::Pending.as_str(),
                globs,
                task.verification_profile,
                task.attempt_budget,
                now_ms,
            ],
        )?;
        Ok(())
    })?;
    task_row(conn, &task.id)?.ok_or_else(|| StoreError::Domain("the task vanished".to_string()))
}

/// Create a run in `READY` for an existing task.
pub fn create_run(conn: &mut Connection, run: &NewRun, now_ms: i64) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        tx.execute(
            "INSERT INTO run
               (id, task_id, policy_hash, workspace_id, base_commit, run_branch,
                target_branch, state, priority, lease_owner, lease_expires_at,
                lease_epoch, created_at)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, 'READY', 100, NULL, NULL, 0, ?7)",
            params![
                run.id.as_str(),
                run.task_id.as_str(),
                run.policy_hash,
                run.base_commit,
                run.run_branch,
                run.target_branch,
                now_ms,
            ],
        )?;
        crate::lease::append_event(
            tx,
            &run.id,
            EventKind::RunStateChanged,
            "{\"to\":\"READY\",\"detail\":\"run created\"}",
            now_ms,
        )?;
        Ok(())
    })
}

/// One task by id.
pub fn task_row(conn: &Connection, id: &TaskId) -> StoreResult<Option<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_version_id, slice_id, state, scope_globs,
                verification_profile, attempt_budget, created_at
           FROM task WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(Some(to_task_row(row)?)),
        None => Ok(None),
    }
}

/// Every task, optionally filtered by state, ordered by id.
///
/// Ordered by id rather than by creation time so that `--json` output is stable
/// between runs and therefore diffable.
pub fn tasks(conn: &Connection, state: Option<TaskState>) -> StoreResult<Vec<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_version_id, slice_id, state, scope_globs,
                verification_profile, attempt_budget, created_at
           FROM task
          WHERE (?1 IS NULL OR state = ?1)
          ORDER BY id",
    )?;
    let filter = state.map(|s| s.as_str());
    let mut rows = stmt.query(params![filter])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(to_task_row(row)?);
    }
    Ok(out)
}

/// Move a task, refusing anything §5.2 does not draw.
///
/// The read and the write are in one `BEGIN IMMEDIATE` transaction, so the state
/// the legality check saw is the state the update writes over.
///
/// **No timestamp parameter.** Part 5.1's `task` table records only
/// `created_at`; a state change leaves its trace on the run, whose `event` rows
/// carry the time. Taking a `now_ms` here and dropping it would be an API that
/// lies about what it records.
pub fn set_task_state(conn: &mut Connection, id: &TaskId, to: TaskState) -> StoreResult<TaskState> {
    with_immediate(conn, |tx| {
        let current: String = tx
            .query_row(
                "SELECT state FROM task WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .map_err(|_| StoreError::Domain(format!("no task {id}")))?;
        let from = current.parse::<TaskState>()?;
        from.transition_to(to)
            .map_err(|e| StoreError::IllegalTaskTransition(e.to_string()))?;
        tx.execute(
            "UPDATE task SET state = ?2 WHERE id = ?1",
            params![id.as_str(), to.as_str()],
        )?;
        Ok(to)
    })
}

fn to_task_row(row: &rusqlite::Row<'_>) -> StoreResult<TaskRow> {
    let id: String = row.get(0)?;
    let state: String = row.get(3)?;
    let globs: String = row.get(4)?;
    Ok(TaskRow {
        id: TaskId::new(id)?,
        plan_version_id: row.get(1)?,
        slice_id: row.get(2)?,
        state: state.parse::<TaskState>()?,
        scope_globs: serde_json::from_str(&globs)?,
        verification_profile: row.get(5)?,
        attempt_budget: row.get(6)?,
        created_at: row.get(7)?,
    })
}
