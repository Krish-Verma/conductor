//! Heartbeating a lease — master plan §4.7.
//!
//! > **Leases:** 60 s. **Heartbeat:** every 15 s, **conditional on the agent
//! > process still existing** (`kill(pid, 0)`) — a supervisor that heartbeats
//! > while its child is dead is worse than one that crashes.
//!
//! "Conditional on" is enforced by the signature, not by a comment: [`heartbeat`]
//! takes a [`ChildAlive`], and the only way to obtain one is
//! [`crate::supervise::probe`] actually finding the process. A supervisor that
//! wanted to heartbeat blindly would have to invent a witness, and the type has
//! no public constructor.
//!
//! The durable half — the fenced `UPDATE` — lives in `conductor-store`. This is
//! the half that needs to look at a process, which is why it is here and not
//! there: the store has no business knowing what a pid is.

use std::time::{Duration, Instant};

use conductor_core::Fence;
use conductor_store::{Store, StoreError};

use crate::supervise::ChildAlive;

/// What a heartbeat did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// The lease was extended to this instant, in epoch milliseconds.
    Renewed {
        /// New `run.lease_expires_at`.
        expires_at: i64,
    },
    /// Not due yet. §4.7's tick is 15 s against a 60 s lease; heartbeating on
    /// every poll would turn a supervision loop into a write loop.
    NotDue,
    /// The epoch moved: this worker no longer owns the run (acceptance row 27).
    ///
    /// The supervisor's correct response is to stop — it is writing on behalf of
    /// a lease somebody else now holds.
    FencedOut,
}

/// Extend a lease, given proof the child is alive.
///
/// `last_beat` is when this worker last renewed; `now_ms` and `interval` decide
/// whether another one is due.
pub fn heartbeat(
    store: &mut Store,
    fence: &Fence,
    _alive: &ChildAlive,
    last_beat: &mut Option<Instant>,
    interval: Duration,
    now_ms: i64,
    lease_ms: i64,
) -> Result<HeartbeatOutcome, StoreError> {
    if let Some(previous) = last_beat
        && previous.elapsed() < interval
    {
        return Ok(HeartbeatOutcome::NotDue);
    }

    match store.renew_lease(fence, now_ms, lease_ms) {
        Ok(expires_at) => {
            *last_beat = Some(Instant::now());
            Ok(HeartbeatOutcome::Renewed { expires_at })
        }
        Err(StoreError::FencedOut { .. }) => Ok(HeartbeatOutcome::FencedOut),
        Err(other) => Err(other),
    }
}
