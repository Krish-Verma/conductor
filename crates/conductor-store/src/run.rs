//! Run, workspace, finding and approval rows the runtime reads and writes.
//!
//! Everything that mutates a run is fenced (§4.7). The read paths are not,
//! because a stale read cannot corrupt anything — only a stale *write* can, and
//! that is what [`crate::lease::check_fence`] refuses.

use conductor_core::{EventKind, Fence, RunId, RunState};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::StoreResult;
use crate::lease::{append_event, check_fence};
use crate::tx::with_immediate;

/// One `run` row, as recovery reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunRow {
    /// `run.id`.
    pub id: RunId,
    /// `run.task_id`.
    pub task_id: String,
    /// `run.state`.
    pub state: RunState,
    /// `run.base_commit`.
    pub base_commit: String,
    /// `run.run_branch`.
    pub run_branch: String,
    /// `run.lease_owner`.
    pub lease_owner: Option<String>,
    /// `run.lease_expires_at`.
    pub lease_expires_at: Option<i64>,
    /// `run.lease_epoch` — the fencing token.
    pub lease_epoch: i64,
    /// The workspace, when one has been recorded.
    pub workspace_path: Option<String>,
}

const RUN_COLUMNS: &str = "run.id, run.task_id, run.state, run.base_commit, run.run_branch, \
                           run.lease_owner, run.lease_expires_at, run.lease_epoch, workspace.path";

fn to_run_row(row: &rusqlite::Row<'_>) -> StoreResult<RunRow> {
    let state: String = row.get(2)?;
    Ok(RunRow {
        id: RunId::new(row.get::<_, String>(0)?)?,
        task_id: row.get(1)?,
        state: state.parse::<RunState>()?,
        base_commit: row.get(3)?,
        run_branch: row.get(4)?,
        lease_owner: row.get(5)?,
        lease_expires_at: row.get(6)?,
        lease_epoch: row.get(7)?,
        workspace_path: row.get(8)?,
    })
}

/// One run by id.
pub fn run(conn: &Connection, run_id: &RunId) -> StoreResult<Option<RunRow>> {
    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM run LEFT JOIN workspace ON workspace.id = run.workspace_id
          WHERE run.id = ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![run_id.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(Some(to_run_row(row)?)),
        None => Ok(None),
    }
}

/// Every run in one of `states`.
pub fn runs_in_states(conn: &Connection, states: &[RunState]) -> StoreResult<Vec<RunRow>> {
    let names: Vec<&str> = states.iter().map(|s| s.as_str()).collect();
    let placeholders = names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM run LEFT JOIN workspace ON workspace.id = run.workspace_id
          WHERE run.state IN ({placeholders}) ORDER BY run.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        names.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let mut rows = stmt.query(params.as_slice())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(to_run_row(row)?);
    }
    Ok(out)
}

/// Every run that is not in a terminal state.
pub fn active_runs(conn: &Connection) -> StoreResult<Vec<RunRow>> {
    let active: Vec<RunState> = RunState::ALL
        .iter()
        .copied()
        .filter(|s| !s.is_terminal())
        .collect();
    runs_in_states(conn, &active)
}

/// Record the run's workspace. Fenced.
pub fn attach_workspace(
    conn: &mut Connection,
    fence: &Fence,
    workspace_id: &str,
    path: &str,
    source_repo: &str,
    now_ms: i64,
) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "INSERT OR IGNORE INTO workspace (id, run_id, path, kind, source_repo, state, created_at)
             VALUES (?1, ?2, ?3, 'CLONE_NO_HARDLINKS', ?4, 'ACTIVE', ?5)",
            params![workspace_id, fence.run_id().as_str(), path, source_repo, now_ms],
        )?;
        tx.execute(
            "UPDATE run SET workspace_id = ?2 WHERE id = ?1 AND lease_epoch = ?3",
            params![fence.run_id().as_str(), workspace_id, fence.lease_epoch()],
        )?;
        Ok(())
    })
}

/// Raise a finding. Fenced.
///
/// §4.8: **findings never auto-resolve.** There is deliberately no function in
/// this module that clears one; `resolution` is written by a human decision path
/// (S13), never by the runtime.
pub fn record_finding(
    conn: &mut Connection,
    fence: &Fence,
    id: &str,
    kind: &str,
    severity: &str,
    evidence: &str,
    now_ms: i64,
) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "INSERT OR IGNORE INTO finding
               (id, run_id, kind, severity, evidence_ref, resolution, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)",
            params![
                id,
                fence.run_id().as_str(),
                kind,
                severity,
                evidence,
                now_ms
            ],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::FindingRaised,
            &format!("{{\"finding\":\"{id}\",\"kind\":\"{kind}\",\"severity\":\"{severity}\"}}"),
            now_ms,
        )?;
        Ok(())
    })
}

/// One `finding` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindingRow {
    /// `finding.id`.
    pub id: String,
    /// The run it belongs to.
    pub run_id: RunId,
    /// `finding.kind`.
    pub kind: String,
    /// `finding.severity`.
    pub severity: String,
    /// `finding.evidence_ref`.
    pub evidence_ref: String,
    /// `finding.resolution` — never written by the runtime.
    pub resolution: Option<String>,
}

/// Every finding for a run, oldest first.
pub fn findings_for_run(conn: &Connection, run_id: &RunId) -> StoreResult<Vec<FindingRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, run_id, kind, severity, evidence_ref, resolution
           FROM finding WHERE run_id = ?1 ORDER BY created_at, id",
    )?;
    let mut rows = stmt.query(params![run_id.as_str()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(FindingRow {
            id: row.get(0)?,
            run_id: RunId::new(row.get::<_, String>(1)?)?,
            kind: row.get(2)?,
            severity: row.get(3)?,
            evidence_ref: row.get(4)?,
            resolution: row.get(5)?,
        });
    }
    Ok(out)
}

/// Expire approval requests whose TTL has passed — §4.7 step 9.
///
/// Unfenced by design: an approval TTL is a property of the request, not of any
/// worker's lease, and a run whose worker died must still have its approval
/// expire. Returns the ids expired.
pub fn expire_approvals(conn: &mut Connection, now_ms: i64) -> StoreResult<Vec<String>> {
    with_immediate(conn, |tx| {
        let ids = {
            let mut stmt = tx.prepare(
                "SELECT id FROM approval_request WHERE state = 'REQUESTED' AND expires_at < ?1",
            )?;
            stmt.query_map(params![now_ms], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<String>, rusqlite::Error>>()?
        };
        if !ids.is_empty() {
            tx.execute(
                "UPDATE approval_request SET state = 'EXPIRED'
                  WHERE state = 'REQUESTED' AND expires_at < ?1",
                params![now_ms],
            )?;
        }
        Ok(ids)
    })
}

/// Approval requests still waiting — §4.7 step 9's "restore `AWAITING_APPROVAL`
/// waits".
pub fn pending_approvals(conn: &Connection) -> StoreResult<Vec<(String, RunId, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT id, run_id, expires_at FROM approval_request
          WHERE state = 'REQUESTED' ORDER BY requested_at, id",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((
            row.get::<_, String>(0)?,
            RunId::new(row.get::<_, String>(1)?)?,
            row.get::<_, i64>(2)?,
        ));
    }
    Ok(out)
}

/// Whether a verification result already exists for this tree (§4.7 step 6:
/// "Re-run verification only if the tree hash has no cached valid result").
///
/// S3 asks the question and records the answer; S4 owns the runner that acts on
/// it. Asking now is not speculative — recovery has to know whether it is
/// looking at work that still needs checking.
pub fn has_valid_verification(conn: &Connection, tree_hash: &str) -> StoreResult<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM verification_check
          WHERE tree_hash = ?1 AND outcome IN ('PASS','FAIL')",
        params![tree_hash],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}
