//! Orphan workspaces — master plan §4.1, acceptance row 18.
//!
//! "Orphans found at startup are **quarantined**, never deleted — an orphan may
//! hold the only copy of an hour of work."
//!
//! There is deliberately no delete path in this module, not even behind a flag.
//! Retention and cleanup of *terminal* runs (`keep_workspaces_days`) is a
//! different decision about a different object: a workspace whose run reached a
//! terminal state and whose artifacts were captured. An orphan is by definition
//! a workspace nobody can account for, which is exactly when deleting is
//! unrecoverable.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use conductor_core::RunId;

use crate::descriptor::{RunDescriptor, now_unix_seconds, read_descriptor};
use crate::error::{GitError, GitResult};

/// Why a directory in the workspaces root is considered an orphan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanReason {
    /// It has a descriptor, and the run it names is not active.
    RunNotActive,
    /// It has no readable `.conductor-run.json`, so it cannot be accounted for
    /// at all. The most tempting case to delete, and the most dangerous.
    NoDescriptor,
}

/// A workspace directory nobody can account for.
#[derive(Debug, Clone)]
pub struct Orphan {
    /// Where it is.
    pub path: PathBuf,
    /// What it says about itself, when it says anything.
    pub descriptor: Option<RunDescriptor>,
    /// Why it is an orphan.
    pub reason: OrphanReason,
}

/// Where an orphan was moved to.
#[derive(Debug, Clone)]
pub struct Quarantined {
    /// Where it was.
    pub from: PathBuf,
    /// Where it is now.
    pub to: PathBuf,
}

/// Find workspaces in `workspaces_root` that no active run claims.
///
/// A missing root is not an error: on a first run there is nothing to scan.
pub fn find_orphans(workspaces_root: &Path, active: &BTreeSet<RunId>) -> GitResult<Vec<Orphan>> {
    let entries = match std::fs::read_dir(workspaces_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(GitError::Io {
                path: workspaces_root.to_path_buf(),
                source,
            });
        }
    };

    let mut orphans = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        match read_descriptor(&path) {
            Ok(descriptor) => {
                if !active.contains(&descriptor.run_id) {
                    orphans.push(Orphan {
                        path,
                        descriptor: Some(descriptor),
                        reason: OrphanReason::RunNotActive,
                    });
                }
            }
            Err(_) => orphans.push(Orphan {
                path,
                descriptor: None,
                reason: OrphanReason::NoDescriptor,
            }),
        }
    }
    orphans.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(orphans)
}

/// Move an orphan into the quarantine root.
///
/// A rename, never a copy-then-delete: a copy that fails halfway would leave
/// Conductor holding a partial duplicate and about to remove the original. If
/// the rename cannot be done, the orphan is left exactly where it was and the
/// failure is reported — failing to rescue is acceptable, deleting because the
/// rescue failed is not.
pub fn quarantine(orphan: &Orphan, quarantine_root: &Path) -> GitResult<Quarantined> {
    std::fs::create_dir_all(quarantine_root).map_err(|source| GitError::Io {
        path: quarantine_root.to_path_buf(),
        source,
    })?;

    let name = orphan
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    let stamp = now_unix_seconds();

    let mut destination = quarantine_root.join(format!("{stamp}-{name}"));
    let mut suffix = 2;
    while destination.exists() {
        destination = quarantine_root.join(format!("{stamp}-{name}-{suffix}"));
        suffix += 1;
    }

    std::fs::rename(&orphan.path, &destination).map_err(|source| GitError::Io {
        path: destination.clone(),
        source,
    })?;

    Ok(Quarantined {
        from: orphan.path.clone(),
        to: destination,
    })
}
