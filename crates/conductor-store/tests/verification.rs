//! The content-addressed verification cache — master plan §4.5 and Part 5.1.
//!
//! ```sql
//! CREATE UNIQUE INDEX ix_verif_cache
//!   ON verification_check(tree_hash, check_id, command_hash, toolchain_fingerprint)
//!   WHERE outcome IN ('PASS','FAIL');
//! ```
//!
//! Two claims are tested here, and neither is "it caches". The first is that a
//! hit requires **all four** key components to match — a cache that answers on
//! three of four returns the wrong result rather than no result. The second is
//! that `INCONCLUSIVE` and `VOID` are recorded and never served.

use conductor_core::{Fence, RunId, RunState, VerificationOutcome};
use conductor_store::verification::{CacheKey, RecordOutcome, VerificationRecord, cached, record};
use conductor_store::{Store, with_immediate};

const RUN: &str = "r-0041";
const NOW: i64 = 1_770_000_000_000;
const POLICY_HASH: &str = "blake3:0000000000000000000000000000000000000000000000000000000000000000";

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    fence: Fence,
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut store = Store::open_or_create(dir.path().join("conductor.db")).expect("store");
    with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT INTO project (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES ('p-1', '/fixture', 'blake3:repo', 'main', 'blake3:cfg', 0)",
            [],
        )?;
        tx.execute(
            "INSERT INTO plan_version (id, project_id, version, content_hash, state, source_path)
             VALUES ('pv-1', 'p-1', 1, 'blake3:plan', 'APPROVED', 'plan.yaml')",
            [],
        )?;
        tx.execute(
            "INSERT INTO policy_snapshot (hash, canonical_blob, created_at) VALUES (?1, '{}', 0)",
            rusqlite::params![POLICY_HASH],
        )?;
        tx.execute(
            "INSERT INTO task (id, plan_version_id, slice_id, state, scope_globs,
                               verification_profile, attempt_budget, created_at)
             VALUES ('T-0012', 'pv-1', 'S4', 'READY', '[\"src/**\"]', 'default', 3, 0)",
            [],
        )?;
        tx.execute(
            "INSERT INTO run (id, task_id, policy_hash, base_commit, run_branch, state,
                              priority, lease_epoch, created_at)
             VALUES (?1, 'T-0012', ?2, 'abc123', 'conductor/T-0012/r-0041', 'READY', 100, 0, 0)",
            rusqlite::params![RUN, POLICY_HASH],
        )?;
        Ok(())
    })
    .expect("seed");

    let claimed = store
        .claim_run(&RunId::new(RUN).expect("id"), "worker-1", NOW, 60_000)
        .expect("claim")
        .expect("a READY run is claimable");
    let fence = claimed.fence();
    World {
        _dir: dir,
        store,
        fence,
    }
}

fn key<'a>() -> CacheKey<'a> {
    CacheKey {
        tree_hash: "tree-1",
        check_id: "typecheck",
        command_hash: "blake3:cmd",
        toolchain_fingerprint: "blake3:tools",
    }
}

fn entry(outcome: VerificationOutcome) -> VerificationRecord {
    VerificationRecord {
        id: format!("vc-{outcome:?}"),
        attempt_id: None,
        commit_sha: "abc123".to_string(),
        exit_code: Some(0),
        duration_ms: Some(12),
        outcome,
        log_path: Some("/artifacts/r-0041/verification/typecheck-1.log".to_string()),
    }
}

#[test]
fn a_result_is_served_back_at_the_same_key() {
    let mut w = world();
    assert!(
        cached(w.store.conn(), &key()).expect("lookup").is_none(),
        "an empty cache must not answer"
    );

    record(
        w.store.conn_mut(),
        &w.fence,
        &key(),
        &entry(VerificationOutcome::Pass),
        NOW,
    )
    .expect("record");

    let hit = cached(w.store.conn(), &key())
        .expect("lookup")
        .expect("a recorded PASS must be served");
    assert_eq!(hit.outcome, VerificationOutcome::Pass);
    assert_eq!(
        hit.log_path.as_deref(),
        Some("/artifacts/r-0041/verification/typecheck-1.log")
    );
}

#[test]
fn changing_any_one_of_the_four_key_components_is_a_miss() {
    // The point of a four-part key is that three parts are not enough. Each
    // component is varied on its own, so a cache that quietly ignored one would
    // fail exactly one of these assertions rather than none of them.
    let mut w = world();
    record(
        w.store.conn_mut(),
        &w.fence,
        &key(),
        &entry(VerificationOutcome::Pass),
        NOW,
    )
    .expect("record");

    let variations: &[(&str, CacheKey<'_>)] = &[
        (
            "tree_hash",
            CacheKey {
                tree_hash: "tree-2",
                ..key()
            },
        ),
        (
            "check_id",
            CacheKey {
                check_id: "unit-tests",
                ..key()
            },
        ),
        (
            "command_hash",
            CacheKey {
                command_hash: "blake3:other-cmd",
                ..key()
            },
        ),
        (
            "toolchain_fingerprint",
            CacheKey {
                toolchain_fingerprint: "blake3:other-tools",
                ..key()
            },
        ),
    ];

    for (component, varied) in variations {
        assert!(
            cached(w.store.conn(), varied).expect("lookup").is_none(),
            "a different {component} must miss the cache"
        );
    }

    // …and the original still hits, so the misses above are not a broken cache.
    assert!(cached(w.store.conn(), &key()).expect("lookup").is_some());
}

#[test]
fn a_fail_is_cached_because_a_failing_tree_stays_failing() {
    let mut w = world();
    record(
        w.store.conn_mut(),
        &w.fence,
        &key(),
        &entry(VerificationOutcome::Fail),
        NOW,
    )
    .expect("record");
    assert_eq!(
        cached(w.store.conn(), &key())
            .expect("lookup")
            .expect("hit")
            .outcome,
        VerificationOutcome::Fail
    );
}

#[test]
fn inconclusive_and_void_are_recorded_but_never_served() {
    // Part 5.1's index is partial on purpose: `WHERE outcome IN ('PASS','FAIL')`.
    //
    // INCONCLUSIVE is a statement about the *environment at a moment* — a
    // timeout, a vanished toolchain, a full disk — not about the tree. Caching
    // it would make a transient condition permanent for that tree, and §4.5's
    // remedy for INCONCLUSIVE is "bounded infra retry", which a cache hit would
    // make impossible.
    //
    // VOID is stronger still: it means "we do not know which tree this
    // observed". Storing it under a tree hash as an answer *for* that tree
    // would be a lie by construction.
    for outcome in [VerificationOutcome::Inconclusive, VerificationOutcome::Void] {
        let mut w = world();
        record(w.store.conn_mut(), &w.fence, &key(), &entry(outcome), NOW).expect("record");

        assert!(
            cached(w.store.conn(), &key()).expect("lookup").is_none(),
            "{outcome:?} must never be served from the cache"
        );

        // But it is on the record: the audit trail is the whole reason the row
        // is written at all.
        let rows: i64 = w
            .store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM verification_check WHERE outcome = ?1",
                rusqlite::params![format!("{outcome:?}").to_uppercase()],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(rows, 1, "{outcome:?} must still be recorded as evidence");
    }
}

#[test]
fn many_inconclusive_results_may_share_a_key() {
    // The partial index does not constrain them, and it must not: an infra
    // retry produces a second INCONCLUSIVE at the same key, and a UNIQUE
    // violation there would turn a retry into a crash.
    let mut w = world();
    for i in 0..3 {
        let mut e = entry(VerificationOutcome::Inconclusive);
        e.id = format!("vc-inconclusive-{i}");
        record(w.store.conn_mut(), &w.fence, &key(), &e, NOW).expect("record");
    }
    let rows: i64 = w
        .store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM verification_check WHERE outcome = 'INCONCLUSIVE'",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(rows, 3);
}

#[test]
fn recording_the_same_outcome_twice_is_idempotent() {
    let mut w = world();
    let first = record(
        w.store.conn_mut(),
        &w.fence,
        &key(),
        &entry(VerificationOutcome::Pass),
        NOW,
    )
    .expect("record");
    assert_eq!(first, RecordOutcome::Inserted);

    let mut again = entry(VerificationOutcome::Pass);
    again.id = "vc-second".to_string();
    let second = record(w.store.conn_mut(), &w.fence, &key(), &again, NOW).expect("record");
    assert_eq!(second, RecordOutcome::AlreadyPresent);

    let rows: i64 = w
        .store
        .conn()
        .query_row("SELECT COUNT(*) FROM verification_check", [], |row| {
            row.get(0)
        })
        .expect("count");
    assert_eq!(rows, 1);
}

#[test]
fn a_second_run_that_contradicts_the_cache_is_reported_not_swallowed() {
    // Same tree, same command, same toolchain, different answer. That is a
    // nondeterministic check, and it is exactly what the cache would otherwise
    // hide: whichever result landed first would be served forever.
    let mut w = world();
    record(
        w.store.conn_mut(),
        &w.fence,
        &key(),
        &entry(VerificationOutcome::Pass),
        NOW,
    )
    .expect("record");

    let mut contradiction = entry(VerificationOutcome::Fail);
    contradiction.id = "vc-contradiction".to_string();
    let result = record(w.store.conn_mut(), &w.fence, &key(), &contradiction, NOW).expect("record");

    assert_eq!(
        result,
        RecordOutcome::Contradicted {
            stored: VerificationOutcome::Pass,
        }
    );
    // The cache keeps the first answer rather than flip-flopping, and the
    // caller is told so it can raise a finding.
    assert_eq!(
        cached(w.store.conn(), &key())
            .expect("lookup")
            .expect("hit")
            .outcome,
        VerificationOutcome::Pass
    );
}

#[test]
fn a_stale_worker_cannot_record_a_verification_result() {
    // Every write in this crate is fenced (§4.7). A verification result is not
    // an exception: a worker that stalled past its lease must not be able to
    // mark a tree green for its successor.
    let mut w = world();
    let stale = w.fence.clone();

    w.store.expire_leases(NOW + 120_000).expect("sweep");

    let error = record(
        w.store.conn_mut(),
        &stale,
        &key(),
        &entry(VerificationOutcome::Pass),
        NOW,
    )
    .expect_err("a stale epoch must be refused");
    assert!(
        error.to_string().contains("epoch") || error.to_string().contains("fence"),
        "unhelpful error: {error}"
    );
    assert!(cached(w.store.conn(), &key()).expect("lookup").is_none());
}

#[test]
fn the_run_state_is_untouched_by_recording_a_result() {
    // S4 records evidence. Advancing the run is S5's business, and a slice that
    // quietly moved the state would make the next slice's tests lie.
    let mut w = world();
    record(
        w.store.conn_mut(),
        &w.fence,
        &key(),
        &entry(VerificationOutcome::Pass),
        NOW,
    )
    .expect("record");
    assert_eq!(
        w.store
            .run_state(&RunId::new(RUN).expect("id"))
            .expect("state")
            .expect("row")
            .0,
        RunState::Running
    );
}
