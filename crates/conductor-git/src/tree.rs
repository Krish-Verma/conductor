//! Hashing the **working tree**, not `HEAD`'s tree.
//!
//! [`Baseline::tree_hash`](crate::Baseline::tree_hash) is `HEAD^{tree}` — what
//! was committed. Verification needs a different identity. An agent's edits are
//! uncommitted, and the mutation §4.5 exists to catch ("an agent or a stray
//! watcher modifying files mid-test") changes the working tree without touching
//! `HEAD` at all. A check bound to `HEAD^{tree}` would be bound to a tree that
//! nothing observed.
//!
//! # The mechanism, and why each part of it is load-bearing
//!
//! ```text
//! GIT_INDEX_FILE=<scratch>  git add -A
//! GIT_INDEX_FILE=<scratch>  git write-tree
//! ```
//!
//! **A scratch index, never the repository's own.** `git add` against the
//! workspace's index would stage the agent's work as a side effect of
//! *observing* it, and §4.8 reconciles staged against unstaged. An observation
//! must not move what it observes.
//!
//! **The scratch index must live outside the working tree.** An index file
//! written inside the workspace is an untracked file, so it would enter the
//! next `add -A` and change the hash it is being used to compute. Refused in
//! [`TreeHasher::new`].
//!
//! **`git add -A` honours `.gitignore`, and that is the point.** Nearly every
//! real check writes to an ignored path — `cargo` fills `target/`, `npm` fills
//! `node_modules/`. If ignored paths counted, *every* check would void itself
//! the moment it did any work, and §4.5's `VOID` outcome would fire constantly
//! while detecting nothing. Excluding them is what makes "the source tree moved
//! under this check" a statement with content.
//!
//! **Untracked-but-not-ignored files do count**, which is the case that
//! matters: a file that appears mid-check is exactly the mutation being sought.
//!
//! Cost is one `git add` per hash — 30–190 ms on a repository the size of
//! Conductor's own — and two hashes per check.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{GitError, GitResult};

/// The identity of a working tree at one instant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TreeHash(String);

impl TreeHash {
    /// The git object id, as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Rebuild from a value the database or a descriptor already holds.
    ///
    /// Named for where the value comes from on purpose: there is no general
    /// constructor, so a caller cannot invent a tree identity that
    /// [`TreeHasher::hash`] would never have produced.
    pub fn from_stored(stored: impl Into<String>) -> Self {
        TreeHash(stored.into())
    }
}

impl std::fmt::Display for TreeHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Computes working-tree hashes for one workspace.
#[derive(Debug, Clone)]
pub struct TreeHasher {
    workspace: PathBuf,
    scratch_index: PathBuf,
}

impl TreeHasher {
    /// Build a hasher for `workspace` that keeps its index at `scratch_index`.
    ///
    /// Fails when the scratch index would live inside the workspace.
    pub fn new(workspace: &Path, scratch_index: &Path) -> GitResult<TreeHasher> {
        let workspace = canonical(workspace)?;

        let parent = scratch_index.parent().ok_or_else(|| GitError::Io {
            path: scratch_index.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the scratch index needs a parent directory",
            ),
        })?;
        std::fs::create_dir_all(parent).map_err(|source| GitError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        // The index file does not exist yet, so only its directory can be
        // canonicalised — which is what containment is about anyway.
        let parent = canonical(parent)?;
        if parent.starts_with(&workspace) {
            return Err(GitError::Io {
                path: scratch_index.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "a scratch index inside the workspace {} would change \
                         the tree it is used to measure",
                        workspace.display()
                    ),
                ),
            });
        }

        let file_name = scratch_index
            .file_name()
            .ok_or_else(|| GitError::Io {
                path: scratch_index.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the scratch index needs a file name",
                ),
            })?
            .to_owned();

        Ok(TreeHasher {
            workspace,
            scratch_index: parent.join(file_name),
        })
    }

    /// The workspace being hashed.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Where the scratch index lives.
    pub fn scratch_index(&self) -> &Path {
        &self.scratch_index
    }

    /// Hash the working tree as it stands right now.
    pub fn hash(&self) -> GitResult<TreeHash> {
        // A fresh index every time. A reused index carries stat information
        // from the previous call, and "fast because it trusted what it saw last
        // time" is not a property a mutation detector may have.
        match std::fs::remove_file(&self.scratch_index) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GitError::Io {
                    path: self.scratch_index.clone(),
                    source,
                });
            }
        }
        self.git(&["add", "-A"])?;
        Ok(TreeHash(self.git(&["write-tree"])?))
    }

    fn git(&self, args: &[&str]) -> GitResult<String> {
        // Not `crate::git::run_git`: this is the one git invocation in
        // Conductor that must carry `GIT_INDEX_FILE`, and a general env
        // parameter on `run_git` would be an invitation to scrub the
        // environment elsewhere — which §2.2 forbids, because Conductor must
        // observe the repository the operator observes.
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.workspace)
            .env("GIT_INDEX_FILE", &self.scratch_index)
            .output()
            .map_err(|source| GitError::GitUnavailable { source })?;
        if !output.status.success() {
            return Err(GitError::Command {
                args: args.join(" "),
                status: match output.status.code() {
                    Some(code) => code.to_string(),
                    None => "by signal".to_string(),
                },
                stderr: String::from_utf8_lossy(&output.stderr)
                    .trim_end()
                    .to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string())
    }
}

fn canonical(path: &Path) -> GitResult<PathBuf> {
    path.canonicalize().map_err(|source| GitError::Io {
        path: path.to_path_buf(),
        source,
    })
}
