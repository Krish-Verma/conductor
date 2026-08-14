//! The atomic claim (§4.7), single-writer semantics.
//!
//! Concurrency lives in `concurrency.rs`; this file pins the statement's
//! behaviour: what it selects, what it writes, and what it refuses.

mod common;

use common::SeedRun;
use conductor_core::RunClaimedPayload;
use conductor_store::claim::CLAIM_SQL;

const LEASE_MS: i64 = 60_000; // master plan §4.7
const NOW: i64 = 1_770_000_000_000;

#[test]
fn claim_sql_is_the_statement_from_section_4_7() {
    // The statement is architecture, not an implementation detail: it is the
    // one place two workers could take the same run. Changing it must break a
    // test, loudly.
    assert!(CLAIM_SQL.contains("UPDATE run"));
    assert!(CLAIM_SQL.contains("lease_epoch = lease_epoch + 1"));
    // S3 resolved §4.7's contradiction: `RECOVERING` is not one of §5.2's
    // states, so the predicate selected a value nothing could ever write.
    assert!(CLAIM_SQL.contains("WHERE state IN ('READY','RECONCILING')"));
    assert!(!CLAIM_SQL.contains("RECOVERING"));
    // A claimed RECONCILING run still owes a reconciliation, so the claim takes
    // ownership without rewriting the state — §5.2 has no RECONCILING → RUNNING
    // edge, and manufacturing one would lose the fact that the repository has
    // not been looked at yet.
    assert!(
        CLAIM_SQL.contains("CASE WHEN state='RECONCILING' THEN 'RECONCILING' ELSE 'RUNNING' END")
    );
    assert!(CLAIM_SQL.contains("ORDER BY priority, created_at LIMIT 1"));
    assert!(CLAIM_SQL.contains("RETURNING id, task_id, policy_hash, lease_epoch, state"));
}

#[test]
fn claim_takes_exactly_one_run_and_increments_the_fencing_epoch() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 3).expect("seed");

    let claimed = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run was available");

    assert_eq!(claimed.run_id.as_str(), "r-0001");
    assert_eq!(claimed.lease_epoch, 1, "lease_epoch is the fencing token");
    assert_eq!(claimed.lease_owner, "worker-a");
    assert_eq!(claimed.lease_expires_at, NOW + LEASE_MS);
    assert_eq!(claimed.policy_hash.as_str(), common::POLICY_HASH);

    assert_eq!(
        common::count(&store, "SELECT COUNT(*) FROM run WHERE state='RUNNING'"),
        1
    );
    assert_eq!(
        common::count(&store, "SELECT COUNT(*) FROM run WHERE state='READY'"),
        2
    );
    assert_eq!(
        common::count(
            &store,
            "SELECT COUNT(*) FROM run WHERE state='RUNNING' AND lease_owner IS NULL"
        ),
        0,
        "no partial transition"
    );
}

#[test]
fn claim_writes_the_run_claimed_event_in_the_same_transaction() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 2).expect("seed");

    let claimed = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");

    let (run_id, seq, kind, payload, at): (String, i64, String, String, i64) = store
        .conn()
        .query_row(
            "SELECT run_id, seq, kind, payload, at FROM event",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("exactly one event row");

    assert_eq!(run_id, claimed.run_id.as_str());
    assert_eq!(seq, 1);
    assert_eq!(kind, "RUN_CLAIMED");
    assert_eq!(at, NOW);

    let decoded: RunClaimedPayload = serde_json::from_str(&payload).expect("payload is typed json");
    assert_eq!(decoded.lease_epoch, 1);
    assert_eq!(decoded.lease_owner, "worker-a");
    assert_eq!(decoded.lease_expires_at, NOW + LEASE_MS);

    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM event"), 1);
}

#[test]
fn event_seq_advances_per_run() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    // Same run becomes claimable again once its lease has expired.
    let later = NOW + LEASE_MS + 1;
    store
        .conn()
        .execute("UPDATE run SET state='READY' WHERE id='r-0001'", [])
        .expect("re-arm");
    store
        .claim_next_run("worker-b", later, LEASE_MS)
        .expect("claim")
        .expect("a run");

    let seqs: Vec<i64> = {
        let mut stmt = store
            .conn()
            .prepare("SELECT seq FROM event WHERE run_id='r-0001' ORDER BY seq")
            .expect("prepare");
        stmt.query_map([], |row| row.get::<_, i64>(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    };
    assert_eq!(seqs, vec![1, 2]);
}

#[test]
fn second_claim_of_the_same_run_bumps_the_epoch_again() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    let first = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    store
        .conn()
        .execute("UPDATE run SET state='RECONCILING' WHERE id='r-0001'", [])
        .expect("re-arm as RECONCILING");

    let second = store
        .claim_next_run("worker-b", NOW + LEASE_MS + 1, LEASE_MS)
        .expect("claim")
        .expect("a run");

    assert_eq!(first.lease_epoch, 1);
    assert_eq!(
        second.lease_epoch, 2,
        "a re-claim must move the fencing token"
    );
}

#[test]
fn claim_returns_none_when_nothing_is_eligible() {
    let (_dir, mut store) = common::temp_store();
    common::seed_runs(
        &mut store,
        &[
            SeedRun::ready("r-0001", 100, 1).with_state("PENDING"),
            SeedRun::ready("r-0002", 100, 2).with_state("COMPLETE"),
            SeedRun::ready("r-0003", 100, 3).with_state("BLOCKED"),
        ],
    )
    .expect("seed");

    assert_eq!(
        store
            .claim_next_run("worker-a", NOW, LEASE_MS)
            .expect("claim"),
        None
    );
    assert_eq!(common::count(&store, "SELECT COUNT(*) FROM event"), 0);
}

#[test]
fn claim_covers_both_ready_and_reconciling() {
    let (_dir, mut store) = common::temp_store();
    common::seed_runs(
        &mut store,
        &[SeedRun::ready("r-0001", 100, 1).with_state("RECONCILING")],
    )
    .expect("seed");

    let claimed = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a RECONCILING run with no live lease is what a restart must take");
    assert_eq!(claimed.run_id.as_str(), "r-0001");
}

#[test]
fn claiming_a_reconciling_run_leaves_it_reconciling() {
    // §5.2's task machine has no RECONCILING → RUNNING edge, and the state is
    // the record that Conductor still owes this run a look at the repository.
    // A claim that rewrote it to RUNNING would erase that obligation, and the
    // next crash would have nothing to recover from.
    let (_dir, mut store) = common::temp_store();
    common::seed_runs(
        &mut store,
        &[SeedRun::ready("r-0001", 100, 1).with_state("RECONCILING")],
    )
    .expect("seed");

    let claimed = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");

    assert_eq!(claimed.state, conductor_core::RunState::Reconciling);
    assert_eq!(
        common::count(
            &store,
            "SELECT COUNT(*) FROM run WHERE id='r-0001' AND state='RECONCILING'"
        ),
        1,
        "the claim must not manufacture a RECONCILING → RUNNING transition"
    );
    assert_eq!(claimed.lease_epoch, 1, "ownership still moved");
}

#[test]
fn claiming_a_ready_run_makes_it_running() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    let claimed = store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");

    assert_eq!(claimed.state, conductor_core::RunState::Running);
    assert_eq!(
        common::count(&store, "SELECT COUNT(*) FROM run WHERE state='RUNNING'"),
        1
    );
}

#[test]
fn a_run_left_in_the_deleted_recovering_state_is_not_claimable() {
    // Nothing writes 'RECOVERING' any more. If a database somehow contains one,
    // it must be inert rather than quietly claimable: the S1 predicate's dead
    // disjunct is the bug being closed, and re-admitting the value here would
    // reopen it.
    let (_dir, mut store) = common::temp_store();
    common::seed_runs(
        &mut store,
        &[SeedRun::ready("r-0001", 100, 1).with_state("RECOVERING")],
    )
    .expect("seed");

    assert_eq!(
        store
            .claim_next_run("worker-a", NOW, LEASE_MS)
            .expect("claim"),
        None
    );
}

#[test]
fn a_live_lease_is_not_stolen_but_an_expired_one_is() {
    let (_dir, mut store) = common::temp_store();
    common::seed_runs(
        &mut store,
        &[SeedRun::ready("r-0001", 100, 1).leased("worker-old", NOW + 10_000)],
    )
    .expect("seed");

    assert_eq!(
        store
            .claim_next_run("worker-new", NOW, LEASE_MS)
            .expect("claim"),
        None,
        "a run whose lease is still live must not be claimable"
    );

    let claimed = store
        .claim_next_run("worker-new", NOW + 20_000, LEASE_MS)
        .expect("claim")
        .expect("an expired lease is reclaimable");
    assert_eq!(claimed.lease_owner, "worker-new");
    assert_eq!(claimed.lease_epoch, 1);
}

#[test]
fn claim_order_is_priority_then_created_at() {
    let (_dir, mut store) = common::temp_store();
    common::seed_runs(
        &mut store,
        &[
            SeedRun::ready("r-late-low", 1, 900),
            SeedRun::ready("r-early-high", 9, 100),
            SeedRun::ready("r-mid", 1, 800),
        ],
    )
    .expect("seed");

    let order: Vec<String> = (0..3)
        .map(|i| {
            store
                .claim_next_run(&format!("worker-{i}"), NOW, LEASE_MS)
                .expect("claim")
                .expect("a run")
                .run_id
                .into_string()
        })
        .collect();

    assert_eq!(order, vec!["r-mid", "r-late-low", "r-early-high"]);
    assert_eq!(
        store
            .claim_next_run("worker-x", NOW, LEASE_MS)
            .expect("claim"),
        None,
        "the queue is exhausted"
    );
}

#[test]
fn one_active_run_per_task_is_enforced_by_the_schema() {
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");

    let task_id: String = store
        .conn()
        .query_row("SELECT task_id FROM run WHERE id='r-0001'", [], |row| {
            row.get(0)
        })
        .expect("task id");

    let err = store
        .conn()
        .execute(
            "INSERT INTO run
               (id, task_id, policy_hash, base_commit, run_branch, state, priority,
                lease_epoch, created_at)
             VALUES ('r-dup', ?1, ?2, 'abc', 'b', 'READY', 100, 0, 0)",
            rusqlite::params![task_id, common::POLICY_HASH],
        )
        .expect_err("a second active run for one task must be refused");
    assert!(
        err.to_string().to_lowercase().contains("unique"),
        "expected the partial unique index to fire, got: {err}"
    );

    // ...but a terminal run does not occupy the slot.
    store
        .conn()
        .execute(
            "INSERT INTO run
               (id, task_id, policy_hash, base_commit, run_branch, state, priority,
                lease_epoch, created_at)
             VALUES ('r-done', ?1, ?2, 'abc', 'b', 'COMPLETE', 100, 0, 0)",
            rusqlite::params![task_id, common::POLICY_HASH],
        )
        .expect("a COMPLETE run is outside the partial index");
}

#[test]
fn event_refuses_a_duplicate_sequence_number_for_one_run() {
    // The `event` log is the evidence that a run was claimed. If two rows can
    // share (run_id, seq), a duplicate claim is recordable rather than
    // rejected, and the only thing that would notice is an offline checker.
    // S0's harness carried this constraint as a tripwire; schema v1 must too.
    let (_dir, mut store) = common::temp_store();
    common::seed_ready_runs(&mut store, 1).expect("seed");
    store
        .claim_next_run("worker-a", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");

    let err = store.conn().execute(
        "INSERT INTO event (run_id, seq, kind, payload, at) VALUES ('r-0001', 1, 'RUN_CLAIMED', '{}', 1)",
        [],
    );

    let err = err.expect_err("a second event at seq=1 for r-0001 must be rejected");
    assert!(
        err.to_string().to_lowercase().contains("unique"),
        "expected a UNIQUE constraint violation, got: {err}"
    );
}
