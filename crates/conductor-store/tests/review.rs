//! The `review` row — §5.2's three states, §6.5's packet and imported decision.
//!
//! Nothing here tests "it stores a review". Every test is a refusal, because
//! every guarantee S13 adds is one: a decision cannot exist without a packet to
//! bind to, a review cannot be exported twice, a decided review cannot be
//! answered again, a run cannot have two open reviews, and a human's resolution
//! of a finding cannot be overwritten by the next one.
//!
//! The two positive controls matter as much as the refusals. A rule that always
//! says no is indistinguishable from a broken query, so
//! `a_run_gets_a_new_review_once_the_first_one_is_decided` proves the partial
//! index still admits the case it is supposed to, and
//! `results_for_run_returns_this_runs_checks_and_not_another_runs` seeds two runs
//! so that an accessor missing its `WHERE run_id` cannot pass.

mod common;

use conductor_core::{Fence, PlanVersionId, ReviewDecision, ReviewState, RunId, TaskId};
use conductor_store::review::NewReview;
use conductor_store::verification::{CacheKey, VerificationRecord};
use conductor_store::{Store, StoreError, with_immediate};

const RUN_A: &str = "r-0001";
const RUN_B: &str = "r-0002";
const TASK_A: &str = "T-0001";
const TASK_B: &str = "T-0002";
const PLAN_VERSION: &str = "pv-1";
const NOW: i64 = 1_770_000_000_000;

struct World {
    _dir: tempfile::TempDir,
    store: Store,
}

/// Two `READY` runs on two tasks of one approved plan version.
///
/// Two, not one, throughout: the run-scoped accessors here are exactly the kind
/// that pass a single-run fixture while missing their `WHERE run_id`.
fn world() -> World {
    let (dir, mut store) = common::temp_store();
    common::seed_parents(&mut store).expect("seed parents");
    with_immediate(store.conn_mut(), |tx| {
        for (task, run) in [(TASK_A, RUN_A), (TASK_B, RUN_B)] {
            tx.execute(
                "INSERT INTO task
                   (id, plan_version_id, slice_id, state, scope_globs,
                    verification_profile, attempt_budget, created_at)
                 VALUES (?1, ?2, 'S13', 'READY', '[\"crates/**\"]', 'default', 3, 0)",
                rusqlite::params![task, PLAN_VERSION],
            )?;
            tx.execute(
                "INSERT INTO run
                   (id, task_id, policy_hash, base_commit, run_branch, target_branch,
                    state, priority, lease_epoch, created_at)
                 VALUES (?1, ?2, ?3, 'abc123', 'conductor/run', 'main', 'READY', 100, 0, 0)",
                rusqlite::params![run, task, common::POLICY_HASH],
            )?;
        }
        Ok(())
    })
    .expect("seed runs");
    World { _dir: dir, store }
}

fn run_id(id: &str) -> RunId {
    RunId::new(id).expect("run id")
}

/// A review of `RUN_A`, at whichever boundary the caller names.
fn new_review(id: &str, run: &str, task: &str, boundary: &str) -> NewReview {
    NewReview {
        id: id.to_string(),
        run_id: run_id(run),
        task_id: TaskId::new(task).expect("task id"),
        plan_version_id: PlanVersionId::new(PLAN_VERSION).expect("plan version id"),
        boundary: boundary.to_string(),
    }
}

/// Claim a `READY` run so a fenced write has a fence.
fn fence_for(store: &mut Store, run: &str) -> Fence {
    store
        .claim_run(&run_id(run), "worker-1", NOW, 60_000)
        .expect("claim")
        .expect("a READY run is claimable")
        .fence()
}

#[test]
fn open_export_and_decide_records_the_packet_and_then_the_human_answer() {
    // §5.2's whole machine, with the row asserted at each step. Would fail if any
    // of the three writes stopped recording what it was given — most usefully if
    // `mark_exported` moved the state without binding the packet hash, which is
    // the row `record_decision` must never accept.
    let mut w = world();

    let opened = w
        .store
        .open_review(&new_review("rv-1", RUN_A, TASK_A, "task-complete"), NOW)
        .expect("open");
    assert_eq!(opened.state, ReviewState::Pending);
    assert_eq!(opened.run_id, run_id(RUN_A));
    assert_eq!(opened.task_id.as_str(), TASK_A);
    assert_eq!(opened.plan_version_id.as_str(), PLAN_VERSION);
    assert_eq!(opened.boundary, "task-complete");
    assert_eq!(
        opened.packet_hash, None,
        "a PENDING review has no packet: that is the only reason the column is nullable"
    );
    assert_eq!(opened.packet_path, None);
    assert_eq!(opened.decision, None);
    assert_eq!(opened.decided_by, None);
    assert_eq!(opened.decided_at, None);

    let exported = w
        .store
        .mark_review_exported("rv-1", "blake3:packet", "/tmp/packet.yaml")
        .expect("export");
    assert_eq!(exported.state, ReviewState::Exported);
    assert_eq!(exported.packet_hash.as_deref(), Some("blake3:packet"));
    assert_eq!(exported.packet_path.as_deref(), Some("/tmp/packet.yaml"));
    assert_eq!(
        exported.decision, None,
        "exporting must not decide anything on the human's behalf"
    );

    let decided = w
        .store
        .record_review_decision(
            "rv-1",
            ReviewDecision::Accept,
            "operator@host",
            Some("scope respected, checks green"),
            NOW + 5_000,
        )
        .expect("decide");
    assert_eq!(decided.state, ReviewState::Decided);
    assert_eq!(decided.decision, Some(ReviewDecision::Accept));
    assert_eq!(decided.decided_by.as_deref(), Some("operator@host"));
    assert_eq!(decided.decided_at, Some(NOW + 5_000));
    assert_eq!(
        decided.notes.as_deref(),
        Some("scope respected, checks green")
    );
    assert_eq!(
        decided.packet_hash.as_deref(),
        Some("blake3:packet"),
        "the decision stays bound to the packet the human read"
    );

    // The same row, read back through the by-id accessor rather than the write's
    // return value — S12's lesson about a mechanism proven only by what the
    // builder handed back.
    let reread = w.store.review("rv-1").expect("read").expect("present");
    assert_eq!(reread, decided);
}

#[test]
fn a_decision_on_a_pending_review_is_refused_because_nothing_was_ever_exported() {
    // A PENDING review has no packet, so there is nothing a human could have
    // read. Would fail if `record_decision` stopped consulting §5.2's table, or
    // if it dropped the `packet_hash IS NOT NULL` precondition.
    let mut w = world();
    w.store
        .open_review(&new_review("rv-1", RUN_A, TASK_A, "task-complete"), NOW)
        .expect("open");

    let err = w
        .store
        .record_review_decision("rv-1", ReviewDecision::Accept, "operator", None, NOW)
        .expect_err("a PENDING review cannot be decided");
    match err {
        StoreError::IllegalReviewTransition(message) => {
            assert!(
                message.contains("PENDING"),
                "the refusal must name the state it refused: {message}"
            );
        }
        other => panic!("expected IllegalReviewTransition, got {other:?}"),
    }

    let row = w.store.review("rv-1").expect("read").expect("present");
    assert_eq!(
        row.state,
        ReviewState::Pending,
        "the refusal changed nothing"
    );
    assert_eq!(row.decision, None);
}

#[test]
fn a_second_export_is_refused_because_it_would_mint_a_second_packet_hash() {
    // The reason §5.2 draws no `EXPORTED → EXPORTED` edge: two packet hashes for
    // one review means an imported decision could be bound to whichever suited
    // whoever wrote it. Would fail if `mark_exported` used `UPDATE … WHERE id = ?`
    // without the state guard, or dropped the `transition_to` check.
    let mut w = world();
    w.store
        .open_review(&new_review("rv-1", RUN_A, TASK_A, "task-complete"), NOW)
        .expect("open");
    w.store
        .mark_review_exported("rv-1", "blake3:first", "/tmp/first.yaml")
        .expect("first export");

    let err = w
        .store
        .mark_review_exported("rv-1", "blake3:second", "/tmp/second.yaml")
        .expect_err("a review is exported once");
    match err {
        StoreError::IllegalReviewTransition(message) => assert!(
            message.contains("EXPORTED"),
            "the refusal must name the state it refused: {message}"
        ),
        other => panic!("expected IllegalReviewTransition, got {other:?}"),
    }

    let row = w.store.review("rv-1").expect("read").expect("present");
    assert_eq!(
        row.packet_hash.as_deref(),
        Some("blake3:first"),
        "the first packet must still be the one the review is bound to"
    );
    assert_eq!(row.packet_path.as_deref(), Some("/tmp/first.yaml"));
}

#[test]
fn a_second_decision_is_refused_because_decided_is_terminal() {
    // §6.5 makes the import a mutating operation, which makes it somewhere an
    // attacker would like to arrive twice — once to have a decision applied and
    // again to have a different one applied. Would fail if `DECIDED` gained a
    // successor, or if `record_decision` stopped guarding on the state.
    let mut w = world();
    w.store
        .open_review(&new_review("rv-1", RUN_A, TASK_A, "task-complete"), NOW)
        .expect("open");
    w.store
        .mark_review_exported("rv-1", "blake3:packet", "/tmp/packet.yaml")
        .expect("export");
    w.store
        .record_review_decision("rv-1", ReviewDecision::Repair, "operator", None, NOW)
        .expect("first decision");

    let err = w
        .store
        .record_review_decision(
            "rv-1",
            ReviewDecision::Accept,
            "someone-else",
            None,
            NOW + 1,
        )
        .expect_err("a decided review cannot be answered again");
    match err {
        StoreError::IllegalReviewTransition(message) => assert!(
            message.contains("DECIDED"),
            "the refusal must name the state it refused: {message}"
        ),
        other => panic!("expected IllegalReviewTransition, got {other:?}"),
    }

    let row = w.store.review("rv-1").expect("read").expect("present");
    assert_eq!(
        row.decision,
        Some(ReviewDecision::Repair),
        "the second answer must not overwrite the first"
    );
    assert_eq!(row.decided_by.as_deref(), Some("operator"));
}

#[test]
fn a_decision_is_refused_while_the_packet_hash_is_null() {
    // The binding the whole review-authority story rests on: §4.3's
    // REVIEW_ACCEPTANCE authorizes *a review packet*, so a decision with nothing
    // to bind to would authorize nothing in particular.
    //
    // `mark_exported` cannot produce this row — it writes the state and the hash
    // in one statement — so the state is forced directly, which is the point: the
    // refusal must hold for a row that arrived by a path the API does not offer
    // (a hand edit, a partial write, a future second writer). The same condition
    // is in `record_decision`'s own `UPDATE … WHERE`, so removing the early check
    // would still refuse; removing both is what this test catches.
    let mut w = world();
    w.store
        .open_review(&new_review("rv-1", RUN_A, TASK_A, "task-complete"), NOW)
        .expect("open");
    w.store
        .conn()
        .execute(
            "UPDATE review SET state = 'EXPORTED', packet_hash = NULL WHERE id = 'rv-1'",
            [],
        )
        .expect("force an EXPORTED review with no packet");

    let err = w
        .store
        .record_review_decision("rv-1", ReviewDecision::Accept, "operator", None, NOW)
        .expect_err("a decision must have a packet to bind to");
    match err {
        StoreError::DecisionWithoutPacket { review_id } => assert_eq!(review_id, "rv-1"),
        other => panic!("expected DecisionWithoutPacket, got {other:?}"),
    }

    let row = w.store.review("rv-1").expect("read").expect("present");
    assert_eq!(
        row.state,
        ReviewState::Exported,
        "the refusal changed nothing"
    );
    assert_eq!(row.decision, None);
}

#[test]
fn a_run_cannot_have_two_open_reviews() {
    // `ix_review_one_open_per_run`. Two concurrent exports would otherwise mint
    // two packets with two hashes for one run. Would fail if the index's
    // predicate were dropped or narrowed to a single state.
    let mut w = world();
    w.store
        .open_review(&new_review("rv-1", RUN_A, TASK_A, "task-complete"), NOW)
        .expect("open the first");

    let err = w
        .store
        .open_review(&new_review("rv-2", RUN_A, TASK_A, "integration"), NOW + 1)
        .expect_err("a run has at most one open review");
    match err {
        StoreError::ReviewAlreadyOpen {
            run_id: reported_run,
            open_review_id,
        } => {
            assert_eq!(reported_run, RUN_A);
            assert_eq!(open_review_id, "rv-1");
        }
        other => panic!("expected ReviewAlreadyOpen, got {other:?}"),
    }

    // A review already exported is still open, so the refusal is not merely about
    // `PENDING`.
    w.store
        .mark_review_exported("rv-1", "blake3:packet", "/tmp/packet.yaml")
        .expect("export");
    assert!(matches!(
        w.store
            .open_review(&new_review("rv-3", RUN_A, TASK_A, "integration"), NOW + 2),
        Err(StoreError::ReviewAlreadyOpen { .. })
    ));

    // ...and the *other* run is unaffected: the index is per-run, not global.
    w.store
        .open_review(&new_review("rv-4", RUN_B, TASK_B, "task-complete"), NOW + 3)
        .expect("a different run may open its own review");
}

#[test]
fn a_run_gets_a_new_review_once_the_first_one_is_decided() {
    // The positive control for the test above. An index that always refuses is
    // indistinguishable from a broken one, and §6.5's `repair` decision sends
    // work back — so the next boundary must be able to open a *new* review rather
    // than reopening a decided one (`DECIDED` is terminal).
    let mut w = world();
    w.store
        .open_review(&new_review("rv-1", RUN_A, TASK_A, "task-complete"), NOW)
        .expect("open");
    w.store
        .mark_review_exported("rv-1", "blake3:first", "/tmp/first.yaml")
        .expect("export");
    w.store
        .record_review_decision("rv-1", ReviewDecision::Repair, "operator", None, NOW + 1)
        .expect("decide");

    let second = w
        .store
        .open_review(&new_review("rv-2", RUN_A, TASK_A, "post-repair"), NOW + 2)
        .expect("a decided review no longer occupies the run's open slot");
    assert_eq!(second.state, ReviewState::Pending);

    // `open_review_for_run` resolves the open one, not the decided one.
    let open = w
        .store
        .open_review_for_run(&run_id(RUN_A))
        .expect("read")
        .expect("one is open");
    assert_eq!(open.id, "rv-2");

    // Both are still on the run's history, oldest first.
    let all = w.store.reviews_for_run(&run_id(RUN_A)).expect("read");
    assert_eq!(
        all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["rv-1", "rv-2"]
    );
    assert!(
        w.store
            .reviews_for_run(&run_id(RUN_B))
            .expect("read")
            .is_empty(),
        "reviews_for_run must not return another run's reviews"
    );
}

#[test]
fn the_partial_index_refuses_a_second_open_review_even_on_a_direct_insert() {
    // Non-vacuity of the guarantee, as distinct from the guard. `review::open`
    // pre-checks so the refusal has a name; the *guarantee* is the unique partial
    // index, and this proves the schema refuses a second open review even when
    // the pre-check is bypassed entirely. Would fail if
    // `ix_review_one_open_per_run` were demoted to a plain index — which no test
    // going through the API alone would notice.
    let mut w = world();
    w.store
        .open_review(&new_review("rv-1", RUN_A, TASK_A, "task-complete"), NOW)
        .expect("open");

    let raw = w.store.conn().execute(
        "INSERT INTO review
           (id, run_id, task_id, plan_version_id, boundary, state, created_at)
         VALUES ('rv-sneaky', ?1, ?2, ?3, 'integration', 'PENDING', 0)",
        rusqlite::params![RUN_A, TASK_A, PLAN_VERSION],
    );
    assert!(
        raw.is_err(),
        "the index, not the pre-check, is what guarantees one open review per run"
    );
}

#[test]
fn a_finding_resolves_once_and_a_second_resolution_is_refused() {
    // §4.8: findings never auto-resolve, and `resolution` carries the reason a
    // *person* gave. A second resolution overwriting the first would delete that
    // reason with no trace it had been given. Would fail if `resolve_finding`
    // dropped `AND resolution IS NULL`, or treated `changed == 0` as success.
    let mut w = world();
    let fence = fence_for(&mut w.store, RUN_A);
    w.store
        .record_finding(
            &fence,
            "f-1",
            "SCOPE_VIOLATION",
            "HIGH",
            "touched docs/",
            NOW,
        )
        .expect("raise");
    assert_eq!(
        w.store.findings_for_run(&run_id(RUN_A)).expect("read")[0].resolution,
        None,
        "the runtime must never resolve its own finding"
    );

    w.store
        .resolve_finding(
            &fence,
            "f-1",
            "accepted: docs/ change was intended",
            NOW + 1,
        )
        .expect("a human resolves it once");
    let findings = w.store.findings_for_run(&run_id(RUN_A)).expect("read");
    assert_eq!(
        findings[0].resolution.as_deref(),
        Some("accepted: docs/ change was intended")
    );

    let err = w
        .store
        .resolve_finding(&fence, "f-1", "actually, never mind", NOW + 2)
        .expect_err("a resolved finding cannot be re-resolved");
    match err {
        StoreError::FindingAlreadyResolved { finding_id } => assert_eq!(finding_id, "f-1"),
        other => panic!("expected FindingAlreadyResolved, got {other:?}"),
    }
    assert_eq!(
        w.store.findings_for_run(&run_id(RUN_A)).expect("read")[0]
            .resolution
            .as_deref(),
        Some("accepted: docs/ change was intended"),
        "the first human's reason must survive the second attempt"
    );

    // A finding this run does not have is a different failure, and reported as
    // one: "already resolved" and "no such finding" call for opposite responses.
    match w
        .store
        .resolve_finding(&fence, "f-absent", "whatever", NOW + 3)
        .expect_err("there is no such finding")
    {
        StoreError::NoSuchFinding { finding_id, .. } => assert_eq!(finding_id, "f-absent"),
        other => panic!("expected NoSuchFinding, got {other:?}"),
    }
}

#[test]
fn results_for_run_returns_this_runs_checks_and_not_another_runs() {
    // Two runs, because an accessor missing its `WHERE run_id` passes every
    // single-run fixture. §4.5's results are bound to a tree hash *and* a run,
    // and a review packet quoting another run's checks would be evidence about
    // work nobody is reviewing.
    let mut w = world();
    let fence_a = fence_for(&mut w.store, RUN_A);
    let fence_b = fence_for(&mut w.store, RUN_B);

    conductor_store::verification::record(
        w.store.conn_mut(),
        &fence_a,
        &CacheKey {
            tree_hash: "tree-a",
            check_id: "typecheck",
            command_hash: "blake3:cmd-a",
            toolchain_fingerprint: "blake3:tools",
        },
        &VerificationRecord {
            id: "vc-a1".to_string(),
            attempt_id: None,
            commit_sha: "aaa".to_string(),
            exit_code: Some(0),
            duration_ms: Some(1_200),
            outcome: conductor_core::VerificationOutcome::Pass,
            log_path: Some("/logs/a1".to_string()),
        },
        NOW,
    )
    .expect("record A's typecheck");
    conductor_store::verification::record(
        w.store.conn_mut(),
        &fence_a,
        &CacheKey {
            tree_hash: "tree-a",
            check_id: "test",
            command_hash: "blake3:cmd-a2",
            toolchain_fingerprint: "blake3:tools",
        },
        &VerificationRecord {
            id: "vc-a2".to_string(),
            attempt_id: None,
            commit_sha: "aaa".to_string(),
            exit_code: Some(1),
            duration_ms: Some(9_000),
            outcome: conductor_core::VerificationOutcome::Fail,
            log_path: None,
        },
        NOW,
    )
    .expect("record A's test");
    conductor_store::verification::record(
        w.store.conn_mut(),
        &fence_b,
        &CacheKey {
            tree_hash: "tree-b",
            check_id: "typecheck",
            command_hash: "blake3:cmd-b",
            toolchain_fingerprint: "blake3:tools",
        },
        &VerificationRecord {
            id: "vc-b1".to_string(),
            attempt_id: None,
            commit_sha: "bbb".to_string(),
            exit_code: Some(0),
            duration_ms: Some(700),
            outcome: conductor_core::VerificationOutcome::Pass,
            log_path: None,
        },
        NOW,
    )
    .expect("record B's typecheck");

    let a = w
        .store
        .verification_results_for_run(&run_id(RUN_A))
        .expect("read A");
    assert_eq!(
        a.iter().map(|r| r.check_id.as_str()).collect::<Vec<_>>(),
        vec!["typecheck", "test"],
        "insertion order, which is the order a human reads them in"
    );
    assert_eq!(a[0].outcome, conductor_core::VerificationOutcome::Pass);
    assert_eq!(a[0].tree_hash, "tree-a");
    assert_eq!(a[0].exit_code, Some(0));
    assert_eq!(a[0].duration_ms, Some(1_200));
    assert_eq!(a[0].log_path.as_deref(), Some("/logs/a1"));
    assert_eq!(a[1].outcome, conductor_core::VerificationOutcome::Fail);
    assert!(
        a.iter().all(|r| r.tree_hash != "tree-b"),
        "run A must not be told about run B's tree: {a:?}"
    );

    let b = w
        .store
        .verification_results_for_run(&run_id(RUN_B))
        .expect("read B");
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].tree_hash, "tree-b");
    assert_eq!(b[0].duration_ms, Some(700));
}
