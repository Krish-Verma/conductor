//! Transaction discipline.
//!
//! Master plan Part 5.1: **every write uses `BEGIN IMMEDIATE`.** A deferred
//! transaction takes a read lock and tries to upgrade it on first write; in WAL
//! that upgrade fails with `SQLITE_BUSY` that no `busy_timeout` resolves,
//! because the reader's snapshot would have to be invalidated.

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::error::{StoreError, StoreResult};

/// Run `f` inside a `BEGIN IMMEDIATE` transaction: commit on `Ok`, roll back on
/// `Err`.
///
/// A panic inside `f` unwinds through `Transaction`'s `Drop`, which rolls back.
pub fn with_immediate<T, F>(conn: &mut Connection, f: F) -> StoreResult<T>
where
    F: FnOnce(&Transaction<'_>) -> StoreResult<T>,
{
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match f(&tx) {
        Ok(value) => {
            tx.commit()?;
            Ok(value)
        }
        Err(err) => match tx.rollback() {
            Ok(()) => Err(err),
            // Losing the original error here would hide why the rollback
            // happened, so carry it as text alongside the rollback failure.
            Err(rollback) => Err(StoreError::RollbackFailed {
                original: err.to_string(),
                source: rollback,
            }),
        },
    }
}
