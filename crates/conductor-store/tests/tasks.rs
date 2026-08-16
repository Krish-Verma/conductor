//! The `task` row: creation, state transitions, and the run that carries the
//! integration target.
//!
//! §5.2's run "mirrors its task", and until S5 nothing wrote `task.state` at
//! all. Two rows meaning the same thing are two rows that can disagree, so every
//! task-state write here goes through §5.2's legality table — which is what makes
//! `RUNNING → COMPLETE` refused in the database as well as unrepresentable in
//! the type system.

mod common;

use conductor_core::{RunState, TaskId, TaskState};
use conductor_store::{NewRun, NewTask, Store};

use common::{POLICY_HASH, seed_parents, temp_store};

fn task_id() -> TaskId {
    TaskId::new("T-0012").expect("task id")
}

fn new_task() -> NewTask {
    NewTask {
        id: task_id(),
        plan_version_id: "pv-1".to_string(),
        slice_id: "S5".to_string(),
        scope_globs: vec!["src/**".to_string()],
        verification_profile: ".conductor/verification.yaml".to_string(),
        attempt_budget: 3,
    }
}

fn seeded() -> (tempfile::TempDir, Store) {
    let (dir, mut store) = temp_store();
    seed_parents(&mut store).expect("seed parents");
    (dir, store)
}

#[test]
fn a_new_task_starts_pending() {
    // §5.2's diagram begins at PENDING, and S5's vertical is the claim that a
    // task gets from there to COMPLETE.
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");

    let row = store.task(&task_id()).expect("read").expect("a row");
    assert_eq!(row.state, TaskState::Pending);
    assert_eq!(row.scope_globs, vec!["src/**".to_string()]);
    assert_eq!(row.attempt_budget, 3);
    assert_eq!(row.verification_profile, ".conductor/verification.yaml");
}

#[test]
fn creating_the_same_task_twice_is_refused_rather_than_silently_reused() {
    // Two tasks with one id would give two runs one identity, and the unique
    // partial index on `run(task_id)` would then refuse the second run with an
    // error about the wrong thing.
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");
    assert!(
        store.create_task(&new_task(), 0).is_err(),
        "a duplicate task id must not be accepted"
    );
}

#[test]
fn a_task_walks_the_slices_happy_path() {
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");

    for state in [
        TaskState::Ready,
        TaskState::Running,
        TaskState::Reconciling,
        TaskState::Verifying,
        TaskState::Complete,
    ] {
        store
            .set_task_state(&task_id(), state)
            .unwrap_or_else(|e| panic!("{state} should be reachable: {e}"));
        assert_eq!(
            store.task(&task_id()).expect("read").expect("a row").state,
            state
        );
    }
}

#[test]
fn the_database_refuses_running_to_complete() {
    // The whole slice in one assertion. S3 made this unrepresentable for a run;
    // the `task` row is a second place the same claim can be written, and it is
    // written by a different statement.
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");
    store
        .set_task_state(&task_id(), TaskState::Ready)
        .expect("ready");
    store
        .set_task_state(&task_id(), TaskState::Running)
        .expect("running");

    let error = store
        .set_task_state(&task_id(), TaskState::Complete)
        .expect_err("RUNNING → COMPLETE must be refused");
    assert!(
        error.to_string().contains("RUNNING → COMPLETE"),
        "the refusal must name the transition: {error}"
    );

    // …and the row did not move.
    assert_eq!(
        store.task(&task_id()).expect("read").expect("a row").state,
        TaskState::Running,
        "a refused transition must not have written anything"
    );
}

#[test]
fn a_run_carries_the_branch_it_will_integrate_into() {
    // §4.1: "if the target ref moved, the run enters `AWAITING_REVIEW` with the
    // divergence attached". Deciding that needs both halves — which ref, and
    // what it pointed at when the run started. `base_commit` is the second half;
    // before S5 the schema had no place for the first, so the question could not
    // be asked at all.
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");
    store
        .create_run(
            &NewRun {
                id: conductor_core::RunId::new("r-0041").expect("run id"),
                task_id: task_id(),
                policy_hash: POLICY_HASH.to_string(),
                base_commit: "abc123".to_string(),
                run_branch: "conductor/T-0012/r-0041".to_string(),
                target_branch: "main".to_string(),
            },
            0,
        )
        .expect("create run");

    let row = store
        .run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("read")
        .expect("a row");
    assert_eq!(row.state, RunState::Ready);
    assert_eq!(row.target_branch.as_deref(), Some("main"));
    assert_eq!(row.base_commit, "abc123");
    assert_eq!(row.run_branch, "conductor/T-0012/r-0041");
}

#[test]
fn tasks_can_be_listed_and_filtered_by_state() {
    // `conductor task list [--state …]` (§7.1).
    let (_dir, mut store) = seeded();
    for (n, state) in [(1, TaskState::Pending), (2, TaskState::Ready)] {
        let id = TaskId::new(format!("T-{n:04}")).expect("id");
        store
            .create_task(
                &NewTask {
                    id: id.clone(),
                    ..new_task()
                },
                0,
            )
            .expect("create");
        if state != TaskState::Pending {
            store.set_task_state(&id, state).expect("advance");
        }
    }

    let all = store.tasks(None).expect("list");
    assert_eq!(all.len(), 2);
    // Stable order, so `--json` output is diffable between runs.
    assert_eq!(all[0].id.as_str(), "T-0001");

    let ready = store.tasks(Some(TaskState::Ready)).expect("list");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id.as_str(), "T-0002");
}

// ---------------------------------------------------------------------------
// Materialized plan content (S11 T2, schema v8): NULL vs '[]' is load-bearing
// ---------------------------------------------------------------------------

#[test]
fn a_freshly_created_task_has_never_materialized_anything() {
    // `create_task` does not touch the three new columns, so a task nobody
    // has run a materializer against must read NULL — "never materialized" —
    // on all three, not the empty-declaration "[]".
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");

    assert_eq!(store.declared_actions(&task_id()).expect("read"), None);
    assert_eq!(store.depends_on(&task_id()).expect("read"), None);
    assert_eq!(store.acceptance_criteria(&task_id()).expect("read"), None);
}

#[test]
fn declaring_zero_actions_is_distinguishable_from_never_having_been_materialized() {
    // Ruling 4, the load-bearing distinction: NULL ("not materialized") and
    // "[]" ("materialized, declares none") must not collapse into one value
    // through this API.
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");

    store
        .set_declared_actions(&task_id(), Some("[]"))
        .expect("materialize with no declared actions");
    assert_eq!(
        store.declared_actions(&task_id()).expect("read"),
        Some("[]".to_string()),
        "a materialized-but-empty declaration must read back as '[]', not NULL"
    );

    // And the same call on depends_on and acceptance_criteria.
    store
        .set_depends_on(&task_id(), Some("[]"))
        .expect("materialize with no dependencies");
    assert_eq!(
        store.depends_on(&task_id()).expect("read"),
        Some("[]".to_string())
    );
    store
        .set_acceptance_criteria(&task_id(), Some("[]"))
        .expect("materialize with no acceptance criteria");
    assert_eq!(
        store.acceptance_criteria(&task_id()).expect("read"),
        Some("[]".to_string())
    );
}

#[test]
fn a_non_empty_declaration_round_trips_as_the_plan_model_wrote_it() {
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");

    store
        .set_declared_actions(&task_id(), Some("[\"git.push\"]"))
        .expect("materialize an action");
    assert_eq!(
        store.declared_actions(&task_id()).expect("read"),
        Some("[\"git.push\"]".to_string())
    );

    store
        .set_depends_on(&task_id(), Some("[\"T-0001\"]"))
        .expect("materialize a dependency");
    assert_eq!(
        store.depends_on(&task_id()).expect("read"),
        Some("[\"T-0001\"]".to_string())
    );

    let criterion = "[{\"id\":\"AC-1\",\"statement\":\"it works\",\"verified_by\":[\"typecheck\"],\"manual\":false}]";
    store
        .set_acceptance_criteria(&task_id(), Some(criterion))
        .expect("materialize a criterion");
    assert_eq!(
        store.acceptance_criteria(&task_id()).expect("read"),
        Some(criterion.to_string())
    );
}

#[test]
fn clearing_a_materialized_declaration_returns_it_to_never_materialized() {
    // `None` is not "materialized as empty" either — it is the same "never
    // materialized" fact a fresh task starts with.
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create");
    store
        .set_declared_actions(&task_id(), Some("[\"git.push\"]"))
        .expect("materialize");

    store.set_declared_actions(&task_id(), None).expect("clear");
    assert_eq!(store.declared_actions(&task_id()).expect("read"), None);
}

#[test]
fn setting_materialized_content_on_a_task_that_does_not_exist_is_refused() {
    let (_dir, mut store) = seeded();
    let error = store
        .set_declared_actions(&TaskId::new("T-ghost").expect("id"), Some("[]"))
        .expect_err("no such task");
    assert!(
        error.to_string().contains("T-ghost"),
        "the refusal must name the task: {error}"
    );
}

#[test]
fn an_older_database_gains_the_target_branch_column_by_migration() {
    // Forward-only, and it must work on a database written *before* the column
    // existed — which is the only interesting case.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("old.db");
    {
        let mut conn = rusqlite::Connection::open(&path).expect("open");
        conductor_store::schema::apply_pragmas(&conn).expect("pragmas");
        conductor_store::migrate::apply_up_to(&mut conn, 3).expect("migrate to v3");
        let columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('run') WHERE name = 'target_branch'",
                [],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(columns, 0, "v3 must not already have the column");
    }

    let store = Store::open_or_create(&path).expect("open and migrate");
    assert_eq!(
        store.schema_version().expect("version"),
        Some(conductor_store::schema::SUPPORTED_SCHEMA_VERSION)
    );
    let columns: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('run') WHERE name = 'target_branch'",
            [],
            |row| row.get(0),
        )
        .expect("query");
    assert_eq!(columns, 1);
    assert_eq!(store.integrity_check().expect("integrity"), ["ok"]);
}

// ---------------------------------------------------------------------------
// Leaving VERIFYING — §5.2's "`VERIFYING → COMPLETE` requires §4.5's seven
// criteria", at the row level.
// ---------------------------------------------------------------------------

/// A run in `VERIFYING`, claimed, with its fence.
fn run_in_verifying(store: &mut Store) -> conductor_core::Fence {
    use conductor_core::{Attempt, RunId};

    store.create_task(&new_task(), 0).expect("create task");
    store
        .create_run(
            &NewRun {
                id: RunId::new("r-0041").expect("run id"),
                task_id: task_id(),
                policy_hash: POLICY_HASH.to_string(),
                base_commit: "abc123".to_string(),
                run_branch: "conductor/T-0012/r-0041".to_string(),
                target_branch: "main".to_string(),
            },
            0,
        )
        .expect("create run");

    let claimed = store
        .claim_next_run("worker-1", 0, conductor_store::LEASE_MS)
        .expect("claim")
        .expect("something to claim");
    let fence = claimed.fence();

    let attempt = Attempt::create(
        conductor_core::AttemptId::new("r-0041-a1").expect("id"),
        RunId::new("r-0041").expect("id"),
        1,
    )
    .starting()
    .active(1, Some(1))
    .exited(0);
    store
        .advance_to_reconciling(&fence, &attempt.evidence(), 0)
        .expect("reconciling");
    store
        .route_reconciled(&fence, conductor_core::ReconciledRoute::Verifying, "", 0)
        .expect("verifying");
    fence
}

#[test]
fn a_verified_run_reaches_complete() {
    let (_dir, mut store) = seeded();
    let fence = run_in_verifying(&mut store);

    let verified = completion_token("tree-abc");
    let state = store
        .route_verified(
            &fence,
            conductor_core::ReconciledRoute::Complete(verified),
            "every criterion held",
            0,
        )
        .expect("complete");
    assert_eq!(state, RunState::Complete);
}

#[test]
fn a_run_that_is_not_verifying_cannot_be_completed() {
    // The mirror of `NotReconciling`: §5.2 puts COMPLETE downstream of
    // VERIFYING, so a run that never verified cannot be completed even holding
    // a valid token.
    let (_dir, mut store) = seeded();
    store.create_task(&new_task(), 0).expect("create task");
    store
        .create_run(
            &NewRun {
                id: conductor_core::RunId::new("r-0042").expect("run id"),
                task_id: task_id(),
                policy_hash: POLICY_HASH.to_string(),
                base_commit: "abc123".to_string(),
                run_branch: "conductor/T-0012/r-0042".to_string(),
                target_branch: "main".to_string(),
            },
            0,
        )
        .expect("create run");
    let claimed = store
        .claim_next_run("worker-1", 0, conductor_store::LEASE_MS)
        .expect("claim")
        .expect("something to claim");

    let error = store
        .route_verified(
            &claimed.fence(),
            conductor_core::ReconciledRoute::Complete(completion_token("tree-abc")),
            "",
            0,
        )
        .expect_err("a RUNNING run must not be completable");
    assert!(
        error.to_string().contains("VERIFYING"),
        "the refusal must name the state it required: {error}"
    );
}

#[test]
fn reconciliation_cannot_route_straight_to_complete() {
    // §5.2 draws no `RECONCILING → COMPLETE` edge, and §4.5's criterion 1 is
    // "every required check PASS **at the current tree hash**" — which is
    // decided in VERIFYING. A run whose results are all cache hits still passes
    // through VERIFYING; the lookup is what makes that cheap, not a reason to
    // skip the state.
    let (_dir, mut store) = seeded();
    use conductor_core::{Attempt, RunId};
    store.create_task(&new_task(), 0).expect("create task");
    store
        .create_run(
            &NewRun {
                id: RunId::new("r-0043").expect("run id"),
                task_id: task_id(),
                policy_hash: POLICY_HASH.to_string(),
                base_commit: "abc123".to_string(),
                run_branch: "conductor/T-0012/r-0043".to_string(),
                target_branch: "main".to_string(),
            },
            0,
        )
        .expect("create run");
    let claimed = store
        .claim_next_run("worker-1", 0, conductor_store::LEASE_MS)
        .expect("claim")
        .expect("something to claim");
    let fence = claimed.fence();
    let attempt = Attempt::create(
        conductor_core::AttemptId::new("r-0043-a1").expect("id"),
        RunId::new("r-0043").expect("id"),
        1,
    )
    .starting()
    .active(1, Some(1))
    .exited(0);
    store
        .advance_to_reconciling(&fence, &attempt.evidence(), 0)
        .expect("reconciling");

    let error = store
        .route_reconciled(
            &fence,
            conductor_core::ReconciledRoute::Complete(completion_token("tree-abc")),
            "",
            0,
        )
        .expect_err("RECONCILING → COMPLETE is not an edge §5.2 draws");
    assert!(
        error.to_string().contains("RECONCILING → COMPLETE"),
        "the refusal must name the transition: {error}"
    );
}

/// A genuine `VerifiedComplete`, minted the only way there is.
fn completion_token(tree: &str) -> conductor_core::completion::VerifiedComplete {
    use conductor_core::VerificationOutcome;
    use conductor_core::completion::*;

    evaluate(&CompletionEvidence {
        tree_hash: tree.to_string(),
        required: ChecksEvidence::new([CheckEvidence {
            check_id: "typecheck".to_string(),
            outcome: VerificationOutcome::Pass,
            tree_hash: tree.to_string(),
        }]),
        conditional: ChecksEvidence::default(),
        invariants: ChecksEvidence::default(),
        findings: FindingsEvidence::unresolved(0),
        acceptance: AcceptanceEvidence::NotEvaluated {
            owner: Slice::S11PlanLedger,
        },
        reconciliation: ReconciliationEvidence::Clean,
        policy: PolicyEvidence::NotEvaluated {
            owner: Slice::S7Policy,
        },
    })
    .expect("the gate must accept complete evidence")
}
