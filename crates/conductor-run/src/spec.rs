//! Loading S5's minimal task-spec file.
//!
//! The types and the validation live in `conductor-core`, which has no I/O and
//! no YAML parser — §2.3 makes that crate "the one load-bearing boundary in the
//! crate layout". Reading the file is therefore here, next to the verification
//! profile, which is the other YAML file Conductor reads.
//!
//! **S11 deletes this.** The plan ledger replaces the whole file with
//! `.conductor/plans/vN/plan.yaml`, whose hashing rules (§3.6: canonical
//! reserialisation, comments excluded, stable IDs) are considerably more
//! demanding than anything here. Nothing in this module should grow.

use std::path::{Path, PathBuf};

use conductor_core::task::{TaskSpec, TaskSpecError, ValidatedTaskSpec};

/// Where the task spec lives by default, relative to the repository root.
pub const DEFAULT_SPEC_PATH: &str = ".conductor/task.yaml";

/// Why a task spec could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    /// The file could not be read.
    #[error("cannot read the task spec at {path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The file is not the YAML this expects.
    #[error("the task spec at {path} is not valid YAML: {source}")]
    Yaml {
        /// The path.
        path: PathBuf,
        /// Why.
        source: serde_yaml::Error,
    },
    /// The spec parsed and is unusable.
    #[error("the task spec at {path} is unusable: {source}")]
    Invalid {
        /// The path.
        path: PathBuf,
        /// Which rule it broke.
        source: TaskSpecError,
    },
}

/// The content hash of the spec bytes.
///
/// **Deliberately over the raw bytes, not over canonicalised content.** §3.6's
/// hash semantics — parse, reserialise canonically, exclude comments, so that
/// reformatting does not invalidate approval — belong to the plan ledger, and
/// implementing half of them here would create a hash that looks like a plan
/// hash and is not. This one is only ever used to say "this file is what the
/// `plan_version` placeholder row stands for", and it is never presented as a
/// plan hash: §3.4's `Conductor-Plan` trailer is **not** emitted at S5.
pub fn spec_content_hash(bytes: &[u8]) -> String {
    conductor_core::effect::content_hash(bytes)
}

/// Read and validate a task spec.
pub fn load(path: &Path) -> Result<(ValidatedTaskSpec, String), SpecError> {
    let bytes = std::fs::read(path).map_err(|source| SpecError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let spec: TaskSpec = serde_yaml::from_slice(&bytes).map_err(|source| SpecError::Yaml {
        path: path.to_path_buf(),
        source,
    })?;
    let validated = spec.validate().map_err(|source| SpecError::Invalid {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((validated, spec_content_hash(&bytes)))
}
