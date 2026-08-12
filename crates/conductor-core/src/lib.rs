//! Pure domain types for Conductor.
//!
//! **No I/O.** This crate is the one load-bearing boundary in the crate layout
//! (master plan §2.3): it must stay testable without a runtime, a database or a
//! filesystem.
//!
//! S1 scope: only what the store actually persists in S1 — strongly-typed
//! identifiers, the state enums written to `TEXT` columns, and the payload of
//! the one event kind S1 emits. State-transition validation, decision functions
//! and reconciliation belong to S3/S5 and are deliberately absent.

pub mod event;
pub mod ids;
pub mod state;

pub use event::{EventKind, RunClaimedPayload};
pub use ids::{
    AttemptId, IdError, PlanVersionId, PolicyHash, ProjectId, RunId, TaskId, WorkspaceId,
};
pub use state::{AttemptOutcome, ParseStateError, PlanVersionState, RunState, TaskState};
