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
pub mod integrate;
pub mod quarantine;
pub mod reconcile;
pub mod tree;

pub use baseline::{
    Baseline, CommitRecord, FileChange, HookEntry, NestedRepo, Observed, RepoHealth, StatusEntry,
    capture_baseline, observe,
};
pub use clone::{Workspace, WorkspaceRequest, assert_registrable, create_workspace};
pub use descriptor::{DESCRIPTOR_FILENAME, RunDescriptor, read_descriptor};
pub use error::{GitError, GitResult};
pub use git::{GitOutput, run_git, run_git_ok};
pub use integrate::{
    Divergence, FetchedRef, MadeCommit, StagedTree, Trailer, Trailers, commit_exists,
    commit_staged, commit_workspace, fetch_run_branch, find_commit, ref_sha, stage_all,
    target_divergence,
};
pub use quarantine::{Orphan, OrphanReason, Quarantined, find_orphans, quarantine};
pub use reconcile::{
    AgentReport, Finding, FindingKind, Reconciliation, ReportClaim, Scope, SensitivePatterns,
    Verdict, VerificationOutcome, glob_match, reconcile,
};
pub use tree::{TreeHash, TreeHasher};
