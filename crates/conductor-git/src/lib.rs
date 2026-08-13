//! Per-run workspace isolation.
//!
//! Master plan §4.1 and ADR-0001. The boundary this crate implements is the one
//! that decides whether a misbehaving agent damages a scratch directory or the
//! operator's real repository.

pub mod baseline;
pub mod clone;
pub mod descriptor;
pub mod error;
pub mod git;
pub mod quarantine;
pub mod reconcile;

pub use baseline::{
    Baseline, CommitRecord, FileChange, HookEntry, NestedRepo, Observed, RepoHealth, StatusEntry,
    capture_baseline, observe,
};
pub use clone::{Workspace, WorkspaceRequest, assert_registrable, create_workspace};
pub use descriptor::{DESCRIPTOR_FILENAME, RunDescriptor, read_descriptor};
pub use error::{GitError, GitResult};
pub use quarantine::{Orphan, OrphanReason, Quarantined, find_orphans, quarantine};
pub use reconcile::{
    AgentReport, Finding, FindingKind, Reconciliation, ReportClaim, Scope, SensitivePatterns,
    Verdict, VerificationOutcome, reconcile,
};
