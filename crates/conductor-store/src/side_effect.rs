//! The side-effect ledger — §4.7, acceptance row 22.
//!
//! ```text
//! BEGIN IMMEDIATE; INSERT side_effect(operation_id, kind, 'INTENDED', precondition); COMMIT;
//!     perform the effect                                    ← crash window
//! BEGIN IMMEDIATE; UPDATE side_effect SET state='CONFIRMED', receipt=?; COMMIT;
//! ```
//!
//! The store's part is narrow on purpose: make the intent durable *before* the
//! effect, make the receipt durable *after* it, and make an unresolved intent
//! findable on restart. Whether the effect actually happened is not a database
//! question — §4.7 answers it by re-checking the precondition against the world,
//! never by blind retry — so that decision lives in `conductor-run`, which can
//! look at the world.

use conductor_core::effect::{OperationId, Precondition, SideEffectKind, SideEffectState};
use conductor_core::{EventKind, Fence, RunId};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::StoreResult;
use crate::lease::{append_event, check_fence};
use crate::tx::with_immediate;

/// One `side_effect` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SideEffectRow {
    /// `blake3(kind ‖ run_id ‖ attempt_ordinal ‖ tree_hash)`.
    pub operation_id: OperationId,
    /// The run that owns it.
    pub run_id: RunId,
    /// What kind of effect.
    pub kind: SideEffectKind,
    /// Where in the intent/receipt cycle it is.
    pub state: SideEffectState,
    /// What must be true of the world for the effect to have happened.
    pub precondition: Precondition,
    /// Evidence of the outcome, once there is any.
    pub receipt: Option<String>,
    /// When the intent was recorded.
    pub intended_at: i64,
    /// When it stopped being `INTENDED`.
    pub resolved_at: Option<i64>,
}

/// Declare an effect before performing it.
///
/// Returns the state of the row **as it now stands**, which is the useful
/// answer: `INTENDED` means go ahead, `CONFIRMED` means somebody already did it
/// and the caller must not do it again, `AMBIGUOUS` means a human owes a
/// decision. Re-intending never overwrites an existing row — the original
/// `intended_at` is what a restart reasons about.
pub fn intend_effect(
    conn: &mut Connection,
    fence: &Fence,
    operation_id: &OperationId,
    kind: SideEffectKind,
    precondition: &Precondition,
    now_ms: i64,
) -> StoreResult<SideEffectState> {
    let precondition_json = serde_json::to_string(precondition)?;
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;

        let existing: Option<String> = tx
            .query_row(
                "SELECT state FROM side_effect WHERE operation_id = ?1",
                params![operation_id.as_str()],
                |row| row.get(0),
            )
            .ok();
        if let Some(state) = existing {
            return Ok(state.parse::<SideEffectState>()?);
        }

        tx.execute(
            "INSERT INTO side_effect
               (operation_id, run_id, kind, state, precondition, receipt, intended_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, NULL)",
            params![
                operation_id.as_str(),
                fence.run_id().as_str(),
                kind.as_str(),
                SideEffectState::Intended.as_str(),
                precondition_json,
                now_ms,
            ],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::EffectIntended,
            &format!(
                "{{\"operation_id\":\"{operation_id}\",\"kind\":\"{}\"}}",
                kind.as_str()
            ),
            now_ms,
        )?;
        Ok(SideEffectState::Intended)
    })
}

/// Record the receipt: the effect happened.
pub fn confirm_effect(
    conn: &mut Connection,
    fence: &Fence,
    operation_id: &OperationId,
    receipt: &str,
    now_ms: i64,
) -> StoreResult<()> {
    resolve(
        conn,
        fence,
        operation_id,
        SideEffectState::Confirmed,
        receipt,
        EventKind::EffectConfirmed,
        now_ms,
    )
}

/// Record that the effect did not happen and is known not to have happened.
pub fn fail_effect(
    conn: &mut Connection,
    fence: &Fence,
    operation_id: &OperationId,
    reason: &str,
    now_ms: i64,
) -> StoreResult<()> {
    resolve(
        conn,
        fence,
        operation_id,
        SideEffectState::Failed,
        reason,
        EventKind::EffectConfirmed,
        now_ms,
    )
}

/// The precondition could not be decided. §4.7: halt, raise a finding, require a
/// human. **Never guess.**
pub fn mark_effect_ambiguous(
    conn: &mut Connection,
    fence: &Fence,
    operation_id: &OperationId,
    reason: &str,
    now_ms: i64,
) -> StoreResult<()> {
    resolve(
        conn,
        fence,
        operation_id,
        SideEffectState::Ambiguous,
        reason,
        EventKind::EffectAmbiguous,
        now_ms,
    )
}

fn resolve(
    conn: &mut Connection,
    fence: &Fence,
    operation_id: &OperationId,
    state: SideEffectState,
    receipt: &str,
    event: EventKind,
    now_ms: i64,
) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "UPDATE side_effect SET state = ?2, receipt = ?3, resolved_at = ?4
              WHERE operation_id = ?1",
            params![operation_id.as_str(), state.as_str(), receipt, now_ms],
        )?;
        append_event(
            tx,
            fence.run_id(),
            event,
            &format!(
                "{{\"operation_id\":\"{operation_id}\",\"state\":\"{}\",\"detail\":{}}}",
                state.as_str(),
                serde_json::to_string(receipt)?
            ),
            now_ms,
        )?;
        Ok(())
    })
}

/// One ledger row by its operation id.
pub fn side_effect(
    conn: &Connection,
    operation_id: &OperationId,
) -> StoreResult<Option<SideEffectRow>> {
    let mut stmt = conn.prepare(
        "SELECT operation_id, run_id, kind, state, precondition, receipt, intended_at, resolved_at
           FROM side_effect WHERE operation_id = ?1",
    )?;
    let mut rows = stmt.query(params![operation_id.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_side_effect(row)?)),
        None => Ok(None),
    }
}

/// Every effect still in `INTENDED` — the crash window, as found on restart.
pub fn unresolved_effects(conn: &Connection) -> StoreResult<Vec<SideEffectRow>> {
    let mut stmt = conn.prepare(
        "SELECT operation_id, run_id, kind, state, precondition, receipt, intended_at, resolved_at
           FROM side_effect WHERE state = 'INTENDED' ORDER BY intended_at, operation_id",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_side_effect(row)?);
    }
    Ok(out)
}

/// Every effect a human must decide about.
pub fn ambiguous_effects(conn: &Connection) -> StoreResult<Vec<SideEffectRow>> {
    let mut stmt = conn.prepare(
        "SELECT operation_id, run_id, kind, state, precondition, receipt, intended_at, resolved_at
           FROM side_effect WHERE state = 'AMBIGUOUS' ORDER BY intended_at, operation_id",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_side_effect(row)?);
    }
    Ok(out)
}

fn row_to_side_effect(row: &rusqlite::Row<'_>) -> StoreResult<SideEffectRow> {
    let operation: String = row.get(0)?;
    let run_id: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let state: String = row.get(3)?;
    let precondition: String = row.get(4)?;
    Ok(SideEffectRow {
        operation_id: OperationId::from_stored(operation),
        run_id: RunId::new(run_id)?,
        kind: kind.parse::<SideEffectKind>()?,
        state: state.parse::<SideEffectState>()?,
        precondition: serde_json::from_str(&precondition)?,
        receipt: row.get(5)?,
        intended_at: row.get(6)?,
        resolved_at: row.get(7)?,
    })
}
