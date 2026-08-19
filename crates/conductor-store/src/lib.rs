//! Conductor's store: one SQLite database, one transaction domain.
//!
//! Concrete by design (master plan §2.5). There is no `Store` trait: splitting
//! the store behind an interface invites a write that spans two "stores" and
//! therefore two transactions, which is the bug class Part 5 exists to prevent.

pub mod attempt;
pub mod claim;
pub mod error;
pub mod lease;
pub mod ledger;
pub mod migrate;
pub mod repair;
pub mod review;
pub mod run;
pub mod schema;
pub mod side_effect;
pub mod task;
pub mod tx;
pub mod verification;

use std::path::{Path, PathBuf};

use conductor_core::effect::{OperationId, Precondition, SideEffectKind, SideEffectState};
use conductor_core::{
    AttemptId, EventKind, Fence, PlanVersionId, PlanVersionState, ProjectId, ReconciledRoute,
    ReviewDecision, RunId, RunState, TaskId, TaskState, TerminalAttempt,
};
use rusqlite::{Connection, OpenFlags};

pub use attempt::{AttemptRow, NewAttempt};
pub use claim::{ClaimedRun, claim_next_run, claim_run};
pub use error::{StoreError, StoreResult};
pub use lease::{ExpiredLease, HEARTBEAT_MS, LEASE_MS};
pub use ledger::{
    DecisionRow, DecisionStatus, NewDecision, NewPlanVersion, NewProject, PlanVersionRow,
    ProjectRow,
};
pub use migrate::{MigrationStep, migrate};
pub use repair::{NewRepairObservation, RepairObservationRow};
pub use review::{NewReview, ReviewRow};
pub use run::{FindingRow, RunRow};
pub use schema::PragmaReport;
pub use side_effect::SideEffectRow;
pub use task::{NewRun, NewTask, TaskRow};
pub use tx::with_immediate;
pub use verification::RunCheckResult;

/// An open store.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open, creating the file and its parent directory if needed, and migrate
    /// forward. This is the only path that may create a database.
    pub fn open_or_create(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut conn = Connection::open(&path)?;
        schema::apply_pragmas(&conn)?;
        migrate::migrate(&mut conn)?;
        Ok(Store { conn, path })
    }

    /// Open an existing store. Never creates the database and never migrates —
    /// this is what `conductor doctor` uses, because reporting on a store must
    /// not bring one into existence.
    pub fn open_existing(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(StoreError::NotFound(path));
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        schema::apply_pragmas(&conn)?;
        Ok(Store { conn, path })
    }

    /// `~/.local/share/conductor/conductor.db` (master plan §3.1), honouring
    /// `XDG_DATA_HOME` when it is set.
    pub fn default_path() -> StoreResult<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
            && !xdg.is_empty()
        {
            return Ok(PathBuf::from(xdg).join("conductor").join("conductor.db"));
        }
        let home = std::env::var_os("HOME").ok_or(StoreError::NoHome("HOME"))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("conductor")
            .join("conductor.db"))
    }

    /// The path this store was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutably borrow the connection — required for transactions.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Pragma values actually in effect on this connection.
    pub fn pragmas(&self) -> StoreResult<PragmaReport> {
        schema::read_pragmas(&self.conn)
    }

    /// Highest applied schema version, `None` on an empty database.
    pub fn schema_version(&self) -> StoreResult<Option<i64>> {
        migrate::current_version(&self.conn)
    }

    /// `PRAGMA integrity_check`.
    pub fn integrity_check(&self) -> StoreResult<Vec<String>> {
        migrate::integrity_check(&self.conn)
    }

    /// Number of `PRAGMA foreign_key_check` violations.
    pub fn foreign_key_check(&self) -> StoreResult<usize> {
        let mut stmt = self.conn.prepare("PRAGMA foreign_key_check")?;
        let count = stmt.query_map([], |_| Ok(()))?.count();
        Ok(count)
    }

    /// Claim the next eligible run (§4.7).
    pub fn claim_next_run(
        &mut self,
        owner: &str,
        now_ms: i64,
        lease_ms: i64,
    ) -> StoreResult<Option<ClaimedRun>> {
        claim::claim_next_run(&mut self.conn, owner, now_ms, lease_ms)
    }

    /// Claim one named run (§4.7), under the same predicate as the general
    /// claim. Startup recovery needs the run it is recovering.
    pub fn claim_run(
        &mut self,
        run_id: &RunId,
        owner: &str,
        now_ms: i64,
        lease_ms: i64,
    ) -> StoreResult<Option<ClaimedRun>> {
        claim::claim_run(&mut self.conn, run_id, owner, now_ms, lease_ms)
    }

    // ---- runs, workspaces, findings, approvals ---------------------------

    /// One run by id, with its workspace path when it has one.
    pub fn run(&self, run_id: &RunId) -> StoreResult<Option<run::RunRow>> {
        run::run(&self.conn, run_id)
    }

    /// Every run in one of `states`.
    pub fn runs_in_states(&self, states: &[RunState]) -> StoreResult<Vec<run::RunRow>> {
        run::runs_in_states(&self.conn, states)
    }

    /// Every run that is not in a terminal state.
    pub fn active_runs(&self) -> StoreResult<Vec<run::RunRow>> {
        run::active_runs(&self.conn)
    }

    /// Record the run's workspace. Fenced.
    pub fn attach_workspace(
        &mut self,
        fence: &Fence,
        workspace_id: &str,
        path: &str,
        source_repo: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        run::attach_workspace(
            &mut self.conn,
            fence,
            workspace_id,
            path,
            source_repo,
            now_ms,
        )
    }

    /// Raise a finding. Fenced. Findings never auto-resolve (§4.8).
    pub fn record_finding(
        &mut self,
        fence: &Fence,
        id: &str,
        kind: &str,
        severity: &str,
        evidence: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        run::record_finding(&mut self.conn, fence, id, kind, severity, evidence, now_ms)
    }

    /// Every finding for a run.
    pub fn findings_for_run(&self, run_id: &RunId) -> StoreResult<Vec<run::FindingRow>> {
        run::findings_for_run(&self.conn, run_id)
    }

    /// Resolve a finding on a human's say-so (§4.8, S13). Fenced.
    ///
    /// The only writer of `finding.resolution`, and it refuses a second
    /// resolution rather than overwriting the first human's reason; see
    /// [`run::resolve_finding`].
    pub fn resolve_finding(
        &mut self,
        fence: &Fence,
        id: &str,
        resolution: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        run::resolve_finding(&mut self.conn, fence, id, resolution, now_ms)
    }

    /// Expire overdue approval requests (§4.7 step 9).
    pub fn expire_approvals(&mut self, now_ms: i64) -> StoreResult<Vec<String>> {
        run::expire_approvals(&mut self.conn, now_ms)
    }

    /// Approval requests still waiting. The run and the expiry are optional —
    /// see [`run::pending_approvals`].
    pub fn pending_approvals(&self) -> StoreResult<Vec<run::PendingApproval>> {
        run::pending_approvals(&self.conn)
    }

    /// Whether a verification result is already cached for this tree.
    pub fn has_valid_verification(&self, tree_hash: &str) -> StoreResult<bool> {
        run::has_valid_verification(&self.conn, tree_hash)
    }

    /// Every verification check recorded for one run, in the order they ran.
    ///
    /// The single accessor for a query two call sites hand-roll today; the
    /// command **text** is not stored, only its hash. See
    /// [`verification::results_for_run`].
    pub fn verification_results_for_run(&self, run_id: &RunId) -> StoreResult<Vec<RunCheckResult>> {
        verification::results_for_run(&self.conn, run_id)
    }

    // ---- leases and fencing (§4.7) ---------------------------------------

    /// Extend a lease this worker holds. Fenced.
    pub fn renew_lease(&mut self, fence: &Fence, now_ms: i64, lease_ms: i64) -> StoreResult<i64> {
        lease::renew_lease(&mut self.conn, fence, now_ms, lease_ms)
    }

    /// Hand a run back without revoking anything. Fenced.
    pub fn release_lease(&mut self, fence: &Fence, now_ms: i64) -> StoreResult<()> {
        lease::release_lease(&mut self.conn, fence, now_ms)
    }

    /// The lease timer: force every lapsed run to `RECONCILING` and move its
    /// epoch, fencing the vanished worker out.
    pub fn expire_leases(&mut self, now_ms: i64) -> StoreResult<Vec<ExpiredLease>> {
        lease::expire_leases(&mut self.conn, now_ms)
    }

    /// `RUNNING → RECONCILING`, the only exit from `RUNNING`. Fenced.
    pub fn advance_to_reconciling(
        &mut self,
        fence: &Fence,
        evidence: &TerminalAttempt,
        now_ms: i64,
    ) -> StoreResult<RunState> {
        lease::advance_to_reconciling(&mut self.conn, fence, evidence, now_ms)
    }

    /// Refuse to launch a `READY` run on §4.2 eligibility grounds — row 30.
    ///
    /// Unfenced by design; see [`lease::refuse_ineligible_launch`].
    pub fn refuse_ineligible_launch(
        &mut self,
        run_id: &RunId,
        finding_id: &str,
        detail: &str,
        now_ms: i64,
    ) -> StoreResult<RunState> {
        lease::refuse_ineligible_launch(&mut self.conn, run_id, finding_id, detail, now_ms)
    }

    /// Re-enter reconciliation after an approval was answered — rows 12, 13.
    ///
    /// Unfenced by design; see [`lease::resume_after_grant`].
    pub fn resume_after_grant(
        &mut self,
        run_id: &RunId,
        detail: &str,
        now_ms: i64,
    ) -> StoreResult<RunState> {
        lease::resume_after_grant(&mut self.conn, run_id, detail, now_ms)
    }

    /// Route a reconciled run onwards. Fenced, and impossible to point at
    /// `COMPLETE`.
    pub fn route_reconciled(
        &mut self,
        fence: &Fence,
        route: ReconciledRoute,
        detail: &str,
        now_ms: i64,
    ) -> StoreResult<RunState> {
        lease::route_reconciled(&mut self.conn, fence, route, detail, now_ms)
    }

    /// Leave `VERIFYING` — §5.2's "`VERIFYING → COMPLETE` requires §4.5's seven
    /// criteria".
    pub fn route_verified(
        &mut self,
        fence: &Fence,
        route: ReconciledRoute,
        detail: &str,
        now_ms: i64,
    ) -> StoreResult<RunState> {
        lease::route_verified(&mut self.conn, fence, route, detail, now_ms)
    }

    /// `REPAIRING → READY` — §4.6's repair edge, so the next attempt can be
    /// claimed. Fenced, and checked against §5.2's table like every other move.
    pub fn reopen_for_repair(
        &mut self,
        fence: &Fence,
        detail: &str,
        now_ms: i64,
    ) -> StoreResult<RunState> {
        lease::reopen_for_repair(&mut self.conn, fence, detail, now_ms)
    }

    /// `REPAIRING → AWAITING_REVIEW` — §4.6's escalation. Fenced.
    pub fn escalate_from_repairing(
        &mut self,
        fence: &Fence,
        detail: &str,
        now_ms: i64,
    ) -> StoreResult<RunState> {
        lease::escalate_from_repairing(&mut self.conn, fence, detail, now_ms)
    }

    /// Move a run out of `AWAITING_REVIEW` on a human's decision — §5.2's four
    /// edges out of the review state, which had no writer at all before S13.
    ///
    /// Unfenced by design, and the evidence travels in the argument:
    /// `ReviewOutcome::Accepted` carries a `VerifiedComplete`, so `accept` cannot
    /// become a door into `COMPLETE` that skips §4.5's gate. See
    /// [`lease::apply_review_decision`].
    pub fn apply_review_decision(
        &mut self,
        run_id: &RunId,
        outcome: conductor_core::ReviewOutcome,
        detail: &str,
        now_ms: i64,
    ) -> StoreResult<RunState> {
        lease::apply_review_decision(&mut self.conn, run_id, outcome, detail, now_ms)
    }

    // ---- repair observations (§4.6, schema v5) ---------------------------

    /// Record what repair observed about one attempt. Fenced.
    pub fn record_repair_observation(
        &mut self,
        fence: &Fence,
        observation: &NewRepairObservation,
        now_ms: i64,
    ) -> StoreResult<()> {
        repair::record_observation(&mut self.conn, fence, observation, now_ms)
    }

    /// Every repair observation of one run, oldest attempt first.
    pub fn repair_observations_for_run(
        &self,
        run_id: &RunId,
    ) -> StoreResult<Vec<RepairObservationRow>> {
        repair::observations_for_run(&self.conn, run_id)
    }

    /// Append one evidence row. Fenced.
    pub fn record_event(
        &mut self,
        fence: &Fence,
        kind: EventKind,
        payload: &str,
        now_ms: i64,
    ) -> StoreResult<i64> {
        lease::record_event(&mut self.conn, fence, kind, payload, now_ms)
    }

    /// `run.state` and `run.lease_epoch` as they now stand.
    pub fn run_state(&self, run_id: &RunId) -> StoreResult<Option<(RunState, i64)>> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT state, lease_epoch FROM run WHERE id = ?1",
                rusqlite::params![run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        match row {
            Some((state, epoch)) => Ok(Some((state.parse::<RunState>()?, epoch))),
            None => Ok(None),
        }
    }

    // ---- attempts (§5.2, schema v2) --------------------------------------

    /// Open an attempt in `CREATED`. Fenced.
    pub fn create_attempt(
        &mut self,
        fence: &Fence,
        new: NewAttempt,
        now_ms: i64,
    ) -> StoreResult<conductor_core::Attempt<conductor_core::attempt::phase::Created>> {
        attempt::create_attempt(&mut self.conn, fence, new, now_ms)
    }

    /// `CREATED → STARTING`. Fenced.
    pub fn record_attempt_starting(
        &mut self,
        fence: &Fence,
        a: &conductor_core::Attempt<conductor_core::attempt::phase::Starting>,
        now_ms: i64,
    ) -> StoreResult<()> {
        attempt::record_attempt_starting(&mut self.conn, fence, a, now_ms)
    }

    /// `STARTING → ACTIVE`, recording pid and start time. Fenced.
    pub fn record_attempt_active(
        &mut self,
        fence: &Fence,
        a: &conductor_core::Attempt<conductor_core::attempt::phase::Active>,
        now_ms: i64,
    ) -> StoreResult<()> {
        attempt::record_attempt_active(&mut self.conn, fence, a, now_ms)
    }

    /// Record a terminal outcome. Fenced.
    pub fn record_attempt_terminal(
        &mut self,
        fence: &Fence,
        a: &conductor_core::attempt::TerminalPhase,
        now_ms: i64,
    ) -> StoreResult<()> {
        attempt::record_attempt_terminal(&mut self.conn, fence, a, now_ms)
    }

    /// Record that reconciliation happened. Fenced.
    pub fn record_attempt_reconciled(
        &mut self,
        fence: &Fence,
        a: &conductor_core::Attempt<conductor_core::attempt::phase::Reconciled>,
        now_ms: i64,
    ) -> StoreResult<()> {
        attempt::record_attempt_reconciled(&mut self.conn, fence, a, now_ms)
    }

    /// Every attempt that was in flight — what §4.7 step 3 reads.
    pub fn in_flight_attempts(&self) -> StoreResult<Vec<AttemptRow>> {
        attempt::in_flight_attempts(&self.conn)
    }

    /// Every attempt of one run, oldest first.
    /// Record the session an agent announced for itself — S10.
    ///
    /// Never clears an assigned session; see
    /// [`attempt::record_agent_session`].
    pub fn record_agent_session(
        &mut self,
        fence: &Fence,
        attempt_id: &AttemptId,
        session_id: &str,
    ) -> StoreResult<()> {
        attempt::record_agent_session(&mut self.conn, fence, attempt_id, session_id)
    }

    pub fn attempts_for_run(&self, run_id: &RunId) -> StoreResult<Vec<AttemptRow>> {
        attempt::attempts_for_run(&self.conn, run_id)
    }

    /// The next free `attempt.ordinal` for a run.
    pub fn next_attempt_ordinal(&self, run_id: &RunId) -> StoreResult<i64> {
        attempt::next_ordinal(&self.conn, run_id)
    }

    // ---- side effects (§4.7) ---------------------------------------------

    /// Declare an effect before performing it. Fenced.
    pub fn intend_effect(
        &mut self,
        fence: &Fence,
        operation_id: &OperationId,
        kind: SideEffectKind,
        precondition: &Precondition,
        now_ms: i64,
    ) -> StoreResult<SideEffectState> {
        side_effect::intend_effect(
            &mut self.conn,
            fence,
            operation_id,
            kind,
            precondition,
            now_ms,
        )
    }

    /// Record the receipt. Fenced.
    pub fn confirm_effect(
        &mut self,
        fence: &Fence,
        operation_id: &OperationId,
        receipt: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        side_effect::confirm_effect(&mut self.conn, fence, operation_id, receipt, now_ms)
    }

    /// Record that the effect is known not to have happened. Fenced.
    pub fn fail_effect(
        &mut self,
        fence: &Fence,
        operation_id: &OperationId,
        reason: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        side_effect::fail_effect(&mut self.conn, fence, operation_id, reason, now_ms)
    }

    /// The precondition could not be decided: halt and ask a human. Fenced.
    pub fn mark_effect_ambiguous(
        &mut self,
        fence: &Fence,
        operation_id: &OperationId,
        reason: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        side_effect::mark_effect_ambiguous(&mut self.conn, fence, operation_id, reason, now_ms)
    }

    /// One ledger row.
    pub fn side_effect(&self, operation_id: &OperationId) -> StoreResult<Option<SideEffectRow>> {
        side_effect::side_effect(&self.conn, operation_id)
    }

    /// Every effect still `INTENDED` — the crash window as found on restart.
    pub fn unresolved_effects(&self) -> StoreResult<Vec<SideEffectRow>> {
        side_effect::unresolved_effects(&self.conn)
    }

    /// Every effect a human must decide about.
    pub fn ambiguous_effects(&self) -> StoreResult<Vec<SideEffectRow>> {
        side_effect::ambiguous_effects(&self.conn)
    }

    // -- tasks and runs (S5) ------------------------------------------------

    /// Create a task in `PENDING`.
    pub fn create_task(&mut self, task: &NewTask, now_ms: i64) -> StoreResult<TaskRow> {
        task::create_task(&mut self.conn, task, now_ms)
    }

    /// Create a run in `READY` for an existing task.
    pub fn create_run(&mut self, run: &NewRun, now_ms: i64) -> StoreResult<()> {
        task::create_run(&mut self.conn, run, now_ms)
    }

    /// One task by id.
    pub fn task(&self, id: &TaskId) -> StoreResult<Option<TaskRow>> {
        task::task_row(&self.conn, id)
    }

    /// Every task, optionally filtered by state.
    pub fn tasks(&self, state: Option<TaskState>) -> StoreResult<Vec<TaskRow>> {
        task::tasks(&self.conn, state)
    }

    /// A task's §4.2 `execution_requirements`, as written. `None` = nothing
    /// gated.
    pub fn execution_requirements(&self, id: &TaskId) -> StoreResult<Option<String>> {
        task::execution_requirements(&self.conn, id)
    }

    /// Record a task's §4.2 `execution_requirements`. `None` clears them.
    pub fn set_execution_requirements(
        &mut self,
        id: &TaskId,
        yaml: Option<&str>,
    ) -> StoreResult<()> {
        task::set_execution_requirements(&mut self.conn, id, yaml)
    }

    /// The one non-terminal run of a task, if it has one.
    pub fn active_run_for_task(&self, task_id: &TaskId) -> StoreResult<Option<RunId>> {
        run::active_run_for_task(&self.conn, task_id.as_str())
    }

    /// Move a task, refusing anything §5.2's machine does not draw.
    pub fn set_task_state(&mut self, id: &TaskId, to: TaskState) -> StoreResult<TaskState> {
        task::set_task_state(&mut self.conn, id, to)
    }

    /// A task's materialized §4.4 `declared_actions`, as raw plan-model JSON.
    /// `None` = never materialized; `Some("[]")` = materialized, declares
    /// none. See [`schema::SCHEMA_V8`].
    pub fn declared_actions(&self, id: &TaskId) -> StoreResult<Option<String>> {
        task::declared_actions(&self.conn, id)
    }

    /// Record a task's materialized `declared_actions`. `None` clears it back
    /// to "never materialized".
    pub fn set_declared_actions(&mut self, id: &TaskId, json: Option<&str>) -> StoreResult<()> {
        task::set_declared_actions(&mut self.conn, id, json)
    }

    /// A task's materialized `depends_on`, as raw plan-model JSON. Same
    /// `None`/`Some("[]")` distinction as [`Store::declared_actions`].
    pub fn depends_on(&self, id: &TaskId) -> StoreResult<Option<String>> {
        task::depends_on(&self.conn, id)
    }

    /// Record a task's materialized `depends_on`. `None` clears it back to
    /// "never materialized".
    pub fn set_depends_on(&mut self, id: &TaskId, json: Option<&str>) -> StoreResult<()> {
        task::set_depends_on(&mut self.conn, id, json)
    }

    /// A task's materialized `acceptance_criteria`, as raw plan-model JSON.
    /// Same `None`/`Some("[]")` distinction as [`Store::declared_actions`].
    pub fn acceptance_criteria(&self, id: &TaskId) -> StoreResult<Option<String>> {
        task::acceptance_criteria(&self.conn, id)
    }

    /// Record a task's materialized `acceptance_criteria`. `None` clears it
    /// back to "never materialized".
    pub fn set_acceptance_criteria(&mut self, id: &TaskId, json: Option<&str>) -> StoreResult<()> {
        task::set_acceptance_criteria(&mut self.conn, id, json)
    }

    // -- plan ledger: project, plan_version, decision (S11 T2) -------------

    /// Insert a project, or refresh the facts about one already known.
    pub fn upsert_project(&mut self, new: &NewProject, now_ms: i64) -> StoreResult<ProjectRow> {
        ledger::upsert_project(&mut self.conn, new, now_ms)
    }

    /// A project by its root path.
    pub fn project_by_root(&self, root_path: &str) -> StoreResult<Option<ProjectRow>> {
        ledger::project_by_root(&self.conn, root_path)
    }

    /// One project by id.
    pub fn project(&self, id: &ProjectId) -> StoreResult<Option<ProjectRow>> {
        ledger::project(&self.conn, id)
    }

    /// Create a plan version in `DRAFT`. Errors on a duplicate id — a plan
    /// version is immutable once written, not resynced.
    pub fn create_plan_version(&mut self, new: &NewPlanVersion) -> StoreResult<PlanVersionRow> {
        ledger::create_plan_version(&mut self.conn, new)
    }

    /// One plan version by id.
    pub fn plan_version(&self, id: &PlanVersionId) -> StoreResult<Option<PlanVersionRow>> {
        ledger::plan_version(&self.conn, id)
    }

    /// Every version of a project's plan, oldest first.
    pub fn plan_versions_for_project(
        &self,
        project_id: &ProjectId,
    ) -> StoreResult<Vec<PlanVersionRow>> {
        ledger::plan_versions_for_project(&self.conn, project_id)
    }

    /// Move a plan version, refusing anything §5.2's machine does not draw.
    pub fn set_plan_state(
        &mut self,
        id: &PlanVersionId,
        to: PlanVersionState,
    ) -> StoreResult<PlanVersionState> {
        ledger::set_plan_state(&mut self.conn, id, to)
    }

    /// `→ SUPERSEDED` — the edge every non-terminal plan state has.
    pub fn supersede_plan_version(&mut self, id: &PlanVersionId) -> StoreResult<PlanVersionState> {
        ledger::supersede_plan_version(&mut self.conn, id)
    }

    /// Record approval content on a plan version that is already `APPROVED`
    /// — Ruling 5's door that is not the transition table. See
    /// [`ledger::record_plan_approval_content`].
    pub fn record_plan_approval_content(
        &mut self,
        id: &PlanVersionId,
        content_hash: &str,
        approved_by: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        ledger::record_plan_approval_content(&mut self.conn, id, content_hash, approved_by, now_ms)
    }

    /// Insert a decision in `OPEN`, or refresh the content facts a re-synced
    /// decision file can change. Never touches `status`.
    pub fn upsert_decision(&mut self, new: &NewDecision) -> StoreResult<DecisionRow> {
        ledger::upsert_decision(&mut self.conn, new)
    }

    /// One decision by id.
    pub fn decision(&self, id: &str) -> StoreResult<Option<DecisionRow>> {
        ledger::decision(&self.conn, id)
    }

    /// Every decision recorded for a project, ordered by id.
    pub fn decisions_for_project(&self, project_id: &ProjectId) -> StoreResult<Vec<DecisionRow>> {
        ledger::decisions_for_project(&self.conn, project_id)
    }

    /// Move a decision, refusing anything the decision machine does not draw.
    pub fn set_decision_status(
        &mut self,
        id: &str,
        to: DecisionStatus,
    ) -> StoreResult<DecisionStatus> {
        ledger::set_decision_status(&mut self.conn, id, to)
    }

    // -- reviews: §5.2's three states, §6.5's packet and decision (S13) -----

    /// Open a review in `PENDING`. At most one open review per run.
    pub fn open_review(&mut self, new: &NewReview, now_ms: i64) -> StoreResult<ReviewRow> {
        review::open(&mut self.conn, new, now_ms)
    }

    /// `PENDING → EXPORTED`, binding the review to the packet a human will read.
    pub fn mark_review_exported(
        &mut self,
        id: &str,
        packet_hash: &str,
        packet_path: &str,
    ) -> StoreResult<ReviewRow> {
        review::mark_exported(&mut self.conn, id, packet_hash, packet_path)
    }

    /// `EXPORTED → DECIDED`, recording §6.5's imported decision.
    ///
    /// Refused while `packet_hash` is `NULL`: the decision authorizes a packet
    /// (§4.3), not a review in the abstract. Does **not** move the task — that
    /// goes through [`Store::set_task_state`], which checks §5.2's table.
    pub fn record_review_decision(
        &mut self,
        id: &str,
        decision: ReviewDecision,
        decided_by: &str,
        notes: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<ReviewRow> {
        review::record_decision(&mut self.conn, id, decision, decided_by, notes, now_ms)
    }

    /// One review by id.
    pub fn review(&self, id: &str) -> StoreResult<Option<ReviewRow>> {
        review::review(&self.conn, id)
    }

    /// The one open review of a run, if it has one — what the export and import
    /// paths resolve, since an operator names a run rather than a review.
    pub fn open_review_for_run(&self, run_id: &RunId) -> StoreResult<Option<ReviewRow>> {
        review::open_review_for_run(&self.conn, run_id)
    }

    /// Every review of a run, oldest first.
    pub fn reviews_for_run(&self, run_id: &RunId) -> StoreResult<Vec<ReviewRow>> {
        review::reviews_for_run(&self.conn, run_id)
    }
}
