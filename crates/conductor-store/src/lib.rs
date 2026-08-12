//! Conductor's store: one SQLite database, one transaction domain.
//!
//! Concrete by design (master plan §2.5). There is no `Store` trait: splitting
//! the store behind an interface invites a write that spans two "stores" and
//! therefore two transactions, which is the bug class Part 5 exists to prevent.

pub mod claim;
pub mod error;
pub mod migrate;
pub mod schema;
pub mod tx;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

pub use claim::{ClaimedRun, claim_next_run};
pub use error::{StoreError, StoreResult};
pub use migrate::{MigrationStep, migrate};
pub use schema::PragmaReport;
pub use tx::with_immediate;

/// An open store.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open, creating the file and its parent directory if needed, and migrate
    /// forward. This is the only path that may create a database.
    pub fn open_or_create(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut conn = Connection::open(&path)?;
        schema::apply_pragmas(&conn)?;
        migrate::migrate(&mut conn)?;
        Ok(Store { conn, path })
    }

    /// Open an existing store. Never creates the database and never migrates —
    /// this is what `conductor doctor` uses, because reporting on a store must
    /// not bring one into existence.
    pub fn open_existing(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            return Err(StoreError::NotFound(path));
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        schema::apply_pragmas(&conn)?;
        Ok(Store { conn, path })
    }

    /// `~/.local/share/conductor/conductor.db` (master plan §3.1), honouring
    /// `XDG_DATA_HOME` when it is set.
    pub fn default_path() -> StoreResult<PathBuf> {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
            && !xdg.is_empty()
        {
            return Ok(PathBuf::from(xdg).join("conductor").join("conductor.db"));
        }
        let home = std::env::var_os("HOME").ok_or(StoreError::NoHome("HOME"))?;
        Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("conductor")
            .join("conductor.db"))
    }

    /// The path this store was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutably borrow the connection — required for transactions.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Pragma values actually in effect on this connection.
    pub fn pragmas(&self) -> StoreResult<PragmaReport> {
        schema::read_pragmas(&self.conn)
    }

    /// Highest applied schema version, `None` on an empty database.
    pub fn schema_version(&self) -> StoreResult<Option<i64>> {
        migrate::current_version(&self.conn)
    }

    /// `PRAGMA integrity_check`.
    pub fn integrity_check(&self) -> StoreResult<Vec<String>> {
        migrate::integrity_check(&self.conn)
    }

    /// Number of `PRAGMA foreign_key_check` violations.
    pub fn foreign_key_check(&self) -> StoreResult<usize> {
        let mut stmt = self.conn.prepare("PRAGMA foreign_key_check")?;
        let count = stmt.query_map([], |_| Ok(()))?.count();
        Ok(count)
    }

    /// Claim the next eligible run (§4.7).
    pub fn claim_next_run(
        &mut self,
        owner: &str,
        now_ms: i64,
        lease_ms: i64,
    ) -> StoreResult<Option<ClaimedRun>> {
        claim::claim_next_run(&mut self.conn, owner, now_ms, lease_ms)
    }
}
