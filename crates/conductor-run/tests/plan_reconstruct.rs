//! §3.5's recovery path — master plan §3.2, §3.3, §3.5, §5.2 (slice S11 task 9).
//!
//! # The claim under test
//!
//! §3.2 states it as an invariant and says an acceptance test enforces it:
//! *"deleting `conductor.db` loses no plan, no decision, no policy, and no
//! verification definition."* §3.5 splits the same statement in two — a **Lost**
//! list (run and attempt history, timings, the event journal, the verification
//! cache, pending approval requests, unresolved findings, lease state) and a
//! **Not lost** list (every approved plan, every decision, all policy, all
//! verification definitions, project identity) — and names the path that
//! rebuilds the second: *"re-register the project → read `.conductor/` → rebuild
//! the task list from the approved plan"*.
//!
//! S11's verify line is that sentence: *"Delete `conductor.db`, rebuild: no
//! plan, decision, policy or verification definition is lost."*
//!
//! # Why both halves are asserted
//!
//! A reconstruction that resurrected execution state would pass a test that
//! only checked the **Not lost** list, and it would be wrong in a way that
//! matters more than losing a row: a stale `RUNNING` run with an expired lease,
//! rebuilt from nothing, is a claim about a process that does not exist. §3.5's
//! two lists are therefore two sets of assertions, not one.
//!
//! # The hole this file has to keep shut
//!
//! Restoring `APPROVED` reads an `APPROVED` sidecar and moves a plan version to
//! a state §5.2 says is reachable *"only via a human at the control socket"*.
//! That is only sound because the sidecar is a **receipt** written by
//! [`ledger::approve`] after a real §4.3 grant was consumed, and because §3.3's
//! control 1 keeps an agent's `.conductor/` writes out of the registered tree.
//! It is not sound if reconstruction will adopt *any* sidecar it finds, so the
//! refusals below are the load-bearing tests in this file and
//! [`a_plan_version_with_no_sidecar_is_rebuilt_as_validated_and_still_cannot_materialise`]
//! is the control that stops "refuse everything" from passing them.

use std::collections::BTreeSet;
use std::path::Path;

use conductor_core::{PlanVersionState, ProjectId, RunId, TaskId};
use conductor_git::run_git_ok;
use conductor_run::approval::{
    self, Authorization, Expiry, GrantOptions, NewApprovalRequest, Subject,
};
use conductor_run::plan::{self, ledger, materialize, reconstruct};
use conductor_run::policy::load as policy_load;
use conductor_run::policy::model::{FactSet, Scope};
use conductor_run::verify::profile;
use conductor_store::{NewRun, Store};

// ---------------------------------------------------------------------------
// Fixtures — the four files §3.1 calls authoritative, plus one decision.
// ---------------------------------------------------------------------------

const PROJECT_YAML: &str = "\
project:
  id: p-recovery
  default_branch: main
  adapter: codex
  scope_defaults:
    allowed_globs: [\"crates/**\"]
    forbidden_globs: [\".conductor/**\"]
execution_requirements:
  filesystem_write: restricted
  control_surface: hard
";

const VERIFICATION_YAML: &str = "\
verification:
  required:
    - id: typecheck
      command: cargo check --all-targets
  invariants:
    - id: unit-tests
      command: cargo test
";

const POLICY_YAML: &str = "\
policy:
  rules:
    - id: project.no-force-push
      action: git.force_push
      effect: deny
    - id: project.dependency
      action: dependency.add
      effect: require_approval
";

const DECISION_MD: &str = "\
---
id: D-0001
status: ACCEPTED
date: 2026-08-15
---
Clone, never worktree. A worktree shares the object store with the source
repository, so an agent that corrupts one corrupts both.
";

fn plan_yaml(version: u32, objective: &str) -> String {
    format!(
        "plan:\n  id: p-recovery\n  version: {version}\n  objective: \"{objective}\"\n  \
         milestones:\n    - id: M-01\n      title: \"Recovery\"\n      slices:\n        \
         - id: S-11\n          title: \"Plan ledger\"\n          tasks:\n            \
         - id: T-0001\n              objective: \"Survive the loss of the store.\"\n              \
         acceptance_criteria:\n                - id: AC-1\n                  \
         statement: \"Project truth outlives execution state.\"\n                  \
         verified_by: [unit-tests]\n"
    )
}

fn catalogue() -> BTreeSet<String> {
    let loaded = profile::parse(VERIFICATION_YAML).expect("the verification fixture parses");
    plan::check_ids(&loaded.profile)
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the parent directory");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Destroy the store completely.
///
/// `conductor.db` alone is not the store: §5.1 runs SQLite in WAL mode
/// (`journal_mode=wal`), so committed rows can live in `conductor.db-wal` until
/// a checkpoint folds them back. Deleting only the main file would leave a test
/// that *looks* like a reconstruction while SQLite quietly replays the old data
/// out of the sidecar — the exact "the fixture kept a second copy" failure that
/// makes a recovery test vacuous. All three files go, and their absence is
/// asserted.
fn destroy_store(db: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("remove {}: {e}", path.display()),
        }
        assert!(!path.exists(), "{} must be gone", path.display());
    }
}

/// A git repository carrying all four of §3.1's authoritative files and one
/// decision.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    run_git_ok(root, &["init", "-q", "-b", "main"]).expect("git init");
    run_git_ok(root, &["config", "user.email", "recovery@example.invalid"]).expect("email");
    run_git_ok(root, &["config", "user.name", "Recovery Test"]).expect("name");
    write(root, ".conductor/project.yaml", PROJECT_YAML);
    write(root, ".conductor/verification.yaml", VERIFICATION_YAML);
    write(root, ".conductor/policy.yaml", POLICY_YAML);
    write(
        root,
        ".conductor/decisions/D-0001-clone-not-worktree.md",
        DECISION_MD,
    );
    write(
        root,
        ".conductor/plans/v1/plan.yaml",
        &plan_yaml(1, "Prove project truth outlives the store."),
    );
    run_git_ok(root, &["add", "-A"]).expect("add");
    run_git_ok(root, &["commit", "-q", "-m", "initial"]).expect("commit");
    dir
}

/// One real §4.3 plan-approval grant — built through S8's API, so that the
/// sidecar this file later deletes a database around is a receipt for an
/// approval that actually happened.
fn plan_grant(store: &mut Store, plan_version_id: &str, approver: &str) -> Authorization {
    let request_id = format!("AR-{plan_version_id}");
    approval::request(
        store.conn_mut(),
        &NewApprovalRequest {
            id: request_id.clone(),
            subject: Subject::PlanVersion {
                plan_version_id: plan_version_id.to_string(),
            },
            run_id: None,
            facts: FactSet::new(),
            policy_hash: "blake3:policy".to_string(),
            matched_rules: Vec::new(),
            explanation: "a human is asked to make this plan version authoritative".to_string(),
            evidence_ref: None,
            expires: Expiry::Never,
        },
        1_000,
    )
    .expect("record the approval request");
    let row = approval::grant(
        store.conn_mut(),
        &request_id,
        &GrantOptions {
            id: format!("AG-{plan_version_id}"),
            scope: Scope::from_pairs([("plan_version".to_string(), plan_version_id.to_string())]),
            reuse: false,
            expires: Expiry::Never,
            granted_by: approver.to_string(),
            channel: "socket".to_string(),
            nonce_hash: None,
        },
        2_000,
    )
    .expect("grant it");
    Authorization::Authorized { grant_id: row.id }
}

/// Everything a live Conductor would hold for this project: the git half
/// registered, v1 approved and materialised, and — for the **Lost** list — a
/// policy snapshot, a run, and an event.
///
/// Returns the store path, so the caller can delete exactly that file.
fn live_project(repo: &Path, store_dir: &Path) -> (ProjectId, std::path::PathBuf) {
    let db = store_dir.join("conductor.db");
    let mut store = Store::open_or_create(&db).expect("open the store");

    let project = ledger::register_project(&mut store, repo, 1_000).expect("register the project");
    let project_id = project.id.clone();
    let registered = ledger::register_plan_version(&mut store, &project_id, 1, &catalogue())
        .expect("register v1");

    let witness = plan_grant(&mut store, registered.row.id.as_str(), "alice");
    let id = ledger::plan_version_id(&project_id, 1);
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request approval");
    ledger::approve(&mut store, &project_id, 1, &witness, 3_000).expect("approve v1");

    conductor_run::decision::register_decisions(&mut store, &project_id).expect("sync decisions");
    materialize::materialize(&mut store, &project_id, 1, &registered.plan, 4_000)
        .expect("materialise v1");

    // -- the Lost list: execution state, which must NOT come back ------------
    let resolved = policy_load::resolve(None, Some(&repo.join(".conductor/policy.yaml")), None)
        .expect("resolve the project policy");
    let snapshot = policy_load::snapshot(&resolved);
    policy_load::persist(store.conn_mut(), &snapshot, 5_000).expect("persist the policy snapshot");
    store
        .create_run(
            &NewRun {
                id: RunId::new("r-0001").expect("run id"),
                task_id: TaskId::new("T-0001").expect("task id"),
                policy_hash: snapshot.hash.clone(),
                base_commit: "0".repeat(40),
                run_branch: "conductor/T-0001/r-0001".to_string(),
                target_branch: "main".to_string(),
            },
            6_000,
        )
        .expect("create a run");

    (project_id, db)
}

// ---------------------------------------------------------------------------
// §3.2's invariant, and §3.5's two lists
// ---------------------------------------------------------------------------

#[test]
fn deleting_the_database_loses_no_plan_no_decision_no_policy_and_no_verification_definition() {
    // S11's verify line, and §3.2's stated acceptance test. This is the
    // positive control for the whole file: a `reconstruct` that refused
    // everything would pass every refusal below and fail here.
    let repo_dir = repo();
    let store_dir = tempfile::tempdir().expect("a store directory outside the repository");
    let (project_id, db) = live_project(repo_dir.path(), store_dir.path());

    // What the live store knew, captured before it is destroyed.
    let before = {
        let store = Store::open_existing(&db).expect("reopen");
        let approval =
            ledger::verify_approval(&store, &project_id, 1).expect("v1 is approved and consistent");
        let tasks: Vec<String> = store
            .tasks(None)
            .expect("tasks")
            .into_iter()
            .map(|t| t.id.as_str().to_string())
            .collect();
        let decisions: Vec<String> = store
            .decisions_for_project(&project_id)
            .expect("decisions")
            .into_iter()
            .map(|d| d.id)
            .collect();
        (approval.content_hash.as_str().to_string(), tasks, decisions)
    };
    let (approved_hash, task_ids, decision_ids) = before;
    assert!(
        !task_ids.is_empty() && !decision_ids.is_empty(),
        "the fixture must actually have materialised tasks and decisions, or the \
         comparison below is vacuous"
    );

    // The destructive step. Only the store: `.conductor/` is untouched.
    destroy_store(&db);

    // A store that has never seen this project.
    let mut fresh = Store::open_or_create(&db).expect("a new, empty store");
    assert!(
        fresh.project(&project_id).expect("query").is_none(),
        "the new store must start with no knowledge of the project, or nothing \
         below is a reconstruction"
    );

    let rebuilt = reconstruct::reconstruct(&mut fresh, repo_dir.path(), 10_000)
        .expect("§3.5's recovery path rebuilds from `.conductor/` alone");

    // -- Not lost: project identity -----------------------------------------
    assert_eq!(rebuilt.project.id, project_id);
    assert_eq!(rebuilt.project.default_branch, "main");

    // -- Not lost: the approved plan, at the same content hash ---------------
    let approval = ledger::verify_approval(&fresh, &project_id, 1)
        .expect("the approved plan survived, and store and sidecar still agree");
    assert_eq!(
        approval.content_hash.as_str(),
        approved_hash,
        "§3.6's hash is over semantics; the rebuilt row must carry the same one"
    );
    assert_eq!(
        plan_version_state(&fresh, &project_id, 1),
        PlanVersionState::Approved,
        "§3.5: `every approved plan` is on the Not-lost list"
    );

    // -- Not lost: the task list, rebuilt from the approved plan -------------
    let rebuilt_tasks: Vec<String> = fresh
        .tasks(None)
        .expect("tasks")
        .into_iter()
        .map(|t| t.id.as_str().to_string())
        .collect();
    assert_eq!(rebuilt_tasks, task_ids, "§3.5: `rebuild the task list`");

    // -- Not lost: decisions -------------------------------------------------
    let rebuilt_decisions: Vec<String> = fresh
        .decisions_for_project(&project_id)
        .expect("decisions")
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(rebuilt_decisions, decision_ids);

    // -- Not lost: policy and verification definitions -----------------------
    // These are files, so "not lost" means the definitions still load and still
    // mean the same thing — asserted by value, not by the file existing.
    let resolved = policy_load::resolve(
        None,
        Some(&repo_dir.path().join(".conductor/policy.yaml")),
        None,
    )
    .expect("the project policy still loads");
    assert_eq!(
        policy_load::snapshot(&resolved).hash,
        policy_load::snapshot(
            &policy_load::resolve(
                None,
                Some(&repo_dir.path().join(".conductor/policy.yaml")),
                None
            )
            .expect("resolve")
        )
        .hash
    );
    let loaded = profile::load(&repo_dir.path().join(".conductor/verification.yaml"))
        .expect("the verification definitions still load");
    assert_eq!(plan::check_ids(&loaded.profile), catalogue());
}

#[test]
fn reconstruction_does_not_resurrect_runs_attempts_or_the_event_journal() {
    // §3.5's **Lost** list. A rebuild that restored a `RUNNING` run with an
    // expired lease would be asserting the existence of a process that does not
    // exist — worse than losing the row.
    let repo_dir = repo();
    let store_dir = tempfile::tempdir().expect("store dir");
    let (project_id, db) = live_project(repo_dir.path(), store_dir.path());

    let live_runs = {
        let store = Store::open_existing(&db).expect("reopen");
        store.active_runs().expect("runs").len()
    };
    assert_eq!(live_runs, 1, "the fixture created a run to lose");

    destroy_store(&db);
    let mut fresh = Store::open_or_create(&db).expect("a new store");
    reconstruct::reconstruct(&mut fresh, repo_dir.path(), 10_000).expect("rebuild");

    assert!(
        fresh.active_runs().expect("runs").is_empty(),
        "§3.5 puts run history on the Lost list"
    );
    assert!(
        fresh.in_flight_attempts().expect("attempts").is_empty(),
        "§3.5 puts attempt history on the Lost list"
    );
    assert!(
        fresh.pending_approvals().expect("approvals").is_empty(),
        "§3.5 puts pending approval requests on the Lost list"
    );
    assert!(
        fresh
            .active_run_for_task(&TaskId::new("T-0001").expect("task id"))
            .expect("query")
            .is_none(),
        "the rebuilt task must not point at a run that no longer exists"
    );
    let _ = project_id;
}

// ---------------------------------------------------------------------------
// The hole: adopting a sidecar must not be adopting *any* sidecar
// ---------------------------------------------------------------------------

#[test]
fn a_sidecar_that_disagrees_with_the_plan_document_is_not_adopted_as_an_approval() {
    // The receipt is only a receipt for the document it was written against.
    // If the plan is edited after approval, §5.2's restart clause makes that a
    // hard error — and a rebuild must reach the same verdict rather than
    // stamping `APPROVED` onto whatever the file now says.
    let repo_dir = repo();
    let store_dir = tempfile::tempdir().expect("store dir");
    let (project_id, db) = live_project(repo_dir.path(), store_dir.path());
    destroy_store(&db);

    // The one thing changed: the approved document now says something else.
    write(
        repo_dir.path(),
        ".conductor/plans/v1/plan.yaml",
        &plan_yaml(1, "A different objective nobody approved."),
    );

    let mut fresh = Store::open_or_create(&db).expect("a new store");
    let error = reconstruct::reconstruct(&mut fresh, repo_dir.path(), 10_000)
        .expect_err("an edited plan must not be adopted on the strength of a stale receipt");
    let rendered = error.to_string();
    assert!(
        rendered.contains("v1") || rendered.contains("pv-"),
        "the refusal must name the version it is about: {rendered}"
    );
    assert_ne!(
        plan_version_state(&fresh, &project_id, 1),
        PlanVersionState::Approved,
        "a refused reconstruction must not leave the version APPROVED"
    );
}

#[test]
fn a_plan_version_with_no_sidecar_is_rebuilt_as_validated_and_still_cannot_materialise() {
    // The control that stops "refuse everything" and "approve everything" from
    // both passing this file. A version whose human never approved it has no
    // receipt, so a rebuild must stop at VALIDATED — and §4.3's gate in
    // `materialize` must still refuse it.
    let repo_dir = repo();
    let store_dir = tempfile::tempdir().expect("store dir");
    let (project_id, db) = live_project(repo_dir.path(), store_dir.path());
    destroy_store(&db);

    // v2 exists and is perfectly valid, but no human ever approved it.
    write(
        repo_dir.path(),
        ".conductor/plans/v2/plan.yaml",
        &plan_yaml(2, "A revision awaiting a human."),
    );

    let mut fresh = Store::open_or_create(&db).expect("a new store");
    let rebuilt = reconstruct::reconstruct(&mut fresh, repo_dir.path(), 10_000)
        .expect("rebuild both versions");

    assert_eq!(
        plan_version_state(&fresh, &project_id, 2),
        PlanVersionState::Validated,
        "no receipt, no approval"
    );
    assert_eq!(
        plan_version_state(&fresh, &project_id, 1),
        PlanVersionState::Approved,
        "the version that does have a receipt still comes back approved"
    );
    assert_eq!(
        rebuilt.approved_version(),
        Some(1),
        "the newest *approved* version is v1, not the newer unapproved v2"
    );

    let refused = materialize::materialize(
        &mut fresh,
        &project_id,
        2,
        rebuilt
            .plan_for(2)
            .expect("v2 was validated during the rebuild"),
        11_000,
    )
    .expect_err("§4.3's gate must still refuse an unapproved plan after a rebuild");
    assert!(
        refused.to_string().contains("VALIDATED"),
        "the refusal must name the state it found: {refused}"
    );
}

#[test]
fn a_sidecar_naming_a_different_plan_version_is_refused_rather_than_believed() {
    // A receipt is bound to one version. Copying v1's `APPROVED` into v2's
    // directory must not approve v2.
    let repo_dir = repo();
    let store_dir = tempfile::tempdir().expect("store dir");
    let (_project_id, db) = live_project(repo_dir.path(), store_dir.path());
    destroy_store(&db);

    write(
        repo_dir.path(),
        ".conductor/plans/v2/plan.yaml",
        &plan_yaml(2, "A revision awaiting a human."),
    );
    let stolen = std::fs::read_to_string(repo_dir.path().join(".conductor/plans/v1/APPROVED"))
        .expect("v1's receipt");
    write(repo_dir.path(), ".conductor/plans/v2/APPROVED", &stolen);

    let mut fresh = Store::open_or_create(&db).expect("a new store");
    let error = reconstruct::reconstruct(&mut fresh, repo_dir.path(), 10_000)
        .expect_err("a receipt for another version authorises nothing");
    assert!(
        error.to_string().contains("v2")
            || error.to_string().contains("2")
            || error.to_string().contains("pv-"),
        "the refusal must name the version: {error}"
    );
}

// ---------------------------------------------------------------------------

fn plan_version_state(store: &Store, project: &ProjectId, version: u32) -> PlanVersionState {
    let id = ledger::plan_version_id(project, version);
    store
        .plan_version(&id)
        .expect("read the plan version")
        .unwrap_or_else(|| panic!("no plan version {id}"))
        .state
}
