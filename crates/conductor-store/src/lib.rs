//! Conductor's store: one SQLite database, one transaction domain.
//!
//! Concrete by design (master plan §2.5). There is no `Store` trait: splitting
//! the store behind an interface invites a write that spans two "stores" and
//! therefore two transactions, which is the bug class Part 5 exists to prevent.

pub mod attempt;
pub mod claim;
pub mod error;
pub mod lease;
pub mod migrate;
pub mod repair;
pub mod run;
pub mod schema;
pub mod side_effect;
pub mod task;
pub mod tx;
pub mod verification;

use std::path::{Path, PathBuf};

use conductor_core::effect::{OperationId, Precondition, SideEffectKind, SideEffectState};
use conductor_core::{
    EventKind, Fence, ReconciledRoute, RunId, RunState, TaskId, TaskState, TerminalAttempt,
};
use rusqlite::{Connection, OpenFlags};

pub use attempt::{AttemptRow, NewAttempt};
pub use claim::{ClaimedRun, claim_next_run, claim_run};
pub use error::{StoreError, StoreResult};
pub use lease::{ExpiredLease, HEARTBEAT_MS, LEASE_MS};
pub use migrate::{MigrationStep, migrate};
pub use repair::{NewRepairObservation, RepairObservationRow};
pub use run::{FindingRow, RunRow};
pub use schema::PragmaReport;
pub use side_effect::SideEffectRow;
pub use task::{NewRun, NewTask, TaskRow};
pub use tx::with_immediate;

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

    /// The one non-terminal run of a task, if it has one.
    pub fn active_run_for_task(&self, task_id: &TaskId) -> StoreResult<Option<RunId>> {
        run::active_run_for_task(&self.conn, task_id.as_str())
    }

    /// Move a task, refusing anything §5.2's machine does not draw.
    pub fn set_task_state(&mut self, id: &TaskId, to: TaskState) -> StoreResult<TaskState> {
        task::set_task_state(&mut self.conn, id, to)
    }
}
