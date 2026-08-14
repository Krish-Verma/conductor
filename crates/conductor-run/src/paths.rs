//! Unique ownership of generated paths — carried forward from S0.
//!
//! S0's completion report: "**The subagent's primary result file was clobbered**
//! by my own concurrent re-run with different parameters. Regenerated at
//! canonical fidelity and metadata verified. Shared output directories need
//! provenance checks when more than one agent writes to them."
//!
//! S3 is where that stops being a lesson about running experiments and becomes a
//! property of the product. Workers claim runs concurrently; a re-claimed run
//! re-derives the same artifact path as the worker that died holding it; startup
//! recovery walks directories it did not create. Two independent writers
//! computing the same path is the normal case, not an accident.
//!
//! Two rules, and neither is a convention:
//!
//! 1. **Exclusive creation, never check-then-write.** `mkdir(2)` and `O_EXCL`
//!    decide the winner in the kernel. A `path.exists()` guard followed by a
//!    write is the same race spelled out over two lines.
//! 2. **Every generated directory carries provenance.** The refusal names the
//!    worker that actually holds the path, so "somebody else has it" is a fact
//!    with a name attached rather than an `EEXIST` to be puzzled over.
//!
//! Paths are **deterministic** — `<root>/<run-id>/<ordinal>/` — because recovery
//! has to find the artifacts of an attempt it did not start. A timestamp or a
//! random suffix would make each restart generate a fresh directory and lose
//! the previous one, which is the same data loss by a different route.

use std::path::{Path, PathBuf};

use conductor_core::RunId;
use serde::{Deserialize, Serialize};

/// The provenance file dropped into every generated directory.
pub const PROVENANCE_FILENAME: &str = ".conductor-owner.json";

/// Who generated a path.
///
/// No `deny_unknown_fields`: an older binary reading a newer record must still
/// be able to see who owns the directory, because that is exactly the moment it
/// most needs to know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The worker identity — the same string as `run.lease_owner`.
    pub worker: String,
    /// The run the directory belongs to.
    pub run_id: RunId,
    /// `attempt.ordinal`.
    pub attempt_ordinal: i64,
    /// The owning process's pid, for the case where the worker name repeats.
    pub pid: i32,
    /// Unix milliseconds at creation.
    pub created_at: i64,
}

/// A worker's identity, as it appears in provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// The worker name.
    pub worker: String,
    /// The worker's pid.
    pub pid: i32,
}

impl Owner {
    /// A named worker running as `pid`.
    pub fn new(worker: impl Into<String>, pid: i32) -> Self {
        Owner {
            worker: worker.into(),
            pid,
        }
    }
}

/// What can go wrong claiming a path.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    /// Another worker already owns this directory.
    #[error("{path} is already owned by {} (pid {})", by.worker, by.pid)]
    AlreadyOwned {
        /// The contested directory.
        path: PathBuf,
        /// Who holds it.
        by: Provenance,
    },

    /// The directory exists but has no provenance, so nobody can say who owns it.
    ///
    /// Refused rather than adopted: a directory Conductor cannot account for is
    /// the same situation as an orphan workspace, and §4.1's answer there is to
    /// preserve it, never to write into it.
    #[error("{0} exists but carries no provenance, so it cannot be safely claimed")]
    Unattributed(PathBuf),

    /// A file inside an owned directory already exists.
    #[error("{0} already exists and generated artifacts are never overwritten")]
    AlreadyExists(PathBuf),

    /// Underlying filesystem failure.
    #[error("io at {path}: {source}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },

    /// The provenance record could not be read or written.
    #[error("provenance at {path} is unusable: {detail}")]
    BadProvenance {
        /// Where the record is.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },
}

/// A directory this worker owns exclusively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedDir {
    path: PathBuf,
    provenance: Provenance,
}

impl OwnedDir {
    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Who owns it.
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// Create a file that must not already exist.
    ///
    /// `create_new` rather than a truncating write: an artifact that already
    /// exists was written by somebody, and silently replacing it is the S0
    /// failure at file granularity.
    pub fn write_new(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, OwnershipError> {
        use std::io::Write;

        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| OwnershipError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(OwnershipError::AlreadyExists(path));
            }
            Err(source) => return Err(OwnershipError::Io { path, source }),
        };
        file.write_all(bytes).map_err(|source| OwnershipError::Io {
            path: path.clone(),
            source,
        })?;
        // Durability matters here in a narrow way: this is an artifact whose
        // existence a side-effect receipt is about to assert (§4.7). A receipt
        // that outlives its file would make the ledger lie.
        file.sync_all().map_err(|source| OwnershipError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }
}

/// The artifacts root — `~/.local/share/conductor/artifacts/` in production
/// (§3.1), a temporary directory in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRoot {
    root: PathBuf,
}

impl ArtifactRoot {
    /// An artifacts tree rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        ArtifactRoot { root: root.into() }
    }

    /// The root directory.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Where one attempt's artifacts live. Deterministic.
    pub fn attempt_dir(&self, run_id: &RunId, attempt_ordinal: i64) -> PathBuf {
        self.root
            .join(run_id.as_str())
            .join(attempt_ordinal.to_string())
    }

    /// Claim an attempt directory exclusively.
    ///
    /// Fails if anybody — including this same worker — already holds it. Use
    /// [`ArtifactRoot::reclaim_attempt_dir`] for the recovery case, where
    /// re-entering one's own directory is legitimate.
    pub fn claim_attempt_dir(
        &self,
        run_id: &RunId,
        attempt_ordinal: i64,
        owner: &Owner,
    ) -> Result<OwnedDir, OwnershipError> {
        let path = self.attempt_dir(run_id, attempt_ordinal);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| OwnershipError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        // `create_dir` is not `create_dir_all`: it fails with AlreadyExists,
        // and that failure is the mutual exclusion. The kernel picks the winner.
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(self.contested(&path));
            }
            Err(source) => return Err(OwnershipError::Io { path, source }),
        }

        let provenance = Provenance {
            worker: owner.worker.clone(),
            run_id: run_id.clone(),
            attempt_ordinal,
            pid: owner.pid,
            created_at: now_ms(),
        };
        write_provenance(&path, &provenance)?;
        Ok(OwnedDir { path, provenance })
    }

    /// Claim an attempt directory, tolerating the case where **this same
    /// worker** already owns it.
    ///
    /// Recovery re-enters the directory of the attempt it is recovering. A
    /// different worker is still refused: adopting somebody else's directory is
    /// the clobbering this module exists to prevent.
    pub fn reclaim_attempt_dir(
        &self,
        run_id: &RunId,
        attempt_ordinal: i64,
        owner: &Owner,
    ) -> Result<OwnedDir, OwnershipError> {
        match self.claim_attempt_dir(run_id, attempt_ordinal, owner) {
            Ok(owned) => Ok(owned),
            Err(OwnershipError::AlreadyOwned { path, by }) if by.worker == owner.worker => {
                Ok(OwnedDir {
                    path,
                    provenance: by,
                })
            }
            Err(other) => Err(other),
        }
    }

    /// Read a directory's provenance, if it has any.
    pub fn read_provenance(&self, dir: &Path) -> Result<Option<Provenance>, OwnershipError> {
        let path = dir.join(PROVENANCE_FILENAME);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).map(Some).map_err(|source| {
                OwnershipError::BadProvenance {
                    path,
                    detail: source.to_string(),
                }
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(OwnershipError::Io { path, source }),
        }
    }

    fn contested(&self, path: &Path) -> OwnershipError {
        match self.read_provenance(path) {
            Ok(Some(by)) => OwnershipError::AlreadyOwned {
                path: path.to_path_buf(),
                by,
            },
            Ok(None) => OwnershipError::Unattributed(path.to_path_buf()),
            Err(error) => error,
        }
    }
}

fn write_provenance(dir: &Path, provenance: &Provenance) -> Result<(), OwnershipError> {
    let path = dir.join(PROVENANCE_FILENAME);
    let json = serde_json::to_string_pretty(provenance).map_err(|source| {
        OwnershipError::BadProvenance {
            path: path.clone(),
            detail: source.to_string(),
        }
    })?;
    std::fs::write(&path, json).map_err(|source| OwnershipError::Io { path, source })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
