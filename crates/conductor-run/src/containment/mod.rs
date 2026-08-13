//! Measured execution containment — master plan §4.2, ADR-0002.
//!
//! Conductor gates unattended sensitive work on what an execution mode actually
//! prevents. That value is **measured on this host and cached by version**,
//! never declared: a hardcoded table silently becomes a lie after a CLI upgrade.
//!
//! Two modules, per S2.5:
//!
//! - [`probe`] runs the suite against one (adapter × launcher) pair.
//! - [`cache`] stores the result under `(adapter, adapter_version, launcher,
//!   launcher_version, os_version)` and fails closed on anything else.
//!
//! **What S2.5 deliberately does not do:** it does not compare capabilities
//! against a requirement. That is the eligibility check, which is S7. This slice
//! only produces trustworthy input for it.

pub mod cache;
pub mod probe;

use std::path::PathBuf;

/// Anything the containment harness can fail with.
///
/// Note what is *not* here: "the sandbox denied the operation" is never an
/// error. A denial is the measurement. Errors are about the instrument.
#[derive(Debug, thiserror::Error)]
pub enum ContainmentError {
    /// The store refused a read or write.
    #[error("probe cache: {0}")]
    Store(#[from] conductor_store::StoreError),

    /// A filesystem operation setting up or tearing down the probe failed.
    #[error("probe io at {path}: {source}")]
    Io {
        /// Path being operated on.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },

    /// The probe cannot be set up in a way that would produce a trustworthy
    /// answer, so it refuses to produce one at all. ADR-0002's methodology
    /// record: S0's first round reported escapes that had not occurred because
    /// its "outside" directory was inside the permitted region.
    #[error("probe cannot be trusted: {0}")]
    Untrustworthy(String),
}

/// Result alias for the containment harness.
pub type ContainmentResult<T> = Result<T, ContainmentError>;
