//! Forward-only migrations.
//!
//! Rules, in order of importance: apply in ascending version order · each
//! migration runs inside one `BEGIN IMMEDIATE` transaction · `PRAGMA
//! integrity_check` must return `ok` after each applied migration · running
//! `migrate` twice is a no-op, not an error · a database from the future is a
//! hard error, because there is no down migration.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};
use crate::schema;
use crate::tx::with_immediate;

/// One forward-only migration.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Ascending, gapless, never reused.
    pub version: i64,
    /// Human name, recorded in reports rather than in the database.
    pub name: &'static str,
    /// DDL, applied as a single batch inside one transaction.
    pub sql: &'static str,
}

/// Every migration known to this binary, in ascending version order.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "schema_v1",
        sql: schema::SCHEMA_V1,
    },
    Migration {
        version: 2,
        name: "attempt_state",
        sql: schema::SCHEMA_V2,
    },
    Migration {
        version: 3,
        name: "artifact_content_hash",
        sql: schema::SCHEMA_V3,
    },
    Migration {
        version: 4,
        name: "run_target_branch",
        sql: schema::SCHEMA_V4,
    },
    Migration {
        version: 5,
        name: "repair_observation",
        sql: schema::SCHEMA_V5,
    },
];

/// What [`migrate`] did about one migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationStep {
    /// Migration version.
    pub version: i64,
    /// Migration name.
    pub name: String,
    /// False when the migration was already present in `schema_version`.
    pub applied: bool,
    /// `PRAGMA integrity_check` output, taken after the commit. `None` when the
    /// migration was skipped.
    pub integrity_check: Option<Vec<String>>,
}

/// The highest applied version, or `None` when `schema_version` does not exist
/// yet (an empty database).
pub fn current_version(conn: &Connection) -> StoreResult<Option<i64>> {
    // `schema_version` is created by migration 1, so its absence is the normal
    // state of a fresh database, not an error.
    let table_present: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |row| row.get(0),
    )?;
    if table_present == 0 {
        return Ok(None);
    }
    let version: Option<i64> =
        conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
    Ok(version)
}

/// Migrations not yet applied to this database.
pub fn pending(conn: &Connection) -> StoreResult<Vec<&'static Migration>> {
    let current = current_version(conn)?;
    reject_future_schema(current)?;
    // `None < Some(_)`, so this is also the "empty database" case.
    Ok(MIGRATIONS
        .iter()
        .filter(|m| Some(m.version) > current)
        .collect())
}

/// Apply every pending migration in order. Returns one step per known
/// migration, applied or skipped.
pub fn migrate(conn: &mut Connection) -> StoreResult<Vec<MigrationStep>> {
    apply_up_to(conn, i64::MAX)
}

/// Apply pending migrations up to and including `max_version`.
///
/// Exists so that a test can build a database at an *older* schema and then
/// migrate it forward — the only way to check that a forward migration actually
/// works on data written before it, rather than only on a database that was
/// created with the migration already applied.
pub fn apply_up_to(conn: &mut Connection, max_version: i64) -> StoreResult<Vec<MigrationStep>> {
    let current = current_version(conn)?;
    reject_future_schema(current)?;

    let mut steps = Vec::with_capacity(MIGRATIONS.len());
    for migration in MIGRATIONS.iter().filter(|m| m.version <= max_version) {
        if Some(migration.version) <= current {
            steps.push(MigrationStep {
                version: migration.version,
                name: migration.name.to_string(),
                applied: false,
                integrity_check: None,
            });
            continue;
        }

        let applied_at = now_ms();
        with_immediate(conn, |tx| {
            tx.execute_batch(migration.sql)?;
            tx.execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                params![migration.version, applied_at],
            )?;
            Ok(())
        })?;

        // After the commit, not inside it: integrity_check reports on what is
        // durably in the file.
        let report = integrity_check(conn)?;
        if report != ["ok"] {
            return Err(StoreError::IntegrityCheckFailed {
                version: migration.version,
                report,
            });
        }
        steps.push(MigrationStep {
            version: migration.version,
            name: migration.name.to_string(),
            applied: true,
            integrity_check: Some(report),
        });
    }
    Ok(steps)
}

/// `PRAGMA integrity_check`, as reported rows.
pub fn integrity_check(conn: &Connection) -> StoreResult<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<String>, rusqlite::Error>>()?;
    Ok(rows)
}

/// Migrations are forward-only: there is no way back down from a database
/// written by a newer binary, so refuse to touch it.
fn reject_future_schema(current: Option<i64>) -> StoreResult<()> {
    match current {
        Some(found) if found > schema::SUPPORTED_SCHEMA_VERSION => Err(StoreError::SchemaTooNew {
            found,
            supported: schema::SUPPORTED_SCHEMA_VERSION,
        }),
        _ => Ok(()),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
