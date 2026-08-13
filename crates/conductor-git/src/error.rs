//! Errors this crate can fail with.
//!
//! Note what is deliberately *not* an error: a repository that is broken,
//! locked, detached or mid-merge. Those are reconciliation input (§4.8
//! `CORRUPT`), not failures — a workspace that cannot be classified is worse
//! than one classified as damaged.

use std::path::PathBuf;

/// Anything workspace isolation can fail with.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The `git` binary could not be spawned at all. Distinct from a git
    /// command that ran and failed: this one means the host is unusable.
    #[error("cannot run git: {source}")]
    GitUnavailable {
        /// Underlying spawn error.
        source: std::io::Error,
    },

    /// A git command ran and exited non-zero on a path where success was
    /// required.
    #[error("git {args} exited {status}: {stderr}")]
    Command {
        /// The arguments passed, space-joined, for diagnosis.
        args: String,
        /// Exit code, or `signal` when the process was killed.
        status: String,
        /// Truncated stderr.
        stderr: String,
    },

    /// A filesystem operation failed.
    #[error("io at {path}: {source}")]
    Io {
        /// Path being operated on.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },

    /// The source path is not a git repository, so there is nothing to clone.
    #[error("{0} is not a git repository")]
    NotARepository(PathBuf),

    /// v1 refuses submodules (§4.1). A hard error at registration, not a
    /// finding: Conductor cannot reason about a repository whose content lives
    /// in repositories it does not manage.
    #[error("submodules are not supported in v1; {0} reports: {1}")]
    SubmodulesUnsupported(PathBuf, String),

    /// The workspace path already exists. Reusing a directory would make the
    /// baseline a lie about what was there before the run.
    #[error("workspace path already exists: {0}")]
    WorkspaceExists(PathBuf),

    /// The workspace has no `.conductor-run.json`, or it could not be parsed.
    /// §4.1 makes the descriptor the thing that permits recovery with no
    /// database at all, so a missing one is reported, never guessed around.
    #[error("workspace descriptor at {path} is unusable: {reason}")]
    Descriptor {
        /// Descriptor path.
        path: PathBuf,
        /// Why it is unusable.
        reason: String,
    },

    /// JSON serialisation failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result alias for workspace operations.
pub type GitResult<T> = Result<T, GitError>;
