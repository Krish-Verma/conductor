//! Shared fixtures for the store's integration tests.
#![allow(dead_code)] // one copy is compiled into each test binary; each uses a subset

use conductor_store::{Store, StoreResult, with_immediate};
use rusqlite::params;
use tempfile::TempDir;

/// `policy_snapshot.hash` every seeded run references.
pub const POLICY_HASH: &str =
    "blake3:0000000000000000000000000000000000000000000000000000000000000000";

/// A store in a fresh temporary directory. The `TempDir` must be kept alive for
/// the lifetime of the store.
pub fn temp_store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open_or_create(dir.path().join("conductor.db")).expect("open store");
    (dir, store)
}

/// Seed the parent rows a `run` needs: one project, one plan version, one
/// policy snapshot. Idempotent.
pub fn seed_parents(store: &mut Store) -> StoreResult<()> {
    with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT OR IGNORE INTO project
               (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES ('p-fixture', '/fixture/repo', 'blake3:repo', 'main', 'blake3:cfg', 0)",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO plan_version
               (id, project_id, version, content_hash, state, source_path)
             VALUES ('pv-1', 'p-fixture', 1, 'blake3:plan', 'APPROVED',
                     '.conductor/plans/v1/plan.yaml')",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO policy_snapshot (hash, canonical_blob, created_at)
             VALUES (?1, '{}', 0)",
            params![POLICY_HASH],
        )?;
        Ok(())
    })
}

/// One `run` row to seed, with the fields the claim's selection subquery reads.
#[derive(Debug, Clone)]
pub struct SeedRun {
    /// `run.id`; the task is derived from it.
    pub id: String,
    /// `run.state`.
    pub state: String,
    /// `run.priority`.
    pub priority: i64,
    /// `run.created_at`.
    pub created_at: i64,
    /// `run.lease_owner`.
    pub lease_owner: Option<String>,
    /// `run.lease_expires_at`.
    pub lease_expires_at: Option<i64>,
}

impl SeedRun {
    /// A `READY`, never-leased run.
    pub fn ready(id: &str, priority: i64, created_at: i64) -> Self {
        SeedRun {
            id: id.to_string(),
            state: "READY".to_string(),
            priority,
            created_at,
            lease_owner: None,
            lease_expires_at: None,
        }
    }

    /// The same run in a different state.
    pub fn with_state(mut self, state: &str) -> Self {
        self.state = state.to_string();
        self
    }

    /// The same run holding a lease.
    pub fn leased(mut self, owner: &str, expires_at: i64) -> Self {
        self.lease_owner = Some(owner.to_string());
        self.lease_expires_at = Some(expires_at);
        self
    }
}

/// Insert one task per run (the unique partial index allows only one active run
/// per task) plus the runs themselves.
pub fn seed_runs(store: &mut Store, runs: &[SeedRun]) -> StoreResult<()> {
    seed_parents(store)?;
    with_immediate(store.conn_mut(), |tx| {
        for (i, run) in runs.iter().enumerate() {
            let task_id = format!("T-{i:04}-{}", run.id);
            tx.execute(
                "INSERT INTO task
                   (id, plan_version_id, slice_id, state, scope_globs,
                    verification_profile, attempt_budget, created_at)
                 VALUES (?1, 'pv-1', 'S1', 'READY', '[\"crates/**\"]', 'default', 3, ?2)",
                params![task_id, run.created_at],
            )?;
            tx.execute(
                "INSERT INTO run
                   (id, task_id, policy_hash, workspace_id, base_commit, run_branch,
                    state, priority, lease_owner, lease_expires_at, lease_epoch, created_at)
                 VALUES (?1, ?2, ?3, NULL, 'abc123', 'conductor/run', ?4, ?5, ?6, ?7, 0, ?8)",
                params![
                    run.id,
                    task_id,
                    POLICY_HASH,
                    run.state,
                    run.priority,
                    run.lease_owner,
                    run.lease_expires_at,
                    run.created_at,
                ],
            )?;
        }
        Ok(())
    })
}

/// `n` `READY` runs named `r-0001..`, all at the same priority, created in order.
pub fn seed_ready_runs(store: &mut Store, n: usize) -> StoreResult<Vec<String>> {
    let runs: Vec<SeedRun> = (1..=n)
        .map(|i| SeedRun::ready(&format!("r-{i:04}"), 100, 1_000 + i as i64))
        .collect();
    seed_runs(store, &runs)?;
    Ok(runs.into_iter().map(|r| r.id).collect())
}

/// `SELECT COUNT(*)` helper.
pub fn count(store: &Store, sql: &str) -> i64 {
    store
        .conn()
        .query_row(sql, [], |row| row.get(0))
        .unwrap_or_else(|e| panic!("count query failed: {sql}: {e}"))
}
