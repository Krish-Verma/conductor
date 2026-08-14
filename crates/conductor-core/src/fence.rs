//! The fencing token — master plan §4.7.
//!
//! > `lease_epoch` **is the fencing token.** Every subsequent write by that
//! > worker carries its epoch and is rejected if the epoch moved. Without
//! > fencing, a process that stalls past its lease and then wakes will happily
//! > write over its successor's work.
//!
//! A [`Fence`] is therefore not a convenience wrapper around two fields: it is
//! the *only* way to name a run in a write, so a write that forgot to fence is
//! a missing argument rather than a missing `WHERE` clause. Acceptance row 27 is
//! the test.

use crate::ids::RunId;

/// Authority to write to one run, at one epoch.
///
/// Obtained from a claim and from nowhere else. Cloning it does not extend it —
/// the epoch is a fact about the database, and a stale copy is exactly what the
/// store rejects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fence {
    run_id: RunId,
    lease_epoch: i64,
}

impl Fence {
    /// Build a fence for a run at a known epoch.
    ///
    /// Public because recovery reads an epoch out of the database rather than
    /// receiving it from a claim; the store still verifies it on every write, so
    /// constructing one confers nothing that the database does not confirm.
    pub fn new(run_id: RunId, lease_epoch: i64) -> Self {
        Fence {
            run_id,
            lease_epoch,
        }
    }

    /// The run this fence authorises.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// The epoch the write will be checked against.
    pub fn lease_epoch(&self) -> i64 {
        self.lease_epoch
    }
}

impl std::fmt::Display for Fence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@epoch{}", self.run_id, self.lease_epoch)
    }
}
