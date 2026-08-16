//! `project`, `plan_version` and `decision` — the plan ledger's own tables.
//!
//! # These tables have existed since `SCHEMA_V1`; nothing wrote them until now
//!
//! Part 5.1 shipped `project`, `plan_version` and `decision` verbatim in S1.
//! What S1 did not ship — its scope was the schema, not the ledger — was any
//! `conductor-store` function that reads or writes a row in them. Every test
//! through S10 hand-seeds `project` and `plan_version` directly
//! (`tests/common::seed_parents`) because there was no other way to get a row
//! in; `decision` has never been written at all. S11 needs real rows: `plan
//! validate` and `plan approve` move a `plan_version` through §5.2's five
//! states, a materializer writes `task` rows against it, and a decision file
//! gets a `decision` row. This module is that API.
//!
//! # `create` vs `upsert`
//!
//! `plan_version` is created with `create_plan_version` and errors on a
//! duplicate id, the same discipline `task::create_task` uses — a plan
//! version is immutable once written (S11's scope line: "repo-tracked,
//! versioned, **immutable** plans"), so a second attempt to create the same
//! one is a mistake upstream, not a resync.
//!
//! `project` and `decision`, by contrast, are `upsert_*`. A project's
//! identity is re-established every time `conductor init` (or equivalent)
//! runs against the same repository, and a decision's identity is
//! re-established every time its Markdown file is re-read — both are the
//! store's mirror of something git already owns, and re-reading the same
//! source must not fail just because the mirror already exists. `upsert_decision`
//! refreshes the content facts a re-synced file can change
//! (`content_hash`, `source_path`, `supersedes`) but never touches `status`:
//! a decision's status is a human's call, made through [`set_decision_status`],
//! and a content resync must not silently reopen or re-close one — the same
//! separation Ruling 5 draws for `plan_version`, applied to the table next to
//! it.
//!
//! # Plan and decision state writes go through a legality table
//!
//! Same discipline as `task::set_task_state`: the current state is read and
//! the new state is written inside one `BEGIN IMMEDIATE` transaction, and a
//! transition §5.2 does not draw is refused — [`StoreError::Domain`], never a
//! silent write.
//!
//! `decision`'s machine is not drawn anywhere in the master plan; §5.2 names
//! four values (`OPEN|ACCEPTED|REJECTED|SUPERSEDED`, Part 5.1's column
//! comment) and S11's scope line names three as outcomes ("decisions with
//! `ACCEPTED|REJECTED|SUPERSEDED`") but no source draws the arrows between
//! them. [`decision_status_successors`] is this task's own construction,
//! reasoned from the one property S11 states outright — "append-only
//! decisions" — the same way [`plan_version_successors`] is reasoned from
//! §5.2's diagram. See that function's doc comment for the reasoning and the
//! alternative it rejects.

use conductor_core::{PlanVersionId, PlanVersionState, ProjectId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};
use crate::tx::with_immediate;

// ---------------------------------------------------------------------------
// project
// ---------------------------------------------------------------------------

/// One `project` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectRow {
    /// `project.id` — `p-<short>`.
    pub id: ProjectId,
    /// `project.root_path`, `UNIQUE`.
    pub root_path: String,
    /// `project.repo_identity` — `blake3(first_commit ‖ normalized_origin)`.
    pub repo_identity: String,
    /// `project.default_branch`.
    pub default_branch: String,
    /// `project.config_hash`.
    pub config_hash: String,
    /// `project.created_at`.
    pub created_at: i64,
}

/// What creating or refreshing a project needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    /// `project.id`. Caller-supplied, like every other id this crate writes.
    pub id: ProjectId,
    /// `project.root_path`.
    pub root_path: String,
    /// `project.repo_identity`.
    pub repo_identity: String,
    /// `project.default_branch`.
    pub default_branch: String,
    /// `project.config_hash`.
    pub config_hash: String,
}

/// Insert a project, or refresh the facts about one already known under this
/// id.
///
/// Keyed on `id` (the primary key `plan_version.project_id` and
/// `decision.project_id` actually reference), not on `root_path`. `root_path`
/// is `NOT NULL UNIQUE` in its own right, so a second id offered at an
/// already-registered root path is refused — but named, not left to surface
/// as SQLite's own constraint violation.
///
/// # Why this check runs before the `INSERT`, rather than letting the
/// constraint answer for it
///
/// One repository has one project identity, and changing it is a decision a
/// human makes deliberately — for instance by renaming `id:` in
/// `project.yaml` and re-registering. §3.5's recovery path is exactly this
/// shape: *"re-register the project → read `.conductor/` → rebuild the task
/// list"* (Task 9's acceptance criterion). A collision discovered mid-recovery
/// is discovered at the worst possible moment, and `"UNIQUE constraint failed:
/// project.root_path"` tells the operator *that* something collided, not
/// *what* — not the root path, not which id already holds it, not which id
/// was being offered. Every other refusal in this module names its subject
/// (`set_plan_state`'s `"{from} → {to} is not a transition..."`); this one
/// now does too, as a [`StoreError::Domain`] naming all three facts.
///
/// **Not fixed by dropping the `UNIQUE` constraint or by re-pointing the
/// existing row to the new id.** Either would let a repository silently
/// change identity, which is the exact failure this refusal exists to
/// prevent — see this function's doc comment history for the record that
/// both were considered and rejected.
///
/// `created_at` is set once, at first insert, and never touched by a later
/// upsert — a project's creation time is a fact about the first time it was
/// seen, not the most recent one.
pub fn upsert_project(
    conn: &mut Connection,
    new: &NewProject,
    now_ms: i64,
) -> StoreResult<ProjectRow> {
    with_immediate(conn, |tx| {
        let holder: Option<String> = tx
            .query_row(
                "SELECT id FROM project WHERE root_path = ?1",
                params![new.root_path],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(holder) = holder
            && holder != new.id.as_str()
        {
            return Err(StoreError::Domain(format!(
                "root path {} is already registered as project {holder}; refusing to \
                 register it as {} — one repository has one project identity",
                new.root_path, new.id
            )));
        }
        tx.execute(
            "INSERT INTO project (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               root_path = excluded.root_path,
               repo_identity = excluded.repo_identity,
               default_branch = excluded.default_branch,
               config_hash = excluded.config_hash",
            params![
                new.id.as_str(),
                new.root_path,
                new.repo_identity,
                new.default_branch,
                new.config_hash,
                now_ms,
            ],
        )?;
        Ok(())
    })?;
    project(conn, &new.id)?.ok_or_else(|| StoreError::Domain("the project vanished".to_string()))
}

/// A project by its root path — what `conductor` reads on every invocation to
/// find "the project this repository is".
pub fn project_by_root(conn: &Connection, root_path: &str) -> StoreResult<Option<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, root_path, repo_identity, default_branch, config_hash, created_at
           FROM project WHERE root_path = ?1",
    )?;
    let mut rows = stmt.query(params![root_path])?;
    match rows.next()? {
        Some(row) => Ok(Some(to_project_row(row)?)),
        None => Ok(None),
    }
}

/// One project by id.
pub fn project(conn: &Connection, id: &ProjectId) -> StoreResult<Option<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, root_path, repo_identity, default_branch, config_hash, created_at
           FROM project WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(Some(to_project_row(row)?)),
        None => Ok(None),
    }
}

fn to_project_row(row: &rusqlite::Row<'_>) -> StoreResult<ProjectRow> {
    let id: String = row.get(0)?;
    Ok(ProjectRow {
        id: ProjectId::new(id)?,
        root_path: row.get(1)?,
        repo_identity: row.get(2)?,
        default_branch: row.get(3)?,
        config_hash: row.get(4)?,
        created_at: row.get(5)?,
    })
}

// ---------------------------------------------------------------------------
// plan_version
// ---------------------------------------------------------------------------

/// One `plan_version` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanVersionRow {
    /// `plan_version.id`.
    pub id: PlanVersionId,
    /// `plan_version.project_id`.
    pub project_id: ProjectId,
    /// `plan_version.version` — `N` from `.conductor/plans/vN/`.
    pub version: i64,
    /// `plan_version.content_hash` — of canonical semantic content.
    pub content_hash: String,
    /// `plan_version.state` — §5.2's five states.
    pub state: PlanVersionState,
    /// `plan_version.approved_at`. `None` until the first approval.
    pub approved_at: Option<i64>,
    /// `plan_version.approved_by`. `None` until the first approval.
    pub approved_by: Option<String>,
    /// `plan_version.source_path`.
    pub source_path: String,
}

/// What creating a plan version needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlanVersion {
    /// `plan_version.id`.
    pub id: PlanVersionId,
    /// `plan_version.project_id` — must already exist (FK is `ON`).
    pub project_id: ProjectId,
    /// `plan_version.version`.
    pub version: i64,
    /// `plan_version.content_hash`, at creation.
    pub content_hash: String,
    /// `plan_version.source_path`.
    pub source_path: String,
}

/// Create a plan version in `DRAFT` — §5.2's start state.
///
/// Plain `INSERT`, never `OR IGNORE` or an upsert: see this module's doc
/// comment for why a plan version is `create`, not `upsert`.
/// `approved_at`/`approved_by` start `NULL`; nothing has approved this
/// version yet.
pub fn create_plan_version(
    conn: &mut Connection,
    new: &NewPlanVersion,
) -> StoreResult<PlanVersionRow> {
    with_immediate(conn, |tx| {
        tx.execute(
            "INSERT INTO plan_version
               (id, project_id, version, content_hash, state, approved_at, approved_by, source_path)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, ?6)",
            params![
                new.id.as_str(),
                new.project_id.as_str(),
                new.version,
                new.content_hash,
                PlanVersionState::Draft.as_str(),
                new.source_path,
            ],
        )?;
        Ok(())
    })?;
    plan_version(conn, &new.id)?
        .ok_or_else(|| StoreError::Domain("the plan version vanished".to_string()))
}

/// One plan version by id.
pub fn plan_version(conn: &Connection, id: &PlanVersionId) -> StoreResult<Option<PlanVersionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, version, content_hash, state, approved_at, approved_by, source_path
           FROM plan_version WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(Some(to_plan_version_row(row)?)),
        None => Ok(None),
    }
}

/// Every version of a project's plan, oldest first — the order supersession
/// walks it in.
pub fn plan_versions_for_project(
    conn: &Connection,
    project_id: &ProjectId,
) -> StoreResult<Vec<PlanVersionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, version, content_hash, state, approved_at, approved_by, source_path
           FROM plan_version WHERE project_id = ?1 ORDER BY version",
    )?;
    let mut rows = stmt.query(params![project_id.as_str()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(to_plan_version_row(row)?);
    }
    Ok(out)
}

fn to_plan_version_row(row: &rusqlite::Row<'_>) -> StoreResult<PlanVersionRow> {
    let id: String = row.get(0)?;
    let project_id: String = row.get(1)?;
    let state: String = row.get(4)?;
    Ok(PlanVersionRow {
        id: PlanVersionId::new(id)?,
        project_id: ProjectId::new(project_id)?,
        version: row.get(2)?,
        content_hash: row.get(3)?,
        state: state.parse::<PlanVersionState>()?,
        approved_at: row.get(5)?,
        approved_by: row.get(6)?,
        source_path: row.get(7)?,
    })
}

/// The states a plan version may move to next — §5.2's "Plan (5 states)"
/// diagram, transcribed the way `conductor_core::TaskState::successors`
/// transcribes the task machine:
///
/// ```text
/// DRAFT ──validate──► VALIDATED ──request──► AWAITING_APPROVAL ──human──► APPROVED
///   ▲                    │                          │                        │
///   └────────────────────┴──────────────────────────┘                   SUPERSEDED
///         (validation failure or rejection returns to DRAFT)      (by a later APPROVED)
/// ```
///
/// Two things the prose says that the arrows alone would not, both encoded
/// here:
///
/// * **`DRAFT` is not its own rejection target.** "Validation failure or
///   rejection returns to `DRAFT`" describes what happens to a `VALIDATED` or
///   `AWAITING_APPROVAL` plan; a plan already in `DRAFT` has nothing to
///   reject *back into* `DRAFT`. Self-transitions are refused throughout this
///   table for the same reason `TaskState::transition_to` refuses them:
///   writing the state a row is already in reads as progress and is not.
/// * **`APPROVED → SUPERSEDED` is the only edge out of `APPROVED`.** Ruling 5
///   of this task: `APPROVED → APPROVED` (re-approval) is refused here, on
///   purpose — the door that updates an `APPROVED` row's content is
///   [`record_plan_approval_content`], which never touches `state`.
fn plan_version_successors(state: PlanVersionState) -> &'static [PlanVersionState] {
    use PlanVersionState::*;
    match state {
        Draft => &[Validated, Superseded],
        Validated => &[AwaitingApproval, Draft, Superseded],
        AwaitingApproval => &[Approved, Draft, Superseded],
        Approved => &[Superseded],
        Superseded => &[],
    }
}

/// Move a plan version, refusing anything §5.2's machine does not draw.
///
/// Same shape as `task::set_task_state`: the read and the write are in one
/// `BEGIN IMMEDIATE` transaction, so the state the legality check saw is the
/// state the update writes over. An illegal transition is
/// [`StoreError::Domain`] and the row is left exactly as it was.
pub fn set_plan_state(
    conn: &mut Connection,
    id: &PlanVersionId,
    to: PlanVersionState,
) -> StoreResult<PlanVersionState> {
    with_immediate(conn, |tx| {
        let current: String = tx
            .query_row(
                "SELECT state FROM plan_version WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .map_err(|_| StoreError::Domain(format!("no plan version {id}")))?;
        let from = current.parse::<PlanVersionState>()?;
        if !plan_version_successors(from).contains(&to) {
            return Err(StoreError::Domain(format!(
                "{from} → {to} is not a transition the plan machine has (§5.2)"
            )));
        }
        tx.execute(
            "UPDATE plan_version SET state = ?2 WHERE id = ?1",
            params![id.as_str(), to.as_str()],
        )?;
        Ok(to)
    })
}

/// `→ SUPERSEDED` — the edge every non-terminal plan state has (§5.2: "by a
/// later `APPROVED`").
///
/// A thin, named wrapper over [`set_plan_state`] rather than a second
/// legality table: one table, checked once by one function, is the whole
/// point of this module existing instead of call sites writing `UPDATE
/// plan_version SET state = …` themselves.
pub fn supersede_plan_version(
    conn: &mut Connection,
    id: &PlanVersionId,
) -> StoreResult<PlanVersionState> {
    set_plan_state(conn, id, PlanVersionState::Superseded)
}

/// Record `content_hash`, `approved_at` and `approved_by` on a plan version
/// that is `APPROVED` — Ruling 5's "a door that is not the transition table".
///
/// # Why this cannot be folded into [`set_plan_state`]
///
/// §5.2's restart clause makes two things true about a content-hash mismatch
/// on an `APPROVED` plan at once: it is *"a hard error, cleared by re-running
/// `conductor plan approve <version>` on the changed document"*, and the same
/// section's invalid-transition list forbids `APPROVED → *` except
/// `SUPERSEDED`. Both hold, because re-approval changes what the row *says*,
/// not what *state* it is in — it was `APPROVED`, the content changed, it is
/// still `APPROVED`. Routing that through `set_plan_state(id, Approved)`
/// would ask the legality table to accept an edge it correctly refuses
/// (`APPROVED → APPROVED` — [`plan_version_successors`] has no self-entries),
/// and weakening the table to admit it would also admit the transition it
/// exists to forbid.
///
/// So the content write and the state write are two functions. The **first**
/// approval calls [`set_plan_state`] to move `AWAITING_APPROVAL → APPROVED`
/// and then this function to stamp the approval fields. A **later**
/// re-approval — content changed, state already `APPROVED` — calls only this
/// function, because there is no state left to move to.
///
/// # Why it requires the row to already be `APPROVED`
///
/// Stamping approval metadata on a `DRAFT` or `VALIDATED` row would let a
/// plan claim to be approved without ever having passed through the state
/// that means it — exactly the hole this function's existence must not open.
/// An unapproved row is refused with [`StoreError::Domain`], not silently
/// written.
pub fn record_plan_approval_content(
    conn: &mut Connection,
    id: &PlanVersionId,
    content_hash: &str,
    approved_by: &str,
    now_ms: i64,
) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        let current: String = tx
            .query_row(
                "SELECT state FROM plan_version WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .map_err(|_| StoreError::Domain(format!("no plan version {id}")))?;
        let from = current.parse::<PlanVersionState>()?;
        if from != PlanVersionState::Approved {
            return Err(StoreError::Domain(format!(
                "plan version {id} is {from}, not APPROVED; approval content can only be \
                 recorded on an APPROVED row"
            )));
        }
        tx.execute(
            "UPDATE plan_version SET content_hash = ?2, approved_at = ?3, approved_by = ?4
             WHERE id = ?1",
            params![id.as_str(), content_hash, now_ms, approved_by],
        )?;
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// decision
// ---------------------------------------------------------------------------

/// `decision.status` — Part 5.1's column comment:
/// `OPEN|ACCEPTED|REJECTED|SUPERSEDED`.
///
/// Hand-written rather than promoted into `conductor_core::state`'s
/// `state_enum!` macro: that macro is `pub(crate)` to its own crate (state.rs's
/// module doc: deciding "which strings are legal in which column" is core's
/// concern), and nothing outside this crate reads a decision's status yet.
/// CLAUDE.md's "a trait needs two implementations" applies to a shared type
/// the same way it applies to an abstraction — one caller does not justify
/// moving it across a crate boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum DecisionStatus {
    /// Recorded, not yet decided. What [`upsert_decision`] writes on first
    /// insert.
    Open,
    /// A human accepted it.
    Accepted,
    /// A human rejected it.
    Rejected,
    /// A later decision replaced it.
    Superseded,
}

impl DecisionStatus {
    /// Every variant, in declaration order.
    pub const ALL: &'static [DecisionStatus] = &[
        DecisionStatus::Open,
        DecisionStatus::Accepted,
        DecisionStatus::Rejected,
        DecisionStatus::Superseded,
    ];

    /// The exact string persisted in `decision.status`.
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionStatus::Open => "OPEN",
            DecisionStatus::Accepted => "ACCEPTED",
            DecisionStatus::Rejected => "REJECTED",
            DecisionStatus::Superseded => "SUPERSEDED",
        }
    }
}

impl std::fmt::Display for DecisionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DecisionStatus {
    type Err = conductor_core::ParseStateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DecisionStatus::ALL
            .iter()
            .copied()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| conductor_core::ParseStateError {
                type_name: "DecisionStatus",
                value: s.to_string(),
            })
    }
}

/// One `decision` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecisionRow {
    /// `decision.id` — `D-0007`.
    pub id: String,
    /// `decision.project_id`.
    pub project_id: ProjectId,
    /// `decision.status`.
    pub status: DecisionStatus,
    /// `decision.supersedes` — the decision this one replaces, if any.
    pub supersedes: Option<String>,
    /// `decision.content_hash`.
    pub content_hash: String,
    /// `decision.source_path`.
    pub source_path: String,
}

/// What creating or resyncing a decision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDecision {
    /// `decision.id`.
    pub id: String,
    /// `decision.project_id` — must already exist (FK is `ON`).
    pub project_id: ProjectId,
    /// `decision.supersedes`.
    pub supersedes: Option<String>,
    /// `decision.content_hash`.
    pub content_hash: String,
    /// `decision.source_path`.
    pub source_path: String,
}

/// Insert a decision in `OPEN`, or — when this id already has a row —
/// refresh the content facts a re-synced decision file can change.
///
/// `status` is never written by this function once a row exists: it starts
/// `OPEN` at first insert and every later call leaves it alone. A decision's
/// status is a human's call, made through [`set_decision_status`]; a content
/// resync (the file's prose changed, its `content_hash` moved) must not
/// silently reopen an `ACCEPTED` decision or re-close a `REJECTED` one. Same
/// separation Ruling 5 draws for `plan_version`'s content vs. state, applied
/// to the table next to it.
pub fn upsert_decision(conn: &mut Connection, new: &NewDecision) -> StoreResult<DecisionRow> {
    with_immediate(conn, |tx| {
        tx.execute(
            "INSERT INTO decision (id, project_id, status, supersedes, content_hash, source_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
               supersedes = excluded.supersedes,
               content_hash = excluded.content_hash,
               source_path = excluded.source_path",
            params![
                new.id,
                new.project_id.as_str(),
                DecisionStatus::Open.as_str(),
                new.supersedes,
                new.content_hash,
                new.source_path,
            ],
        )?;
        Ok(())
    })?;
    decision(conn, &new.id)?.ok_or_else(|| StoreError::Domain("the decision vanished".to_string()))
}

/// One decision by id.
pub fn decision(conn: &Connection, id: &str) -> StoreResult<Option<DecisionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, status, supersedes, content_hash, source_path
           FROM decision WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some(to_decision_row(row)?)),
        None => Ok(None),
    }
}

/// Every decision recorded for a project, ordered by id for stable, diffable
/// output — the same reasoning `task::tasks` gives for ordering by id rather
/// than by insertion order.
pub fn decisions_for_project(
    conn: &Connection,
    project_id: &ProjectId,
) -> StoreResult<Vec<DecisionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, status, supersedes, content_hash, source_path
           FROM decision WHERE project_id = ?1 ORDER BY id",
    )?;
    let mut rows = stmt.query(params![project_id.as_str()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(to_decision_row(row)?);
    }
    Ok(out)
}

fn to_decision_row(row: &rusqlite::Row<'_>) -> StoreResult<DecisionRow> {
    let project_id: String = row.get(1)?;
    let status: String = row.get(2)?;
    Ok(DecisionRow {
        id: row.get(0)?,
        project_id: ProjectId::new(project_id)?,
        status: status.parse::<DecisionStatus>()?,
        supersedes: row.get(3)?,
        content_hash: row.get(4)?,
        source_path: row.get(5)?,
    })
}

/// The statuses a decision may move to next.
///
/// **Not drawn anywhere in the master plan.** §5.2 lists no "Decision" state
/// machine — only Plan, Task, Attempt, Approval and Review get diagrams. Part
/// 5.1's column comment names the four values and S11's scope line names
/// three of them as outcomes, but no source draws an arrow between any two.
/// This table is therefore this task's own construction, not a transcription,
/// and is reasoned the same way [`plan_version_successors`] is reasoned from
/// its diagram — from the one property S11 states outright.
///
/// **"Append-only decisions" (S11's objective) is the constraint.** A
/// decision record is a small ADR: once a human has accepted or rejected one,
/// reversing that call by editing the row would make the ledger say a
/// decision was always what it has just been changed to — the same
/// falsification `attempt`'s immutability and `plan_version`'s "immutable once
/// approved" both exist to prevent. So `ACCEPTED` and `REJECTED` are treated
/// as `APPROVED` is treated in [`plan_version_successors`]: terminal except
/// for one edge, `→ SUPERSEDED`, which is how a reversal is actually recorded
/// — as a **new** decision that supersedes the old one, leaving the old row's
/// own history intact. `OPEN → SUPERSEDED` is included for the same reason
/// `plan_version`'s "any → SUPERSEDED" is: a decision can be abandoned before
/// it is ever resolved.
///
/// The rejected alternative: allowing `ACCEPTED ↔ REJECTED` directly, so a
/// human could "just flip it back". That is exactly the append-only property
/// being asked to relitigate — it would let the ledger's only record of a
/// decision's outcome be silently overwritten, with nothing showing that a
/// decision was ever anything else. If S13's review bridge later needs a
/// reversal path, it is a new decision superseding this one, not a new edge
/// here.
fn decision_status_successors(status: DecisionStatus) -> &'static [DecisionStatus] {
    use DecisionStatus::*;
    match status {
        Open => &[Accepted, Rejected, Superseded],
        Accepted => &[Superseded],
        Rejected => &[Superseded],
        Superseded => &[],
    }
}

/// Move a decision, refusing anything [`decision_status_successors`] does not
/// draw. Same shape as [`set_plan_state`] and `task::set_task_state`.
pub fn set_decision_status(
    conn: &mut Connection,
    id: &str,
    to: DecisionStatus,
) -> StoreResult<DecisionStatus> {
    with_immediate(conn, |tx| {
        let current: String = tx
            .query_row(
                "SELECT status FROM decision WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(|_| StoreError::Domain(format!("no decision {id}")))?;
        let from = current.parse::<DecisionStatus>()?;
        if !decision_status_successors(from).contains(&to) {
            return Err(StoreError::Domain(format!(
                "{from} → {to} is not a transition the decision machine has"
            )));
        }
        tx.execute(
            "UPDATE decision SET status = ?2 WHERE id = ?1",
            params![id, to.as_str()],
        )?;
        Ok(to)
    })
}
