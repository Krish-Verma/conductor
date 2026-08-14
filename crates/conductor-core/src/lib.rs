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

pub mod attempt;
pub mod containment;
pub mod effect;
pub mod event;
pub mod fence;
pub mod ids;
pub mod report;
pub mod state;

pub use attempt::{Attempt, AttemptState, TerminalAttempt};
pub use containment::{Enforcement, ExecutionCapabilities, GatingDimension, Informational};
pub use effect::{OperationId, Precondition, SideEffectKind, SideEffectState};
pub use event::{EventKind, RunClaimedPayload};
pub use fence::Fence;
pub use ids::{
    AttemptId, IdError, PlanVersionId, PolicyHash, ProjectId, RunId, TaskId, WorkspaceId,
};
pub use report::{AgentReport, ReportClaim};
pub use state::{
    AttemptOutcome, ParseStateError, PlanVersionState, ReconciledRoute, RunState, TaskState,
};
