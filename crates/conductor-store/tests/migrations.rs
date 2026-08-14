//! Forward-only migrations: ordered, idempotent, transactional, and verified by
//! `integrity_check` after each applied step.

mod common;

use std::collections::BTreeSet;

use conductor_store::migrate::{MIGRATIONS, current_version, migrate, pending};
use conductor_store::{Store, StoreError};

fn table_names(store: &Store) -> BTreeSet<String> {
    let mut stmt = store
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query");
    rows.map(|r| r.expect("row")).collect()
}

fn index_names(store: &Store) -> BTreeSet<String> {
    let mut stmt = store
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query");
    rows.map(|r| r.expect("row")).collect()
}

#[test]
fn migrate_applies_schema_v1_and_records_the_version() {
    let (_dir, store) = common::temp_store();
    assert_eq!(
        store.schema_version().expect("version"),
        Some(conductor_store::schema::SUPPORTED_SCHEMA_VERSION)
    );

    let rows: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .expect("count");
    assert_eq!(rows, MIGRATIONS.len() as i64);

    let applied_at: i64 = store
        .conn()
        .query_row(
            "SELECT applied_at FROM schema_version WHERE version = 1",
            [],
            |row| row.get(0),
        )
        .expect("applied_at");
    assert!(applied_at > 0, "applied_at must be a real timestamp");
}

#[test]
fn schema_v1_contains_exactly_the_tables_and_indexes_of_part_5_1() {
    let (_dir, store) = common::temp_store();

    let expected_tables: BTreeSet<String> = [
        "approval_grant",
        "approval_request",
        "artifact",
        "attempt",
        "containment_probe",
        "decision",
        "event",
        "finding",
        "plan_version",
        "policy_snapshot",
        "project",
        "run",
        "schema_version",
        "side_effect",
        "task",
        "verification_check",
        "workspace",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    assert_eq!(table_names(&store), expected_tables);

    let indexes = index_names(&store);
    for expected in [
        "ix_run_one_active_per_task",
        "ix_run_claim",
        "ix_event_run",
        "ix_verif_cache",
        "ix_grant_binding",
    ] {
        assert!(
            indexes.contains(expected),
            "missing index {expected}; have {indexes:?}"
        );
    }
}

#[test]
fn the_run_table_has_exactly_the_columns_the_claim_depends_on() {
    let (_dir, store) = common::temp_store();
    let mut stmt = store
        .conn()
        .prepare("SELECT name FROM pragma_table_info('run') ORDER BY cid")
        .expect("prepare");
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    assert_eq!(
        columns,
        vec![
            "id",
            "task_id",
            "policy_hash",
            "workspace_id",
            "base_commit",
            "run_branch",
            "state",
            "priority",
            "lease_owner",
            "lease_expires_at",
            "lease_epoch",
            "created_at",
        ]
    );
}

#[test]
fn migrate_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    let mut store = Store::open_or_create(&path).expect("open");

    let before = table_names(&store);
    let steps = migrate(store.conn_mut()).expect("second migrate must not error");
    assert_eq!(steps.len(), MIGRATIONS.len());
    assert!(
        steps.iter().all(|s| !s.applied),
        "a second migrate must apply nothing: {steps:?}"
    );
    assert_eq!(table_names(&store), before);

    let rows: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        rows,
        MIGRATIONS.len() as i64,
        "schema_version must not gain duplicate rows"
    );

    // ...and re-opening (which migrates) is equally a no-op.
    drop(store);
    let store = Store::open_or_create(&path).expect("reopen");
    assert_eq!(
        store.schema_version().expect("version"),
        Some(conductor_store::schema::SUPPORTED_SCHEMA_VERSION)
    );
    assert_eq!(table_names(&store), before);
}

#[test]
fn integrity_check_is_run_and_is_ok_after_each_applied_migration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    let mut conn = rusqlite::Connection::open(&path).expect("raw open");
    conductor_store::schema::apply_pragmas(&conn).expect("pragmas");

    let steps = migrate(&mut conn).expect("migrate");
    assert_eq!(steps.len(), MIGRATIONS.len());
    for step in &steps {
        assert!(step.applied, "first migrate must apply every migration");
        assert_eq!(
            step.integrity_check.as_deref(),
            Some(["ok".to_string()].as_slice()),
            "migration {} must be followed by integrity_check = ok",
            step.version
        );
    }
}

#[test]
fn pending_reports_work_before_and_nothing_after() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    let mut conn = rusqlite::Connection::open(&path).expect("raw open");
    conductor_store::schema::apply_pragmas(&conn).expect("pragmas");

    assert_eq!(current_version(&conn).expect("version"), None);
    assert_eq!(pending(&conn).expect("pending").len(), MIGRATIONS.len());

    migrate(&mut conn).expect("migrate");

    assert_eq!(
        current_version(&conn).expect("version"),
        Some(conductor_store::schema::SUPPORTED_SCHEMA_VERSION)
    );
    assert!(pending(&conn).expect("pending").is_empty());
}

#[test]
fn a_database_from_the_future_is_refused_not_downgraded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    {
        let store = Store::open_or_create(&path).expect("open");
        store
            .conn()
            .execute(
                "INSERT INTO schema_version (version, applied_at) VALUES (99, 1)",
                [],
            )
            .expect("insert future version");
    }

    let err = Store::open_or_create(&path).expect_err("must refuse a future schema");
    match err {
        StoreError::SchemaTooNew { found, supported } => {
            assert_eq!(found, 99);
            assert_eq!(supported, conductor_store::schema::SUPPORTED_SCHEMA_VERSION);
        }
        other => panic!("expected SchemaTooNew, got {other:?}"),
    }
}

#[test]
fn migrations_are_ordered_and_versions_are_unique() {
    let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
    let mut sorted = versions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(versions, sorted, "migrations must be ascending and unique");
    assert_eq!(versions.first(), Some(&1));
    // Gapless: a missing version would make `pending` skip work silently.
    for (i, v) in versions.iter().enumerate() {
        assert_eq!(*v, i as i64 + 1);
    }
}

#[test]
fn migration_2_adds_the_attempt_state_column() {
    // The contradiction §5.2 recorded: the attempt machine has eight states and
    // schema v1 could persist five of them, none of which say "in flight". A
    // supervisor could not record that an attempt was running, which is exactly
    // what startup recovery reads.
    let (_dir, store) = common::temp_store();

    let mut stmt = store
        .conn()
        .prepare("SELECT name FROM pragma_table_info('attempt') ORDER BY cid")
        .expect("prepare");
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    assert!(
        columns.contains(&"state".to_string()),
        "attempt.state is missing; have {columns:?}"
    );
    // v1's columns are all still there: this is a forward migration, not a
    // rewrite.
    for v1 in [
        "id",
        "run_id",
        "ordinal",
        "kind",
        "adapter",
        "launcher",
        "caps_snapshot",
        "agent_session_id",
        "pid",
        "pid_start_time",
        "started_at",
        "ended_at",
        "exit_code",
        "signal",
        "outcome",
    ] {
        assert!(columns.contains(&v1.to_string()), "v1 lost column {v1}");
    }
}

#[test]
fn migration_2_defaults_existing_attempts_to_created() {
    // A row written under v1 has no state. It must come out of the migration as
    // something recovery can reason about rather than NULL.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");

    // Build a v1 database by applying only migration 1.
    {
        let mut conn = rusqlite::Connection::open(&path).expect("raw open");
        conductor_store::schema::apply_pragmas(&conn).expect("pragmas");
        conductor_store::migrate::apply_up_to(&mut conn, 1).expect("v1 only");
        assert_eq!(current_version(&conn).expect("version"), Some(1));
        conn.execute_batch(
            "INSERT INTO project (id, root_path, repo_identity, default_branch, config_hash, created_at)
               VALUES ('p-1','/r','blake3:r','main','blake3:c',0);
             INSERT INTO plan_version (id, project_id, version, content_hash, state, source_path)
               VALUES ('pv-1','p-1',1,'blake3:p','APPROVED','p.yaml');
             INSERT INTO policy_snapshot (hash, canonical_blob, created_at)
               VALUES ('blake3:pol','{}',0);
             INSERT INTO task (id, plan_version_id, slice_id, state, scope_globs,
                               verification_profile, attempt_budget, created_at)
               VALUES ('T-1','pv-1','S1','RUNNING','[]','default',3,0);
             INSERT INTO run (id, task_id, policy_hash, base_commit, run_branch, state,
                              priority, lease_epoch, created_at)
               VALUES ('r-1','T-1','blake3:pol','abc','b','RUNNING',100,1,0);
             INSERT INTO attempt (id, run_id, ordinal, kind, adapter, launcher, caps_snapshot)
               VALUES ('a-1','r-1',1,'IMPLEMENT','fake','none','{}');",
        )
        .expect("seed a v1 row");
    }

    let store = Store::open_or_create(&path).expect("migrate forward");
    assert_eq!(
        store.schema_version().expect("version"),
        Some(conductor_store::schema::SUPPORTED_SCHEMA_VERSION)
    );
    let state: String = store
        .conn()
        .query_row("SELECT state FROM attempt WHERE id='a-1'", [], |r| r.get(0))
        .expect("state");
    assert_eq!(state, "CREATED");
    assert_eq!(
        store.integrity_check().expect("integrity"),
        vec!["ok".to_string()]
    );
}

#[test]
fn the_artifact_digest_column_is_named_for_the_hash_conductor_actually_uses() {
    // §2.2 authorises `blake3` and no SHA-2 implementation, and S3 established
    // content_hash() = "blake3:<hex>". A column called `sha256` holding a BLAKE3
    // digest is a lie told by the schema to everyone who later reads it, and it
    // would eventually be "fixed" by adding an unauthorised dependency (ADR-0007).
    let (_dir, store) = common::temp_store();
    let cols: Vec<String> = store
        .conn()
        .prepare("SELECT name FROM pragma_table_info('artifact')")
        .expect("prepare")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");

    assert!(
        cols.iter().any(|c| c == "content_hash"),
        "artifact must record a content_hash; columns were {cols:?}"
    );
    assert!(
        !cols.iter().any(|c| c == "sha256"),
        "artifact must not name a hash it does not compute; columns were {cols:?}"
    );
}
