//! Store errors.

use std::path::PathBuf;

/// Anything the store can fail with.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite said no.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Filesystem operation around the store path failed.
    #[error("io at {path}: {source}")]
    Io {
        /// Path being operated on.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },

    /// The store file does not exist and this open path is not allowed to
    /// create it.
    #[error("no store at {0}")]
    NotFound(PathBuf),

    /// The home directory could not be determined, so the default store path
    /// cannot be computed.
    #[error("cannot determine the default store path: ${0} is not set")]
    NoHome(&'static str),

    /// A pragma did not read back as configured. Fails closed: a durability
    /// pragma that was silently dropped is exactly the failure ADR-0004 left
    /// open for S1.
    #[error("PRAGMA {pragma} reads back as {actual:?}, expected {expected:?}")]
    PragmaMismatch {
        /// Pragma name.
        pragma: &'static str,
        /// Expected readback.
        expected: &'static str,
        /// Observed readback.
        actual: String,
    },

    /// `PRAGMA integrity_check` did not return `ok` after a migration.
    #[error("integrity_check after migration {version} returned {report:?}")]
    IntegrityCheckFailed {
        /// Migration whose commit was followed by the failing check.
        version: i64,
        /// What `integrity_check` reported.
        report: Vec<String>,
    },

    /// The database was written by a newer binary. Forward-only migrations mean
    /// there is no way down.
    #[error("store schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version found in the database.
        found: i64,
        /// Highest version this binary knows.
        supported: i64,
    },

    /// The claim's `UPDATE … RETURNING` matched more than one row, which would
    /// mean two runs were claimed by one statement.
    #[error("claim matched {0} rows, expected at most 1")]
    ClaimMatchedMultipleRows(usize),

    /// A transaction failed and the rollback failed too. Both are reported: the
    /// rollback failure must not hide what went wrong first.
    #[error("rollback failed ({source}) while handling: {original}")]
    RollbackFailed {
        /// The error that caused the rollback, rendered.
        original: String,
        /// The rollback's own failure.
        source: rusqlite::Error,
    },

    /// A `TEXT` column held a value the domain does not recognise.
    #[error("invalid domain value read from the store: {0}")]
    Domain(String),

    /// Event payload serialisation failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<conductor_core::IdError> for StoreError {
    fn from(value: conductor_core::IdError) -> Self {
        StoreError::Domain(value.to_string())
    }
}

impl From<conductor_core::ParseStateError> for StoreError {
    fn from(value: conductor_core::ParseStateError) -> Self {
        StoreError::Domain(value.to_string())
    }
}

/// Result alias for store operations.
pub type StoreResult<T> = Result<T, StoreError>;
