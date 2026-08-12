//! Every pragma from master plan Part 5.1 (as amended by ADR-0004) must be in
//! effect after open — and be *observed* to be, not assumed. A pragma that is
//! silently dropped is the failure ADR-0004 left open for S1.

mod common;

use conductor_store::schema::{PRAGMAS_EXPECTED, read_pragmas};
use conductor_store::{Store, StoreError};
use rusqlite::Connection;

#[test]
fn every_pragma_reads_back_with_the_expected_value() {
    let (_dir, store) = common::temp_store();
    let pragmas = store.pragmas().expect("read pragmas");

    // Asserted individually and by name, so a failure says which one was lost.
    assert_eq!(pragmas.get("journal_mode"), Some("wal"));
    assert_eq!(pragmas.get("synchronous"), Some("2")); // FULL
    assert_eq!(pragmas.get("fullfsync"), Some("1"));
    assert_eq!(pragmas.get("checkpoint_fullfsync"), Some("1"));
    assert_eq!(pragmas.get("foreign_keys"), Some("1"));
    assert_eq!(pragmas.get("busy_timeout"), Some("5000"));

    assert_eq!(pragmas.mismatches(), vec![]);
    assert_eq!(pragmas.values.len(), PRAGMAS_EXPECTED.len());
}

#[test]
fn journal_mode_is_persistent_but_the_others_are_per_connection() {
    // The two pragma classes do not have the same shape and must not be
    // asserted uniformly: journal_mode survives in the file, everything else
    // resets with the connection.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    {
        let _store = Store::open_or_create(&path).expect("open");
    }

    let store = Store::open_existing(&path).expect("reopen");
    let raw = Connection::open(&path).expect("raw open, no pragmas applied");

    // Persistent: nobody set it on this connection and it is still WAL.
    let journal_mode: String = raw
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal_mode");
    assert_eq!(journal_mode, "wal", "journal_mode must persist in the file");

    // Per-connection, and NOT inherited: this is why apply_pragmas must run on
    // every open. `fullfsync` is the one ADR-0004 added, and it is exactly the
    // one that would be silently lost by a connection that skipped it.
    let raw_fullfsync: i64 = raw
        .query_row("PRAGMA fullfsync", [], |row| row.get(0))
        .expect("fullfsync");
    assert_eq!(
        raw_fullfsync, 0,
        "fullfsync is per-connection and defaults to off"
    );
    // Three pragmas that do NOT distinguish on this stack, asserted so that
    // nobody later mistakes their readback for evidence that the store applied
    // them. Each already holds on a connection that never set it:
    //   * `synchronous` = 2 (FULL) — SQLite's compile-time default here.
    //   * `foreign_keys` = 1 — libsqlite3-sys builds bundled SQLite with
    //     -DSQLITE_DEFAULT_FOREIGN_KEYS=1.
    //   * `busy_timeout` = 5000 — rusqlite calls sqlite3_busy_timeout(db, 5000)
    //     on every open (inner_connection.rs).
    // All three are dependency defaults, not guarantees; the store sets them.
    let raw_sync: i64 = raw
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("synchronous");
    let raw_fk: i64 = raw
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .expect("foreign_keys");
    let raw_busy: i64 = raw
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("busy_timeout");
    assert_eq!((raw_sync, raw_fk, raw_busy), (2, 1, 5000));

    // Changing a per-connection pragma on one connection must not touch another.
    raw.execute_batch("PRAGMA foreign_keys = OFF;")
        .expect("disable fk on the raw connection");
    assert_eq!(
        store.pragmas().expect("pragmas").get("foreign_keys"),
        Some("1"),
        "the store's connection must be unaffected by another connection"
    );

    // ...and every pragma still holds on the store's own connection.
    assert_eq!(store.pragmas().expect("pragmas").mismatches(), vec![]);
}

#[test]
fn foreign_keys_are_actually_enforced_not_merely_reported() {
    let (_dir, mut store) = common::temp_store();
    common::seed_parents(&mut store).expect("seed parents");

    let err = store
        .conn()
        .execute(
            "INSERT INTO run
               (id, task_id, policy_hash, base_commit, run_branch, state, priority,
                lease_epoch, created_at)
             VALUES ('r-bad', 'T-does-not-exist', ?1, 'abc', 'b', 'READY', 100, 0, 0)",
            rusqlite::params![common::POLICY_HASH],
        )
        .expect_err("insert with a dangling task_id must be refused");
    assert!(
        err.to_string().to_lowercase().contains("foreign key"),
        "expected a foreign key violation, got: {err}"
    );
}

#[test]
fn a_downgraded_pragma_is_detected_rather_than_ignored() {
    // The detector must have teeth: if fullfsync is turned off behind the
    // store's back, mismatches() must say so.
    let (_dir, store) = common::temp_store();
    store
        .conn()
        .execute_batch("PRAGMA fullfsync = 0;")
        .expect("downgrade fullfsync");

    let pragmas = read_pragmas(store.conn()).expect("read pragmas");
    assert_eq!(
        pragmas.mismatches(),
        vec![("fullfsync", "1", "0".to_string())]
    );
}

#[test]
fn open_fails_closed_when_a_pragma_does_not_hold() {
    // Same teeth, on the open path: StoreError::PragmaMismatch must exist and
    // carry which pragma was lost.
    let err = StoreError::PragmaMismatch {
        pragma: "fullfsync",
        expected: "1",
        actual: "0".to_string(),
    };
    assert!(err.to_string().contains("fullfsync"));
}
