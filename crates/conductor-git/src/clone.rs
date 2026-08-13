//! Creating a run workspace — master plan §4.1, ADR-0001.
//!
//! The command sequence below is mandatory and has **no optimised variant**.
//! M4 measured `--no-hardlinks` as *faster* than the hardlinking default at
//! realistic repository size, so a "fast mode" would be both unsafe (M2) and
//! slower. There is nothing to trade off.

use std::path::{Path, PathBuf};

use conductor_core::{PolicyHash, RunId, TaskId};

use crate::baseline::{Baseline, capture_baseline};
use crate::descriptor::{RunDescriptor, exclude_descriptor_locally, now_unix_seconds};
use crate::error::{GitError, GitResult};
use crate::git::{run_git, run_git_ok};

/// What a caller must supply to create a workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceRequest {
    /// The operator's real repository. Never written to.
    pub source: PathBuf,
    /// Where the clone goes. Must not already exist.
    pub workspace: PathBuf,
    /// The run that will own it.
    pub run_id: RunId,
    /// The task being executed.
    pub task_id: TaskId,
    /// The commit the run is pinned to.
    pub base_commit: String,
    /// The policy snapshot in force.
    pub policy_hash: PolicyHash,
}

impl WorkspaceRequest {
    /// `conductor/<task-id>/<run-id>` (§4.1). Exists only in the run clone until
    /// Conductor fetches it; never pushed, never auto-merged.
    pub fn branch(&self) -> String {
        format!("conductor/{}/{}", self.task_id, self.run_id)
    }
}

/// A created workspace.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// The clone's root.
    pub path: PathBuf,
    /// The run branch that is checked out.
    pub branch: String,
    /// The descriptor written into it.
    pub descriptor: RunDescriptor,
    /// The §4.1 baseline, captured **after** the descriptor is written.
    ///
    /// The ordering is load-bearing, so it is a structural property of creation
    /// rather than a rule a caller has to remember: capture the baseline first
    /// and `.conductor-run.json` becomes a new untracked file at reconciliation,
    /// making Conductor raise a finding against itself on every run.
    pub baseline: Baseline,
}

/// Refuse a source repository v1 cannot reason about.
///
/// §4.1: "Non-empty `git submodule status` at registration is a hard error."
/// Hard, not a finding: a submodule's content lives in a repository Conductor
/// does not clone, does not baseline and cannot reconcile, so every downstream
/// guarantee would be silently narrower than it claims to be.
pub fn assert_registrable(source: &Path) -> GitResult<()> {
    if !source.exists() {
        return Err(GitError::NotARepository(source.to_path_buf()));
    }
    let probe = run_git(source, &["rev-parse", "--git-dir"])?;
    if !probe.ok() {
        return Err(GitError::NotARepository(source.to_path_buf()));
    }

    let submodules = run_git_ok(source, &["submodule", "status"])?;
    let status = submodules.stdout_lossy().trim().to_string();
    if !status.is_empty() {
        return Err(GitError::SubmodulesUnsupported(
            source.to_path_buf(),
            status,
        ));
    }
    Ok(())
}

/// Create a per-run workspace.
///
/// On any failure after the clone begins, the partial workspace is removed: a
/// half-built workspace on disk is indistinguishable from an orphan holding an
/// hour of real work, and §4.1 says orphans are quarantined rather than deleted.
/// Leaving debris would therefore turn a clean failure into a permanent one.
pub fn create_workspace(request: &WorkspaceRequest) -> GitResult<Workspace> {
    assert_registrable(&request.source)?;

    if request.workspace.exists() {
        return Err(GitError::WorkspaceExists(request.workspace.clone()));
    }
    let parent = request
        .workspace
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|source| GitError::Io {
        path: parent.clone(),
        source,
    })?;

    match build(request, &parent) {
        Ok(workspace) => Ok(workspace),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&request.workspace);
            Err(error)
        }
    }
}

fn build(request: &WorkspaceRequest, parent: &Path) -> GitResult<Workspace> {
    let source = request.source.to_string_lossy().to_string();
    let workspace = request.workspace.to_string_lossy().to_string();
    let branch = request.branch();

    // §4.1, verbatim. `--no-hardlinks` is the isolation boundary (M1–M3);
    // `--no-checkout` is what lets the branch be created at the base commit
    // rather than at whatever the source's HEAD happens to be.
    run_git_ok(
        parent,
        &[
            "clone",
            "--no-hardlinks",
            "--no-checkout",
            &source,
            &workspace,
        ],
    )?;

    let ws = request.workspace.as_path();
    run_git_ok(ws, &["checkout", "-b", &branch, &request.base_commit])?;
    run_git_ok(ws, &["remote", "remove", "origin"])?;
    run_git_ok(ws, &["config", "core.hooksPath", "/dev/null"])?;
    run_git_ok(ws, &["config", "user.name", "Conductor Agent"])?;
    run_git_ok(ws, &["config", "user.email", "conductor@localhost"])?;
    run_git_ok(ws, &["config", "commit.gpgsign", "false"])?;

    let git_dir = run_git_ok(ws, &["rev-parse", "--absolute-git-dir"])?.stdout_trimmed();
    exclude_descriptor_locally(Path::new(&git_dir))?;

    let descriptor = RunDescriptor {
        run_id: request.run_id.clone(),
        task_id: request.task_id.clone(),
        base_commit: request.base_commit.clone(),
        policy_hash: request.policy_hash.clone(),
        created_at: now_unix_seconds(),
    };
    descriptor.write_to(ws)?;
    let baseline = capture_baseline(ws)?;

    Ok(Workspace {
        path: request.workspace.clone(),
        branch,
        descriptor,
        baseline,
    })
}
