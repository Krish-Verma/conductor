//! Failure-injection helper for the S1 `SIGKILL` test.
//!
//! Opens the real store, starts a real `BEGIN IMMEDIATE` transaction, writes
//! rows into three tables, announces readiness on stdout, and then waits to be
//! killed. It never commits, so every row it wrote must be absent after the
//! parent reopens the database.

use std::io::Write;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use conductor_store::Store;
use rusqlite::{TransactionBehavior, params};

/// Rows written inside the doomed transaction. Combined with the tiny page
/// cache below this forces a **cache spill**, so uncommitted pages are really
/// written into the WAL before the kill. Without that, SQLite would still be
/// holding every dirty page in memory and the kill would prove nothing: there
/// would be nothing on disk to roll back.
const UNCOMMITTED_EVENTS: i64 = 4000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [db_path, marker] = args.as_slice() else {
        eprintln!("usage: conductor-kill-victim <db-path> <marker>");
        return ExitCode::from(64);
    };

    let mut store = match Store::open_existing(db_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("open {db_path}: {err}");
            return ExitCode::from(70);
        }
    };

    let policy_hash: String = store
        .conn()
        .query_row("SELECT hash FROM policy_snapshot LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("the parent must have seeded a policy snapshot");

    let task_id = format!("T-{marker}");
    let run_id = format!("r-{marker}");
    let payload = format!("{{\"marker\":\"{marker}\"}}");

    // 16 KiB of page cache, spilling enabled: dirty pages from an *open*
    // transaction get written to the WAL.
    store
        .conn()
        .execute_batch("PRAGMA cache_size = -16; PRAGMA cache_spill = 1;")
        .expect("shrink page cache");

    let tx = store
        .conn_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin immediate");

    tx.execute(
        "INSERT INTO task
           (id, plan_version_id, slice_id, state, scope_globs, verification_profile,
            attempt_budget, created_at)
         VALUES (?1, 'pv-1', 'S1', 'READY', '[]', 'default', 3, 0)",
        params![task_id],
    )
    .expect("insert task");
    tx.execute(
        "INSERT INTO run
           (id, task_id, policy_hash, base_commit, run_branch, state, priority,
            lease_epoch, created_at)
         VALUES (?1, ?2, ?3, 'abc', 'b', 'READY', 100, 0, 0)",
        params![run_id, task_id, policy_hash],
    )
    .expect("insert run");
    for seq in 1..=UNCOMMITTED_EVENTS {
        tx.execute(
            "INSERT INTO event (run_id, seq, kind, payload, at)
             VALUES (?1, ?2, 'RUN_CLAIMED', ?3, 0)",
            params![run_id, seq, payload],
        )
        .expect("insert event");
    }

    // The write lock is held and the rows exist only inside this transaction.
    println!("READY");
    std::io::stdout().flush().expect("flush readiness");

    // Wait to be killed. If the parent ever fails to kill us, exiting after a
    // bounded wait is better than hanging its test run forever — and `tx` is
    // dropped, which rolls back, so this path cannot commit either.
    sleep(Duration::from_secs(120));
    eprintln!("victim was never killed");
    ExitCode::from(70)
}
