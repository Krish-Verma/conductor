//! The plan ledger's own tables: `project`, `plan_version`, `decision`.
//!
//! Until S11, `project` and `plan_version` were hand-seeded by every other
//! test suite (`tests/common::seed_parents`) because nothing in
//! `conductor-store` wrote them, and `decision` had never been written at
//! all. This file exercises the real writer.

mod common;

use conductor_core::{PlanVersionId, PlanVersionState, ProjectId, TaskId};
use conductor_store::{
    DecisionStatus, NewDecision, NewPlanVersion, NewProject, NewTask, Store, StoreError,
};

use common::temp_store;

fn new_project(id: &str, root: &str) -> NewProject {
    NewProject {
        id: ProjectId::new(id).expect("project id"),
        root_path: root.to_string(),
        repo_identity: "blake3:repo".to_string(),
        default_branch: "main".to_string(),
        config_hash: "blake3:cfg".to_string(),
    }
}

/// A project row, already upserted.
fn seeded_project(store: &mut Store) -> ProjectId {
    let row = store
        .upsert_project(&new_project("p-1", "/repo"), 0)
        .expect("upsert project");
    row.id
}

fn new_plan_version(id: &str, project_id: &ProjectId, version: i64) -> NewPlanVersion {
    NewPlanVersion {
        id: PlanVersionId::new(id).expect("plan version id"),
        project_id: project_id.clone(),
        version,
        content_hash: "blake3:v1".to_string(),
        source_path: format!(".conductor/plans/v{version}/plan.yaml"),
    }
}

// ---------------------------------------------------------------------------
// project
// ---------------------------------------------------------------------------

#[test]
fn a_new_project_can_be_read_back_by_id_and_by_root_path() {
    let (_dir, mut store) = temp_store();
    let row = store
        .upsert_project(&new_project("p-1", "/repo"), 1_000)
        .expect("upsert");
    assert_eq!(row.id.as_str(), "p-1");
    assert_eq!(row.root_path, "/repo");
    assert_eq!(row.created_at, 1_000);

    let by_id = store
        .project(&ProjectId::new("p-1").expect("id"))
        .expect("read")
        .expect("a row");
    assert_eq!(by_id, row);

    let by_root = store
        .project_by_root("/repo")
        .expect("read")
        .expect("a row");
    assert_eq!(by_root, row);
}

#[test]
fn upserting_the_same_project_id_twice_refreshes_facts_but_keeps_created_at() {
    // `project` is `upsert`, not `create`: re-running `conductor init` against
    // the same repository must not fail just because the mirror row already
    // exists.
    let (_dir, mut store) = temp_store();
    store
        .upsert_project(&new_project("p-1", "/repo"), 1_000)
        .expect("first upsert");

    let mut second = new_project("p-1", "/repo");
    second.config_hash = "blake3:new-cfg".to_string();
    let row = store.upsert_project(&second, 9_999).expect("second upsert");

    assert_eq!(row.config_hash, "blake3:new-cfg");
    assert_eq!(
        row.created_at, 1_000,
        "created_at is a fact about the first time the project was seen, \
         not the most recent upsert"
    );
}

#[test]
fn a_different_id_at_an_already_registered_root_path_is_a_named_refusal() {
    // Finding 1 (round 1 review). §3.5's recovery path is "re-register the
    // project → read `.conductor/` → rebuild the task list" (Task 9). An
    // operator who renamed `id:` in `project.yaml` and re-registers must be
    // told this root path already belongs to a different project id — not
    // handed a bare "UNIQUE constraint failed: project.root_path".
    let (_dir, mut store) = temp_store();
    store
        .upsert_project(&new_project("p-1", "/repo"), 1_000)
        .expect("first registration");

    let error = store
        .upsert_project(&new_project("p-2", "/repo"), 2_000)
        .expect_err("a different id at an already-registered root path must be refused");
    assert!(
        matches!(error, StoreError::Domain(_)),
        "expected a named domain refusal, not a raw sqlite error: {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("/repo"),
        "must name the root path: {message}"
    );
    assert!(
        message.contains("p-1"),
        "must name the id already holding it: {message}"
    );
    assert!(
        message.contains("p-2"),
        "must name the id being offered: {message}"
    );

    // The refusal must not have written anything: the original row survives
    // untouched, and the offered id never gained a row of its own.
    let row = store.project_by_root("/repo").expect("read").expect("row");
    assert_eq!(row.id.as_str(), "p-1");
    assert_eq!(row.created_at, 1_000);
    assert_eq!(
        store
            .project(&ProjectId::new("p-2").expect("id"))
            .expect("read"),
        None,
        "the rejected id must not have gained a row"
    );
}

// ---------------------------------------------------------------------------
// plan_version — creation and the §5.2 legality table
// ---------------------------------------------------------------------------

#[test]
fn a_new_plan_version_starts_draft() {
    // §5.2's "Plan (5 states)" diagram begins at DRAFT.
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let row = store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");
    assert_eq!(row.state, PlanVersionState::Draft);
    assert_eq!(row.approved_at, None);
    assert_eq!(row.approved_by, None);
    assert_eq!(row.version, 1);
}

#[test]
fn creating_the_same_plan_version_id_twice_is_refused() {
    // A plan version is immutable once written (S11's objective) — a second
    // create attempt is a mistake upstream, not something to resync.
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("first create");
    assert!(
        store
            .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
            .is_err(),
        "a duplicate plan version id must not be accepted"
    );
}

#[test]
fn creating_a_plan_version_against_a_project_that_does_not_exist_fails_closed() {
    // `PRAGMA foreign_keys = ON` (schema.rs). Materialization depends on this
    // holding, the same reasoning the task/plan_version_id acceptance
    // criterion below states explicitly.
    let (_dir, mut store) = temp_store();
    let ghost = ProjectId::new("p-ghost").expect("id");
    let error = store
        .create_plan_version(&new_plan_version("pv-1", &ghost, 1))
        .expect_err("a plan version cannot reference a project that was never created");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected the FK violation to surface as a sqlite error: {error:?}"
    );
}

#[test]
fn plan_versions_for_project_lists_oldest_first() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    store
        .create_plan_version(&new_plan_version("pv-2", &project_id, 2))
        .expect("create v2");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create v1");

    let versions = store.plan_versions_for_project(&project_id).expect("list");
    let numbers: Vec<i64> = versions.iter().map(|v| v.version).collect();
    assert_eq!(numbers, vec![1, 2], "must be ordered by version, not by id");
}

/// Walk a fresh `DRAFT` plan version through every legal §5.2 edge, and
/// assert every state along the way is exactly what `plan_version` reads
/// back. Positive control for [`set_plan_state`]'s legality table: every one
/// of these must succeed, or the table is wrong in the refusing direction
/// too.
#[test]
fn a_plan_version_walks_the_forward_chain_and_can_then_be_superseded() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");

    for state in [
        PlanVersionState::Validated,
        PlanVersionState::AwaitingApproval,
        PlanVersionState::Approved,
        PlanVersionState::Superseded,
    ] {
        store
            .set_plan_state(&id, state)
            .unwrap_or_else(|e| panic!("{state} should be reachable: {e}"));
        assert_eq!(
            store.plan_version(&id).expect("read").expect("row").state,
            state
        );
    }
}

#[test]
fn a_validated_plan_can_be_rejected_back_to_draft() {
    // §5.2: "validation failure or rejection returns to DRAFT" — the edge out
    // of VALIDATED, not a self-loop on DRAFT.
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");
    store
        .set_plan_state(&id, PlanVersionState::Validated)
        .expect("validate");

    let state = store
        .set_plan_state(&id, PlanVersionState::Draft)
        .expect("VALIDATED → DRAFT must be a legal rejection edge");
    assert_eq!(state, PlanVersionState::Draft);
}

#[test]
fn an_awaiting_approval_plan_can_be_rejected_back_to_draft() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");
    store
        .set_plan_state(&id, PlanVersionState::Validated)
        .expect("validate");
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request");

    let state = store
        .set_plan_state(&id, PlanVersionState::Draft)
        .expect("AWAITING_APPROVAL → DRAFT must be a legal rejection edge");
    assert_eq!(state, PlanVersionState::Draft);
}

#[test]
fn draft_has_no_self_rejection_edge() {
    // A plan already in DRAFT has nothing to reject back into DRAFT — the
    // "first three" language describes what VALIDATED and AWAITING_APPROVAL
    // do, not DRAFT itself.
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");

    let error = store
        .set_plan_state(&id, PlanVersionState::Draft)
        .expect_err("DRAFT → DRAFT must be refused as a self-transition");
    assert!(
        error.to_string().contains("DRAFT → DRAFT"),
        "the refusal must name the transition: {error}"
    );
    assert_eq!(
        store.plan_version(&id).expect("read").expect("row").state,
        PlanVersionState::Draft,
        "a refused transition must not have written anything"
    );
}

#[test]
fn approved_to_approved_is_refused_because_reapproval_is_not_a_transition() {
    // Ruling 5, stated explicitly: "your transition API must refuse
    // APPROVED → APPROVED (and every other illegal edge)".
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");
    store
        .set_plan_state(&id, PlanVersionState::Validated)
        .expect("validate");
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request");
    store
        .set_plan_state(&id, PlanVersionState::Approved)
        .expect("first approval");

    let error = store
        .set_plan_state(&id, PlanVersionState::Approved)
        .expect_err("APPROVED → APPROVED must be refused");
    assert!(
        error.to_string().contains("APPROVED → APPROVED"),
        "the refusal must name the transition: {error}"
    );
    assert_eq!(
        store.plan_version(&id).expect("read").expect("row").state,
        PlanVersionState::Approved,
        "a refused transition must not have written anything"
    );
}

#[test]
fn approved_cannot_fall_back_to_draft_or_awaiting_approval() {
    // "APPROVED is terminal except for SUPERSEDED."
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");
    store
        .set_plan_state(&id, PlanVersionState::Validated)
        .expect("validate");
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request");
    store
        .set_plan_state(&id, PlanVersionState::Approved)
        .expect("approve");

    for illegal in [PlanVersionState::Draft, PlanVersionState::AwaitingApproval] {
        let error = store
            .set_plan_state(&id, illegal)
            .expect_err(&format!("APPROVED → {illegal} must be refused"));
        assert!(error.to_string().contains("APPROVED"), "{error}");
    }
    assert_eq!(
        store.plan_version(&id).expect("read").expect("row").state,
        PlanVersionState::Approved
    );
}

#[test]
fn superseded_is_terminal() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");
    store
        .supersede_plan_version(&id)
        .expect("DRAFT → SUPERSEDED is legal from every non-terminal state");

    let error = store
        .set_plan_state(&id, PlanVersionState::Draft)
        .expect_err("SUPERSEDED has no outgoing edge");
    assert!(error.to_string().contains("SUPERSEDED → DRAFT"), "{error}");
}

// ---------------------------------------------------------------------------
// Ruling 5 — the content-update door that is not the transition table
// ---------------------------------------------------------------------------

#[test]
fn record_plan_approval_content_refuses_a_row_that_is_not_approved() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");

    let error = store
        .record_plan_approval_content(&id, "blake3:new", "alice", 5_000)
        .expect_err("a DRAFT row has never been approved");
    assert!(matches!(error, StoreError::Domain(_)), "{error:?}");

    let row = store.plan_version(&id).expect("read").expect("row");
    assert_eq!(
        row.content_hash, "blake3:v1",
        "unapproved write must not land"
    );
    assert_eq!(row.approved_at, None);
    assert_eq!(row.approved_by, None);
}

#[test]
fn a_content_change_on_an_approved_plan_is_recorded_without_a_state_transition() {
    // The whole point of Ruling 5: re-approval changes content, not state.
    // `set_plan_state(id, Approved)` must be refused (asserted above); this
    // is the door that is used instead.
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let id = PlanVersionId::new("pv-1").expect("id");
    store
        .create_plan_version(&new_plan_version("pv-1", &project_id, 1))
        .expect("create");
    store
        .set_plan_state(&id, PlanVersionState::Validated)
        .expect("validate");
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request");
    store
        .set_plan_state(&id, PlanVersionState::Approved)
        .expect("first transition to APPROVED");
    store
        .record_plan_approval_content(&id, "blake3:first-approval", "alice", 1_000)
        .expect("stamp the first approval's content");

    // The document changed (a reformat, say) and is re-approved. No
    // transition is attempted — the row never left APPROVED.
    store
        .record_plan_approval_content(&id, "blake3:second-approval", "bob", 2_000)
        .expect("re-approval must succeed without a transition");

    let row = store.plan_version(&id).expect("read").expect("row");
    assert_eq!(row.state, PlanVersionState::Approved);
    assert_eq!(row.content_hash, "blake3:second-approval");
    assert_eq!(row.approved_at, Some(2_000));
    assert_eq!(row.approved_by.as_deref(), Some("bob"));

    // And the transition door still refuses APPROVED → APPROVED, proving the
    // content update above did not go through it.
    let error = store
        .set_plan_state(&id, PlanVersionState::Approved)
        .expect_err("APPROVED → APPROVED remains refused");
    assert!(error.to_string().contains("APPROVED → APPROVED"), "{error}");
}

// ---------------------------------------------------------------------------
// decision
// ---------------------------------------------------------------------------

fn new_decision(id: &str, project_id: &ProjectId) -> NewDecision {
    NewDecision {
        id: id.to_string(),
        project_id: project_id.clone(),
        supersedes: None,
        content_hash: "blake3:d1".to_string(),
        source_path: format!("docs/decisions/{id}.md"),
    }
}

#[test]
fn a_new_decision_starts_open() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    let row = store
        .upsert_decision(&new_decision("D-0001", &project_id))
        .expect("upsert");
    assert_eq!(row.status, DecisionStatus::Open);
    assert_eq!(row.supersedes, None);

    let read = store.decision("D-0001").expect("read").expect("row");
    assert_eq!(read, row);
}

#[test]
fn resyncing_a_decision_refreshes_content_but_never_its_status() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    store
        .upsert_decision(&new_decision("D-0001", &project_id))
        .expect("first upsert");
    store
        .set_decision_status("D-0001", DecisionStatus::Accepted)
        .expect("accept");

    let mut resync = new_decision("D-0001", &project_id);
    resync.content_hash = "blake3:d2-reformatted".to_string();
    let row = store.upsert_decision(&resync).expect("resync");

    assert_eq!(row.content_hash, "blake3:d2-reformatted");
    assert_eq!(
        row.status,
        DecisionStatus::Accepted,
        "a content resync must not silently reopen an accepted decision"
    );
}

#[test]
fn decisions_for_project_lists_by_id() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    store
        .upsert_decision(&new_decision("D-0002", &project_id))
        .expect("d2");
    store
        .upsert_decision(&new_decision("D-0001", &project_id))
        .expect("d1");

    let ids: Vec<String> = store
        .decisions_for_project(&project_id)
        .expect("list")
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids, vec!["D-0001".to_string(), "D-0002".to_string()]);
}

/// Positive control: every legal decision edge succeeds. Mirrors the plan
/// version's forward-chain control above.
#[test]
fn a_decision_can_be_accepted_then_superseded() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    store
        .upsert_decision(&new_decision("D-0001", &project_id))
        .expect("create");

    for status in [DecisionStatus::Accepted, DecisionStatus::Superseded] {
        store
            .set_decision_status("D-0001", status)
            .unwrap_or_else(|e| panic!("{status} should be reachable: {e}"));
        assert_eq!(
            store.decision("D-0001").expect("read").expect("row").status,
            status
        );
    }
}

#[test]
fn a_decision_can_be_rejected_then_superseded() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    store
        .upsert_decision(&new_decision("D-0001", &project_id))
        .expect("create");
    store
        .set_decision_status("D-0001", DecisionStatus::Rejected)
        .expect("reject");
    store
        .set_decision_status("D-0001", DecisionStatus::Superseded)
        .expect("supersede a rejected decision");
}

#[test]
fn an_accepted_decision_cannot_be_flipped_directly_to_rejected() {
    // Append-only: reversing a call is a new, superseding decision — never an
    // edit of the old row's outcome.
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    store
        .upsert_decision(&new_decision("D-0001", &project_id))
        .expect("create");
    store
        .set_decision_status("D-0001", DecisionStatus::Accepted)
        .expect("accept");

    let error = store
        .set_decision_status("D-0001", DecisionStatus::Rejected)
        .expect_err("ACCEPTED → REJECTED must be refused");
    assert!(
        error.to_string().contains("ACCEPTED → REJECTED"),
        "the refusal must name the transition: {error}"
    );
    assert_eq!(
        store.decision("D-0001").expect("read").expect("row").status,
        DecisionStatus::Accepted,
        "a refused transition must not have written anything"
    );
}

#[test]
fn superseded_decisions_are_terminal() {
    let (_dir, mut store) = temp_store();
    let project_id = seeded_project(&mut store);
    store
        .upsert_decision(&new_decision("D-0001", &project_id))
        .expect("create");
    store
        .set_decision_status("D-0001", DecisionStatus::Superseded)
        .expect("supersede");

    let error = store
        .set_decision_status("D-0001", DecisionStatus::Accepted)
        .expect_err("SUPERSEDED has no outgoing edge");
    assert!(
        error.to_string().contains("SUPERSEDED → ACCEPTED"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Acceptance criterion: materialization depends on task's plan_version_id FK
// ---------------------------------------------------------------------------

#[test]
fn creating_a_task_against_a_plan_version_that_does_not_exist_fails_closed() {
    // "Creating a task whose plan_version_id has no row fails (FK is ON) —
    // assert it, because materialization depends on it." A materializer that
    // races ahead of `plan approve` and tries to write a task must be
    // stopped by the database, not merely by careful call ordering.
    let (_dir, mut store) = temp_store();
    let error = store
        .create_task(
            &NewTask {
                id: TaskId::new("T-0001").expect("id"),
                plan_version_id: "pv-does-not-exist".to_string(),
                slice_id: "S1".to_string(),
                scope_globs: vec!["src/**".to_string()],
                verification_profile: "default".to_string(),
                attempt_budget: 3,
            },
            0,
        )
        .expect_err("a task cannot reference a plan version that was never created");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected the FK violation to surface as a sqlite error: {error:?}"
    );
}
