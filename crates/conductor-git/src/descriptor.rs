//! `.conductor-run.json` — the workspace descriptor.
//!
//! Master plan §4.1: "This is what makes recovery possible with **no database at
//! all**." That sentence is the design constraint. The descriptor must be
//! self-sufficient — parseable from the file alone, with no store, no clone and
//! no schema version to look up — and it must never fail to parse because a
//! newer Conductor added a field (§2.2: never `deny_unknown_fields`).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use conductor_core::{PolicyHash, RunId, TaskId};
use serde::{Deserialize, Serialize};

use crate::error::{GitError, GitResult};

/// The descriptor's filename inside every workspace.
pub const DESCRIPTOR_FILENAME: &str = ".conductor-run.json";

/// What a workspace says about itself.
///
/// No `deny_unknown_fields`: an older binary reading a newer descriptor is
/// exactly the recovery case this file exists for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDescriptor {
    /// The run that owns this workspace.
    pub run_id: RunId,
    /// The task the run is executing.
    pub task_id: TaskId,
    /// The commit the workspace was cloned at.
    pub base_commit: String,
    /// `policy_snapshot.hash` in force for the run.
    pub policy_hash: PolicyHash,
    /// Unix seconds at creation.
    pub created_at: i64,
}

impl RunDescriptor {
    /// Where the descriptor lives inside a workspace.
    pub fn path_in(workspace: &Path) -> std::path::PathBuf {
        workspace.join(DESCRIPTOR_FILENAME)
    }

    /// Write the descriptor into a workspace.
    pub fn write_to(&self, workspace: &Path) -> GitResult<()> {
        let path = Self::path_in(workspace);
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        std::fs::write(&path, json).map_err(|source| GitError::Io { path, source })
    }
}

/// Make the descriptor invisible to `git add`, locally and only in this clone.
///
/// `.git/info/exclude` rather than a `.gitignore`: a `.gitignore` would itself
/// be a tracked change on the run branch, and the run branch is what Conductor
/// later fetches into the operator's real repository. `git add -A` is the single
/// most likely thing an agent does, so leaving the descriptor merely untracked
/// puts Conductor's own bookkeeping into the user's history.
pub fn exclude_descriptor_locally(git_dir: &Path) -> GitResult<()> {
    let info = git_dir.join("info");
    std::fs::create_dir_all(&info).map_err(|source| GitError::Io {
        path: info.clone(),
        source,
    })?;
    let exclude = info.join("exclude");
    let mut contents = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str("# Conductor's own run descriptor. Never part of the run branch.\n");
    contents.push('/');
    contents.push_str(DESCRIPTOR_FILENAME);
    contents.push('\n');
    std::fs::write(&exclude, contents).map_err(|source| GitError::Io {
        path: exclude,
        source,
    })
}

/// Unix seconds now. Clamped at 0 rather than failing: a clock before the epoch
/// is not a reason to refuse to record a run.
pub fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the descriptor out of a workspace directory.
pub fn read_descriptor(workspace: &Path) -> GitResult<RunDescriptor> {
    let path = RunDescriptor::path_in(workspace);
    let raw = std::fs::read_to_string(&path).map_err(|source| GitError::Descriptor {
        path: path.clone(),
        reason: format!("{DESCRIPTOR_FILENAME} could not be read: {source}"),
    })?;
    serde_json::from_str(&raw).map_err(|source| GitError::Descriptor {
        path,
        reason: format!("{DESCRIPTOR_FILENAME} is unusable: {source}"),
    })
}
