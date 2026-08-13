//! The probe cache — `containment_probe`, master plan Part 5.1.
//!
//! One rule, and it is the whole point of the table (§4.2):
//!
//! > A stale or absent probe forces every dimension to `None` — fail closed.
//!
//! "Stale" here is not a clock. It is the version triple: a row is usable only
//! when the adapter, the launcher and the OS are all still exactly what they
//! were when the measurement was taken. No TTL is applied, because a TTL would
//! be an invented policy — the plan keys the cache on versions, and versions are
//! what change sandbox behaviour.

use conductor_core::containment::ExecutionCapabilities;
use conductor_store::with_immediate;
use rusqlite::{Connection, OptionalExtension, params};

use super::{ContainmentError, ContainmentResult};

/// The cache key: master plan Part 5.1's `UNIQUE(adapter, adapter_version,
/// launcher, launcher_version, os_version)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ProbeKey {
    /// Adapter name, e.g. `codex`.
    pub adapter: String,
    /// Adapter version string, exactly as the binary reported it.
    pub adapter_version: String,
    /// Launcher name, e.g. `codex-sandbox`, or `none`.
    pub launcher: String,
    /// Launcher version string, or `n/a` when there is no launcher.
    pub launcher_version: String,
    /// Host OS version, including the build — a build bump can change seatbelt.
    pub os_version: String,
}

impl ProbeKey {
    /// Build a key from its five components.
    pub fn new(
        adapter: impl Into<String>,
        adapter_version: impl Into<String>,
        launcher: impl Into<String>,
        launcher_version: impl Into<String>,
        os_version: impl Into<String>,
    ) -> Self {
        ProbeKey {
            adapter: adapter.into(),
            adapter_version: adapter_version.into(),
            launcher: launcher.into(),
            launcher_version: launcher_version.into(),
            os_version: os_version.into(),
        }
    }

    /// `containment_probe.id`. Derived from the key as a JSON array so that the
    /// primary key and the `UNIQUE` constraint cannot disagree, and so that no
    /// separator can collide with a version string that contains one.
    pub fn row_id(&self) -> String {
        serde_json::to_string(&[
            self.adapter.as_str(),
            self.adapter_version.as_str(),
            self.launcher.as_str(),
            self.launcher_version.as_str(),
            self.os_version.as_str(),
        ])
        .expect("a fixed array of strings always serializes")
    }
}

/// What the cache had to say about one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    /// An exact match on the version triple.
    Hit {
        /// What was measured.
        capabilities: ExecutionCapabilities,
        /// When, in milliseconds since the epoch.
        probed_at_ms: i64,
    },
    /// No row for this exact key. Either never probed, or probed before an
    /// upgrade — indistinguishable, and treated identically.
    Miss,
    /// A row exists but Conductor cannot read it.
    Unusable {
        /// Why not.
        reason: String,
    },
}

impl CacheLookup {
    /// The capabilities this lookup authorizes Conductor to assume.
    ///
    /// **Only a hit yields a measurement.** A miss and an unreadable row both
    /// yield [`ExecutionCapabilities::fail_closed`], because "we do not know"
    /// and "we know nothing is enforced" must lead to the same refusal.
    pub fn capabilities(&self) -> ExecutionCapabilities {
        match self {
            CacheLookup::Hit { capabilities, .. } => capabilities.clone(),
            CacheLookup::Miss | CacheLookup::Unusable { .. } => {
                ExecutionCapabilities::fail_closed()
            }
        }
    }

    /// Whether this lookup produced a usable measurement.
    pub fn is_hit(&self) -> bool {
        matches!(self, CacheLookup::Hit { .. })
    }
}

/// Look up one key. Only an exact match on all five components hits.
pub fn lookup(conn: &Connection, key: &ProbeKey) -> ContainmentResult<CacheLookup> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT capabilities, probed_at FROM containment_probe
              WHERE adapter = ?1 AND adapter_version = ?2
                AND launcher = ?3 AND launcher_version = ?4
                AND os_version = ?5",
            params![
                key.adapter,
                key.adapter_version,
                key.launcher,
                key.launcher_version,
                key.os_version
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|err| ContainmentError::Store(err.into()))?;

    let Some((json, probed_at_ms)) = row else {
        return Ok(CacheLookup::Miss);
    };

    match serde_json::from_str::<ExecutionCapabilities>(&json) {
        Ok(capabilities) => Ok(CacheLookup::Hit {
            capabilities,
            probed_at_ms,
        }),
        // A row written by a binary with a different capability model is not a
        // crash and is certainly not a permission.
        Err(err) => Ok(CacheLookup::Unusable {
            reason: err.to_string(),
        }),
    }
}

/// Record a measurement, replacing any previous one for the same key.
pub fn upsert(
    conn: &mut Connection,
    key: &ProbeKey,
    capabilities: &ExecutionCapabilities,
    probed_at_ms: i64,
) -> ContainmentResult<()> {
    let json = serde_json::to_string(capabilities)
        .map_err(|err| ContainmentError::Store(conductor_store::StoreError::Json(err)))?;

    with_immediate(conn, |tx| {
        tx.execute(
            "INSERT INTO containment_probe
                 (id, adapter, adapter_version, launcher, launcher_version,
                  os_version, capabilities, probed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(adapter, adapter_version, launcher, launcher_version, os_version)
             DO UPDATE SET capabilities = excluded.capabilities,
                           probed_at    = excluded.probed_at",
            params![
                key.row_id(),
                key.adapter,
                key.adapter_version,
                key.launcher,
                key.launcher_version,
                key.os_version,
                json,
                probed_at_ms
            ],
        )?;
        Ok(())
    })?;
    Ok(())
}
