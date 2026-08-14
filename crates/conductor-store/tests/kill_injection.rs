//! Failure injection: `SIGKILL` a writer mid-transaction, 100 cycles.
//!
//! **What this proves and what it does not.** `SIGKILL` destroys a process
//! without letting it commit, so this demonstrates *crash atomicity*: the WAL
//! recovers, no partial rows survive, `integrity_check` stays `ok`. It does
//! **not** demonstrate media durability under power loss — killing a process
//! does not drop the OS page cache or the drive write cache, which is precisely
//! what `fullfsync=1` exists to defeat. Testing that needs hardware or VM-level
//! fault injection which is not available on this host.

mod common;

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

use conductor_store::Store;

const CYCLES: usize = 100;
const VICTIM: &str = env!("CARGO_BIN_EXE_conductor-kill-victim");

fn table_counts(store: &Store) -> (i64, i64, i64, i64) {
    (
        common::count(store, "SELECT COUNT(*) FROM task"),
        common::count(store, "SELECT COUNT(*) FROM run"),
        common::count(store, "SELECT COUNT(*) FROM event"),
        common::count(store, "SELECT COUNT(*) FROM schema_version"),
    )
}

#[test]
fn sigkill_mid_transaction_never_leaves_partial_rows_or_corruption() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");

    let baseline = {
        let mut store = Store::open_or_create(&db).expect("create store");
        common::seed_parents(&mut store).expect("seed parents");
        table_counts(&store)
    };
    assert_eq!(
        baseline,
        (0, 0, 0, conductor_store::migrate::MIGRATIONS.len() as i64)
    );

    for cycle in 0..CYCLES {
        let marker = format!("victim-{cycle}");

        let mut child = Command::new(VICTIM)
            .arg(&db)
            .arg(&marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cycle {cycle}: spawn victim: {e}"));

        // The victim prints READY only once its transaction holds the write
        // lock and has written rows that must not survive.
        let stdout = child.stdout.take().expect("victim stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .unwrap_or_else(|e| panic!("cycle {cycle}: read readiness: {e}"));
        assert_eq!(
            line.trim(),
            "READY",
            "cycle {cycle}: victim did not reach the mid-transaction point"
        );

        // The kill must interrupt something real. The victim runs with a tiny
        // page cache so its *uncommitted* pages spill into the WAL; if the WAL
        // did not grow, nothing was on disk to roll back and this cycle would
        // prove nothing.
        let wal_len = std::fs::metadata(db.with_extension("db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(
            wal_len > 64 * 1024,
            "cycle {cycle}: WAL is only {wal_len} bytes, so no uncommitted pages \
             reached disk and the kill would prove nothing"
        );

        child
            .kill()
            .unwrap_or_else(|e| panic!("cycle {cycle}: kill: {e}"));
        let status = child
            .wait()
            .unwrap_or_else(|e| panic!("cycle {cycle}: wait: {e}"));
        assert_eq!(
            status.signal(),
            Some(9),
            "cycle {cycle}: victim must die by SIGKILL, got {status:?}"
        );

        let store = Store::open_existing(&db)
            .unwrap_or_else(|e| panic!("cycle {cycle}: reopen after kill: {e}"));

        assert_eq!(
            store.integrity_check().expect("integrity_check"),
            vec!["ok".to_string()],
            "cycle {cycle}: integrity_check failed after SIGKILL"
        );
        assert_eq!(
            store.foreign_key_check().expect("foreign_key_check"),
            0,
            "cycle {cycle}: foreign key violations after SIGKILL"
        );
        assert_eq!(
            table_counts(&store),
            baseline,
            "cycle {cycle}: partial rows survived an uncommitted transaction"
        );
        assert_eq!(
            common::count(
                &store,
                &format!("SELECT COUNT(*) FROM event WHERE payload LIKE '%{marker}%'")
            ),
            0,
            "cycle {cycle}: uncommitted event rows survived"
        );
        assert_eq!(
            store.schema_version().expect("version"),
            Some(conductor_store::schema::SUPPORTED_SCHEMA_VERSION)
        );
    }

    // The store must still be usable after 100 crashes, not merely intact.
    let mut store = Store::open_or_create(&db).expect("reopen");
    common::seed_ready_runs(&mut store, 1).expect("seed after crashes");
    let claimed = store
        .claim_next_run("post-crash", 1_770_000_000_000, 60_000)
        .expect("claim after crashes")
        .expect("a run");
    assert_eq!(claimed.lease_epoch, 1);
}
