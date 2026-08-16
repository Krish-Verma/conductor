//! Task materialisation — master plan §3.6, §5.1, §5.2, acceptance row 21.
//!
//! # What these tests are guarding against
//!
//! Materialisation is the step that turns an approved plan into the rows a run
//! is dispatched from. Three separate failures are possible and each has a
//! different shape, so each gets its own group below.
//!
//! 1. **A row that does not say what the plan said.** §5.1's `task` table is an
//!    *index* over the plan, not a second source of truth (§3.1 keeps git
//!    authoritative), so every column has to be traceable to a field of the
//!    document.
//!    [`materializing_a_validated_plan_writes_one_row_per_task_carrying_every_field_the_plan_declares`]
//!    is the positive control the refusals below are all measured against, and
//!    it asserts the JSON columns **byte for byte** rather than after decoding:
//!    a canonical encoding that only round-trips is not canonical.
//!
//! 2. **A row that is not reproducible.** Ruling 3 makes rebuildability the
//!    property that lets the index be thrown away and recomputed.
//!    [`one_plan_materialized_into_two_separate_stores_produces_byte_identical_json_columns`]
//!    is deliberately **not** "call the encoder twice and compare it to
//!    itself": it drives the whole public entry point into two independent
//!    databases, at two different wall clocks, and compares what landed.
//!
//! 3. **A revision that runs over work in flight.** Acceptance row 21 —
//!    *"approve v4 during a v3 run → run keeps `plan_version=3` → finish under
//!    v3; new tasks under v4"*.
//!    [`a_task_with_a_run_in_flight_keeps_its_old_plan_version_while_one_without_it_is_superseded`]
//!    is the row, and the two tasks in its fixture differ in **exactly one
//!    thing** — whether a non-terminal run exists — so a materializer that
//!    superseded everything, or nothing, fails it either way.
//!
//! 4. **A document nobody approved becoming work.** §4.3 gives a plan approval
//!    exactly one thing to authorize — *"a plan version becoming
//!    authoritative"* — and materialisation is that moment.
//!    [`materializing_a_plan_version_no_human_has_approved_is_refused`] carries
//!    its own positive control in its body, because a gate that refused
//!    everything would satisfy the refusal on its own.
//!
//! Every other test in this file goes through [`approved`], which moves the
//! version through §5.2's machine and a **real** §4.3 grant rather than writing
//! `state = 'APPROVED'`. So the gate is exercised against the shape a human at
//! the control socket actually produces, on every single path.
//!
//! # The positive controls
//!
//! A materializer that refused every plan would pass every rejection test here.
//! [`materializing_a_validated_plan_writes_one_row_per_task_carrying_every_field_the_plan_declares`]
//! is the control for the refusals, and
//! [`a_plan_that_section_3_7_refuses_never_becomes_a_validated_plan_to_materialize`]
//! is labelled in its own body as a control that **cannot** fail first: the
//! invariant it names is enforced by [`materialize`]'s signature, and a test
//! cannot watch a type error happen at runtime.

use std::collections::BTreeSet;
use std::path::Path;

use conductor_core::{PlanVersionState, ProjectId, RunId, TaskId, TaskState};
use conductor_git::run_git_ok;
use conductor_run::approval::{
    self, Authorization, Expiry, GrantOptions, NewApprovalRequest, Subject,
};
use conductor_run::plan::materialize::{MaterializeError, materialize};
use conductor_run::plan::{self, ledger, project};
use conductor_run::policy::load;
use conductor_run::policy::model::{FactSet, Origin, Scope};
use conductor_run::verify::profile;
use conductor_store::{NewRun, NewTask, Store};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// §3.1's project file. The `execution_requirements:` block at the top level is
/// §4.2's, and it is what a task that declares none inherits.
const PROJECT_YAML: &str = "\
project:
  id: p-fixture
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

/// v1: two tasks that between them exercise every column materialisation
/// writes — an override and an inherited block, a declared action list and an
/// empty one, a dependency and none, a bound criterion and a `manual: true`
/// one, a non-default attempt budget and the schema's default.
const PLAN_V1: &str = r#"
plan:
  id: p-fixture
  version: 1
  objective: "Materialize the plan."
  milestones:
    - id: M-01
      title: "Materialisation"
      slices:
        - id: S-11
          title: "Task materialisation"
          tasks:
            - id: T-0001
              objective: "Write the row."
              rationale: "Section 5.1 asks for one row per task."
              scope:
                allowed_globs: ["crates/**", "docs/**"]
                forbidden_globs: [".conductor/**"]
              verification_profile: "verification.yaml"
              attempt_budget: 5
              actions: ["git.push", "test.run"]
              acceptance_criteria:
                - id: AC-1
                  statement: "The row exists."
                  verified_by: ["unit-tests"]
                - id: AC-2
                  statement: "A human read it."
                  manual: true
            - id: T-0002
              objective: "Depend on the first."
              depends_on: ["T-0001"]
              scope:
                allowed_globs: ["docs/**"]
              verification_profile: "verification.yaml"
              execution_requirements: |
                execution_requirements:
                  control_surface: hard
              acceptance_criteria:
                - id: AC-1
                  statement: "It ran second."
                  verified_by: ["typecheck"]
"#;

/// v1 with a task that declares no `scope:` at all, so §3.1's project-level
/// `scope_defaults` are the only thing that can give it one.
const PLAN_V1_NO_SCOPE: &str = r#"
plan:
  id: p-fixture
  version: 1
  objective: "Declare no scope."
  milestones:
    - id: M-01
      title: "Materialisation"
      slices:
        - id: S-11
          title: "Task materialisation"
          tasks:
            - id: T-0001
              objective: "Inherit the project's scope."
              verification_profile: "verification.yaml"
              acceptance_criteria:
                - id: AC-1
                  statement: "The row exists."
                  verified_by: ["unit-tests"]
"#;

/// v2 as a revision that replaces v1's work: neither of v1's ids appears, so
/// every v1 task is a supersession candidate and the only thing that can save
/// one is acceptance row 21.
const PLAN_V2_REPLACING: &str = r#"
plan:
  id: p-fixture
  version: 2
  objective: "Replace the first attempt."
  milestones:
    - id: M-01
      title: "Materialisation"
      slices:
        - id: S-12
          title: "Revised slice"
          tasks:
            - id: T-0003
              objective: "Do it differently."
              scope:
                allowed_globs: ["crates/**"]
              verification_profile: "verification.yaml"
              acceptance_criteria:
                - id: AC-1
                  statement: "The revision landed."
                  verified_by: ["unit-tests"]
"#;

/// v2 as a revision that keeps `T-0001`'s meaning — §3.6's *"a revision that
/// preserves a task's meaning preserves its ID"*.
const PLAN_V2_KEEPING_T0001: &str = r#"
plan:
  id: p-fixture
  version: 2
  objective: "Keep the first task."
  milestones:
    - id: M-01
      title: "Materialisation"
      slices:
        - id: S-11
          title: "Task materialisation"
          tasks:
            - id: T-0001
              objective: "Write the row."
              rationale: "Section 5.1 asks for one row per task."
              scope:
                allowed_globs: ["crates/**", "docs/**"]
                forbidden_globs: [".conductor/**"]
              verification_profile: "verification.yaml"
              attempt_budget: 5
              actions: ["git.push", "test.run"]
              acceptance_criteria:
                - id: AC-1
                  statement: "The row exists."
                  verified_by: ["unit-tests"]
                - id: AC-2
                  statement: "A human read it."
                  manual: true
"#;

/// §3.7's catalogue, assembled the way clarification 3 says a caller must.
fn catalogue() -> BTreeSet<String> {
    let loaded = profile::parse(VERIFICATION_YAML).expect("the verification fixture parses");
    plan::check_ids(&loaded.profile)
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the parent directory");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// A git repository with one commit, `.conductor/project.yaml`,
/// `.conductor/verification.yaml` and plan v1.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    run_git_ok(root, &["init", "-q", "-b", "main"]).expect("git init");
    run_git_ok(root, &["config", "user.email", "plan@example.invalid"]).expect("email");
    run_git_ok(root, &["config", "user.name", "Materialize Test"]).expect("name");
    write(root, ".conductor/project.yaml", PROJECT_YAML);
    write(root, ".conductor/verification.yaml", VERIFICATION_YAML);
    write(root, &plan::plan_path(1), PLAN_V1);
    run_git_ok(root, &["add", "-A"]).expect("add");
    run_git_ok(root, &["commit", "-q", "-m", "initial"]).expect("commit");
    dir
}

fn store(dir: &tempfile::TempDir) -> Store {
    Store::open_or_create(dir.path().join("conductor.db")).expect("open the store")
}

/// A registered project with v1 registered and `VALIDATED` — approved by
/// nobody.
///
/// Used only where the *absence* of an approval is the subject. Everything else
/// goes through [`approved`], because §4.3 makes a plan approval the thing that
/// authorises *"a plan version becoming authoritative"* and materialisation is
/// that moment.
fn registered(dir: &tempfile::TempDir) -> (Store, ProjectId, plan::ValidatedPlan) {
    let mut store = store(dir);
    let project = ledger::register_project(&mut store, dir.path(), 1_000).expect("register");
    let registered = ledger::register_plan_version(&mut store, &project.id, 1, &catalogue())
        .expect("register v1");
    (store, project.id, registered.plan)
}

/// A registered project whose v1 a human has approved at the control socket.
fn approved(dir: &tempfile::TempDir) -> (Store, ProjectId, plan::ValidatedPlan) {
    let (mut store, project, plan) = registered(dir);
    approve(&mut store, &project, 1);
    (store, project, plan)
}

/// Write, register and return version `version` of the plan, approved.
fn register_and_approve_version(
    store: &mut Store,
    dir: &tempfile::TempDir,
    project: &ProjectId,
    version: u32,
    yaml: &str,
) -> plan::ValidatedPlan {
    write(dir.path(), &plan::plan_path(version), yaml);
    let plan = ledger::register_plan_version(store, project, version, &catalogue())
        .unwrap_or_else(|e| panic!("register v{version}: {e}"))
        .plan;
    approve(store, project, version);
    plan
}

/// Move a plan version to `APPROVED` through §5.2's machine and a **real**
/// §4.3 plan grant.
///
/// Built through S8's own API rather than by writing `state = 'APPROVED'`,
/// so the gate under test is exercised against the shape a human at the
/// control socket actually produces — `plan_ledger.rs` builds its witnesses
/// the same way and for the same reason.
fn approve(store: &mut Store, project: &ProjectId, version: u32) {
    let id = ledger::plan_version_id(project, version);
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request approval");
    let request_id = format!("AR-{id}");
    approval::request(
        store.conn_mut(),
        &NewApprovalRequest {
            id: request_id.clone(),
            subject: Subject::PlanVersion {
                plan_version_id: id.as_str().to_string(),
            },
            run_id: None,
            facts: FactSet::new(),
            policy_hash: "blake3:policy".to_string(),
            matched_rules: Vec::new(),
            explanation: "a human is asked to make this plan version authoritative".to_string(),
            evidence_ref: None,
            // §4.3: a plan approval does not expire, and the store refuses a TTL.
            expires: Expiry::Never,
        },
        1_000,
    )
    .expect("record the approval request");
    let row = approval::grant(
        store.conn_mut(),
        &request_id,
        &GrantOptions {
            id: format!("AG-{id}"),
            scope: Scope::from_pairs([("plan_version".to_string(), id.as_str().to_string())]),
            reuse: false,
            expires: Expiry::Never,
            granted_by: "krish".to_string(),
            channel: "socket".to_string(),
            nonce_hash: None,
        },
        2_000,
    )
    .expect("grant it");
    let witness = Authorization::Authorized { grant_id: row.id };
    ledger::approve(store, project, version, &witness, 2_500).expect("approve");
}

/// A `policy_snapshot` row, so that a `run` can reference one.
fn seed_policy(store: &mut Store) -> String {
    let document = load::parse_document(
        "policy:\n  rules:\n    - id: global.push\n      action: git.push\n      \
         effect: require_approval\n",
        Origin::Global,
    )
    .expect("parse the policy fixture");
    let resolved = load::resolve_documents(Some(document), None, None).expect("resolve");
    let snapshot = load::snapshot(&resolved);
    load::persist(store.conn_mut(), &snapshot, 0).expect("persist the snapshot");
    snapshot.hash
}

/// A non-terminal (`READY`) run against `task` — acceptance row 21's "in
/// flight".
fn start_run(store: &mut Store, task: &str, run: &str) {
    let policy_hash = seed_policy(store);
    store
        .create_run(
            &NewRun {
                id: RunId::new(run).expect("run id"),
                task_id: TaskId::new(task).expect("task id"),
                policy_hash,
                base_commit: "0000000000000000000000000000000000000000".to_string(),
                run_branch: format!("conductor/{task}/{run}"),
                target_branch: "main".to_string(),
            },
            2_000,
        )
        .expect("create the run");
}

fn task_id(id: &str) -> TaskId {
    TaskId::new(id).expect("task id")
}

// ---------------------------------------------------------------------------
// the row a plan becomes
// ---------------------------------------------------------------------------

/// The positive control. Every refusal below is this fixture with one thing
/// changed, so without it a materializer that refused everything would score a
/// clean sheet.
#[test]
fn materializing_a_validated_plan_writes_one_row_per_task_carrying_every_field_the_plan_declares() {
    let dir = repo();
    let (mut store, project, plan) = approved(&dir);

    let outcome = materialize(&mut store, &project, 1, &plan, 3_000).expect("materialize v1");

    assert_eq!(outcome.plan_version_id.as_str(), "p-fixture-v1");
    let created: Vec<&str> = outcome.created.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(created, vec!["T-0001", "T-0002"], "one row per plan task");
    assert!(outcome.superseded.is_empty(), "nothing to supersede");
    assert!(outcome.carried.is_empty(), "nothing pre-existing to carry");

    let first = store
        .task(&task_id("T-0001"))
        .expect("read")
        .expect("T-0001");
    assert_eq!(first.plan_version_id, "p-fixture-v1");
    assert_eq!(first.slice_id, "S-11");
    assert_eq!(first.state, TaskState::Pending);
    assert_eq!(first.scope_globs, vec!["crates/**", "docs/**"]);
    assert_eq!(first.verification_profile, "verification.yaml");
    assert_eq!(first.attempt_budget, 5);

    // Byte-for-byte, not after decoding: the JSON is a canonical encoding, and
    // a canonical encoding that only round-trips is not canonical. Keys are
    // sorted; elements keep declaration order, which §3.6 makes content.
    assert_eq!(
        store.declared_actions(&first.id).expect("read"),
        Some(r#"["git.push","test.run"]"#.to_string())
    );
    assert_eq!(
        store.depends_on(&first.id).expect("read"),
        Some("[]".to_string())
    );
    assert_eq!(
        store.acceptance_criteria(&first.id).expect("read"),
        Some(
            r#"[{"id":"AC-1","manual":false,"statement":"The row exists.","verified_by":["unit-tests"]},{"id":"AC-2","manual":true,"statement":"A human read it.","verified_by":[]}]"#
                .to_string()
        )
    );

    let second = store
        .task(&task_id("T-0002"))
        .expect("read")
        .expect("T-0002");
    assert_eq!(second.slice_id, "S-11");
    assert_eq!(second.scope_globs, vec!["docs/**"]);
    assert_eq!(second.attempt_budget, 3, "§5.1's default");
    assert_eq!(
        store.depends_on(&second.id).expect("read"),
        Some(r#"["T-0001"]"#.to_string())
    );
    assert_eq!(
        store.acceptance_criteria(&second.id).expect("read"),
        Some(
            r#"[{"id":"AC-1","manual":false,"statement":"It ran second.","verified_by":["typecheck"]}]"#
                .to_string()
        )
    );
}

/// Ruling 4, and the reason `declared_actions` is nullable at all: `NULL` means
/// "no plan document was ever read for this row" and `'[]'` means "a plan was
/// read and it declares none". A materializer that left the column `NULL` for
/// an empty list would erase the difference the approval gate needs.
#[test]
fn a_task_that_declares_no_actions_materializes_as_declaring_none_rather_than_as_never_materialized()
 {
    let dir = repo();
    let (mut store, project, plan) = approved(&dir);
    materialize(&mut store, &project, 1, &plan, 3_000).expect("materialize v1");

    let second = task_id("T-0002");
    assert_eq!(
        store.declared_actions(&second).expect("read"),
        Some("[]".to_string()),
        "materialized and declares none — not NULL"
    );

    // The control for the distinction: a row created outside materialisation
    // has never had a plan read for it, and reads back NULL.
    store
        .create_task(
            &NewTask {
                id: task_id("T-9999"),
                plan_version_id: "p-fixture-v1".to_string(),
                slice_id: "S-11".to_string(),
                scope_globs: vec![],
                verification_profile: "verification.yaml".to_string(),
                attempt_budget: 3,
            },
            3_000,
        )
        .expect("create a pre-S11-shaped row");
    assert_eq!(
        store.declared_actions(&task_id("T-9999")).expect("read"),
        None
    );
}

/// §3.1 makes `.conductor/project.yaml` authoritative for *"scope defaults"*,
/// and a default that nothing applies is a knob an operator can set and watch
/// do nothing.
///
/// Both directions, in one test: the task that declares its own globs keeps
/// them, and the task that declares none gets the project's rather than the
/// empty list that would put every path it touches out of scope.
#[test]
fn a_task_that_declares_no_scope_inherits_the_projects_scope_defaults() {
    let inheriting = repo();
    write(inheriting.path(), &plan::plan_path(1), PLAN_V1_NO_SCOPE);
    let (mut store, project, plan) = approved(&inheriting);
    materialize(&mut store, &project, 1, &plan, 3_000).expect("materialize v1");
    assert_eq!(
        store
            .task(&task_id("T-0001"))
            .expect("read")
            .expect("row")
            .scope_globs,
        vec!["crates/**"],
        "the project's scope_defaults.allowed_globs"
    );

    // The control: a task that declares its own scope is not overwritten by the
    // defaults, so the inheritance above is a fallback and not a clobber.
    let declaring = repo();
    let (mut store, project, plan) = approved(&declaring);
    materialize(&mut store, &project, 1, &plan, 3_000).expect("materialize v1");
    assert_eq!(
        store
            .task(&task_id("T-0001"))
            .expect("read")
            .expect("row")
            .scope_globs,
        vec!["crates/**", "docs/**"],
        "the task's own globs, not the project's"
    );
}

/// §4.2's *"or per-task override"*: the task's own block wins, and a task that
/// declares none inherits the project's rather than being left ungated.
#[test]
fn a_task_inherits_the_projects_execution_requirements_and_its_own_override_wins() {
    let dir = repo();
    let (mut store, project, plan) = approved(&dir);
    materialize(&mut store, &project, 1, &plan, 3_000).expect("materialize v1");

    let from_file = project::parse(PROJECT_YAML)
        .expect("the project fixture parses")
        .execution_requirements
        .expect("the fixture declares a block");
    assert_eq!(
        store
            .execution_requirements(&task_id("T-0001"))
            .expect("read"),
        Some(from_file),
        "T-0001 declares no override, so it inherits the project's block"
    );
    assert_eq!(
        store
            .execution_requirements(&task_id("T-0002"))
            .expect("read"),
        Some("execution_requirements:\n  control_surface: hard\n".to_string()),
        "T-0002's own block wins"
    );
}

// ---------------------------------------------------------------------------
// determinism
// ---------------------------------------------------------------------------

/// Ruling 3: the rows are an index, and an index has to be rebuildable from the
/// document it indexes.
///
/// Two **separate** stores, two different wall clocks. Calling one encoder
/// twice and comparing it to itself would pass even if a timestamp had leaked
/// into the payload, which is the failure this is for.
#[test]
fn one_plan_materialized_into_two_separate_stores_produces_byte_identical_json_columns() {
    let first_dir = repo();
    let (mut first, first_project, first_plan) = approved(&first_dir);
    materialize(&mut first, &first_project, 1, &first_plan, 3_000).expect("materialize into one");

    let second_dir = repo();
    let (mut second, second_project, second_plan) = approved(&second_dir);
    materialize(&mut second, &second_project, 1, &second_plan, 9_876_543_210)
        .expect("materialize into the other");

    for id in ["T-0001", "T-0002"] {
        let id = task_id(id);
        assert_eq!(
            first.declared_actions(&id).expect("read"),
            second.declared_actions(&id).expect("read"),
            "{id}: declared_actions"
        );
        assert_eq!(
            first.depends_on(&id).expect("read"),
            second.depends_on(&id).expect("read"),
            "{id}: depends_on"
        );
        assert_eq!(
            first.acceptance_criteria(&id).expect("read"),
            second.acceptance_criteria(&id).expect("read"),
            "{id}: acceptance_criteria"
        );
        assert_eq!(
            first.task(&id).expect("read").expect("row").scope_globs,
            second.task(&id).expect("read").expect("row").scope_globs,
            "{id}: scope_globs"
        );
    }
}

/// §3.5's recovery path re-reads `.conductor/` and rebuilds; that only works if
/// re-running materialisation is not itself a change.
#[test]
fn materializing_the_same_plan_twice_creates_nothing_the_second_time_and_changes_no_column() {
    let dir = repo();
    let (mut store, project, plan) = approved(&dir);
    materialize(&mut store, &project, 1, &plan, 3_000).expect("materialize once");
    let before: Vec<Option<String>> = ["T-0001", "T-0002"]
        .iter()
        .map(|id| store.acceptance_criteria(&task_id(id)).expect("read"))
        .collect();

    let again = materialize(&mut store, &project, 1, &plan, 4_000).expect("materialize again");

    assert!(again.created.is_empty(), "nothing is created twice");
    assert!(
        again.superseded.is_empty(),
        "a plan does not supersede itself"
    );
    let carried: Vec<&str> = again.carried.iter().map(|task| task.id.as_str()).collect();
    assert_eq!(carried, vec!["T-0001", "T-0002"]);
    assert_eq!(store.tasks(None).expect("read").len(), 2, "no duplicates");

    let after: Vec<Option<String>> = ["T-0001", "T-0002"]
        .iter()
        .map(|id| store.acceptance_criteria(&task_id(id)).expect("read"))
        .collect();
    assert_eq!(before, after);
}

// ---------------------------------------------------------------------------
// acceptance row 21
// ---------------------------------------------------------------------------

/// Acceptance row 21 — *"approve v4 during a v3 run → run keeps
/// `plan_version=3` → finish under v3; new tasks under v4"*.
///
/// The two v1 tasks differ in **exactly one** respect: `T-0001` has a
/// non-terminal run and `T-0002` does not. Both are `PENDING`, so §5.2 would
/// permit superseding either; a materializer that ignored the run would
/// supersede both and a materializer that superseded nothing would fail the
/// other half.
#[test]
fn a_task_with_a_run_in_flight_keeps_its_old_plan_version_while_one_without_it_is_superseded() {
    let dir = repo();
    let (mut store, project, v1) = approved(&dir);
    materialize(&mut store, &project, 1, &v1, 3_000).expect("materialize v1");
    start_run(&mut store, "T-0001", "r-0041");

    let v2 = register_and_approve_version(&mut store, &dir, &project, 2, PLAN_V2_REPLACING);
    let outcome = materialize(&mut store, &project, 2, &v2, 5_000).expect("materialize v2");

    let in_flight = store.task(&task_id("T-0001")).expect("read").expect("row");
    assert_eq!(
        in_flight.plan_version_id, "p-fixture-v1",
        "row 21: the in-flight task finishes under the version it started on"
    );
    assert_eq!(in_flight.state, TaskState::Pending, "not superseded");

    let idle = store.task(&task_id("T-0002")).expect("read").expect("row");
    assert_eq!(idle.state, TaskState::Superseded);
    assert_eq!(
        idle.plan_version_id, "p-fixture-v1",
        "a superseded task still records which plan it came from"
    );

    let fresh = store.task(&task_id("T-0003")).expect("read").expect("row");
    assert_eq!(fresh.plan_version_id, "p-fixture-v2", "new tasks under v2");

    let superseded: Vec<&str> = outcome.superseded.iter().map(TaskId::as_str).collect();
    assert_eq!(superseded, vec!["T-0002"]);
    let carried = outcome
        .carried
        .iter()
        .find(|task| task.id.as_str() == "T-0001")
        .expect("T-0001 is reported as carried");
    assert_eq!(
        carried.active_run.as_ref().map(RunId::as_str),
        Some("r-0041"),
        "and the report names the run that is why"
    );
}

/// The terminal-state question is answered by §5.2's machine, not by a list
/// spelled out here.
///
/// `conductor_core` refuses `RUNNING → SUPERSEDED` precisely because acceptance
/// row 21 says started work finishes under its own plan version. A materializer
/// that hardcoded "terminal means COMPLETE/CANCELLED/SUPERSEDED" and forced the
/// rest would hit that refusal and either error or, worse, skip the row without
/// saying so.
#[test]
fn a_task_the_state_machine_refuses_to_supersede_is_carried_rather_than_forced() {
    let dir = repo();
    let (mut store, project, v1) = approved(&dir);
    materialize(&mut store, &project, 1, &v1, 3_000).expect("materialize v1");
    // No run row at all — the only thing stopping supersession is §5.2.
    store
        .set_task_state(&task_id("T-0001"), TaskState::Ready)
        .expect("PENDING → READY");
    store
        .set_task_state(&task_id("T-0001"), TaskState::Running)
        .expect("READY → RUNNING");

    let v2 = register_and_approve_version(&mut store, &dir, &project, 2, PLAN_V2_REPLACING);
    let outcome = materialize(&mut store, &project, 2, &v2, 5_000).expect("materialize v2");

    let running = store.task(&task_id("T-0001")).expect("read").expect("row");
    assert_eq!(running.state, TaskState::Running);
    assert_eq!(running.plan_version_id, "p-fixture-v1");
    let carried = outcome
        .carried
        .iter()
        .find(|task| task.id.as_str() == "T-0001")
        .expect("T-0001 is reported as carried");
    assert_eq!(carried.active_run, None, "carried by §5.2, not by a run");
    let superseded: Vec<&str> = outcome.superseded.iter().map(TaskId::as_str).collect();
    assert_eq!(superseded, vec!["T-0002"]);
}

/// §3.6: *"A revision that preserves a task's meaning preserves its ID."* The
/// row that already exists is the task; re-creating it would collide on the
/// primary key, and superseding it would delete a task the current plan still
/// declares.
#[test]
fn a_task_id_the_next_version_still_declares_keeps_its_row_instead_of_being_superseded() {
    let dir = repo();
    let (mut store, project, v1) = approved(&dir);
    materialize(&mut store, &project, 1, &v1, 3_000).expect("materialize v1");

    let v2 = register_and_approve_version(&mut store, &dir, &project, 2, PLAN_V2_KEEPING_T0001);
    let outcome = materialize(&mut store, &project, 2, &v2, 5_000).expect("materialize v2");

    assert!(outcome.created.is_empty(), "v2 declares no new id");
    let carried: Vec<&str> = outcome
        .carried
        .iter()
        .map(|task| task.id.as_str())
        .collect();
    assert_eq!(carried, vec!["T-0001"]);
    let kept = store.task(&task_id("T-0001")).expect("read").expect("row");
    assert_eq!(
        kept.state,
        TaskState::Pending,
        "still the current plan's task"
    );
    // T-0002 is *not* in v2, and has no run, so it is the one that goes.
    let dropped = store.task(&task_id("T-0002")).expect("read").expect("row");
    assert_eq!(dropped.state, TaskState::Superseded);
}

// ---------------------------------------------------------------------------
// refusals
// ---------------------------------------------------------------------------

/// §4.3's table says a plan approval authorizes *"a plan version becoming
/// authoritative"*, and materialisation **is** that moment: it is where a
/// document turns into rows a run can be claimed from.
///
/// Without this gate `plan approve` authorizes nothing observable — a
/// `VALIDATED` plan would become work exactly as readily as an `APPROVED` one,
/// which makes §5.2's *"`APPROVED` only via a human at the control socket"* a
/// decoration. §3.7's clarification 4 refuses a document that declares its own
/// state for the same reason.
///
/// The **positive control is in the body**, deliberately: the same plan, the
/// same store, the same call, with a real §4.3 grant as the only difference. A
/// gate that refused everything would pass the first half and fail the second.
#[test]
fn materializing_a_plan_version_no_human_has_approved_is_refused() {
    let dir = repo();
    let (mut store, project, plan) = registered(&dir); // VALIDATED, approved by nobody

    let error = materialize(&mut store, &project, 1, &plan, 3_000).expect_err("refused");
    match &error {
        MaterializeError::NotApproved { id, state } => {
            assert_eq!(id.as_str(), "p-fixture-v1");
            assert_eq!(
                *state,
                PlanVersionState::Validated,
                "and it names the state"
            );
        }
        other => panic!("wrong refusal: {other}"),
    }
    assert!(
        store.tasks(None).expect("read").is_empty(),
        "and no task became claimable work"
    );

    // POSITIVE CONTROL — one human decision later, the identical call succeeds.
    approve(&mut store, &project, 1);
    let outcome = materialize(&mut store, &project, 1, &plan, 4_000).expect("approved plan");
    let created: Vec<&str> = outcome.created.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(created, vec!["T-0001", "T-0002"]);
}

/// The version the caller names and the version the document declares must be
/// one answer, for the same reason [`ledger::register_plan_version`] refuses a
/// document whose `version:` disagrees with its directory: `task.plan_version_id`,
/// supersession and §3.4's trailer all need one.
#[test]
fn materializing_one_version_from_another_versions_document_is_refused() {
    let dir = repo();
    let (mut store, project, v1) = approved(&dir);
    register_and_approve_version(&mut store, &dir, &project, 2, PLAN_V2_REPLACING);

    let error = materialize(&mut store, &project, 2, &v1, 3_000).expect_err("refused");
    assert!(
        matches!(
            error,
            MaterializeError::VersionMismatch {
                declared: 1,
                requested: 2,
                ..
            }
        ),
        "{error}"
    );
    assert!(
        store.tasks(None).expect("read").is_empty(),
        "and wrote nothing"
    );
}

/// §5.1 makes `task.plan_version_id` a foreign key into `plan_version`. A
/// materialisation against a version nobody registered has no row to point at,
/// and inventing one would put the ledger's own record downstream of task
/// creation.
#[test]
fn materializing_a_plan_version_that_was_never_registered_is_refused() {
    let dir = repo();
    let (mut store, project, _v1) = approved(&dir);
    write(dir.path(), &plan::plan_path(2), PLAN_V2_REPLACING);
    let document = plan::parse(PLAN_V2_REPLACING).expect("parse");
    let v2 = plan::validate(&document, &catalogue()).expect("v2 validates");

    let error = materialize(&mut store, &project, 2, &v2, 3_000).expect_err("refused");
    assert!(
        matches!(error, MaterializeError::UnknownPlanVersion { .. }),
        "{error}"
    );
    assert!(
        store.tasks(None).expect("read").is_empty(),
        "and wrote nothing"
    );
}

/// §3.3 control 2 in this module's shape: the project's `root_path` is where
/// `.conductor/project.yaml` is read from, so an unregistered project has no
/// tree to read and no inheritable `execution_requirements`.
#[test]
fn materializing_into_a_project_that_was_never_registered_is_refused() {
    let dir = repo();
    let (mut store, project, v1) = approved(&dir);
    let stranger = ProjectId::new("p-not-registered").expect("id");

    let error = materialize(&mut store, &stranger, 1, &v1, 3_000).expect_err("refused");
    assert!(
        matches!(error, MaterializeError::UnknownProject { .. }),
        "{error}"
    );
    // The control: the same call against the registered project succeeds, so
    // the refusal is about registration and not about the fixture.
    materialize(&mut store, &project, 1, &v1, 3_000).expect("the registered project materializes");
}

/// A task id is unique across the whole `task` table, so two projects can name
/// the same one. Adopting another project's row would silently re-point work
/// that is already someone else's — refused instead.
#[test]
fn a_task_id_that_already_belongs_to_another_project_is_refused_rather_than_adopted() {
    let dir = repo();
    let (mut store, project, v1) = approved(&dir);

    // A second project, with a row that claims T-0001.
    let other_dir = repo();
    write(
        other_dir.path(),
        ".conductor/project.yaml",
        &PROJECT_YAML.replace("p-fixture", "p-other"),
    );
    let other = ledger::register_project(&mut store, other_dir.path(), 1_000).expect("register");
    write(
        other_dir.path(),
        &plan::plan_path(1),
        &PLAN_V1.replace("id: p-fixture", "id: p-other"),
    );
    let other_plan = ledger::register_plan_version(&mut store, &other.id, 1, &catalogue())
        .expect("register the other project's v1")
        .plan;
    approve(&mut store, &other.id, 1);
    materialize(&mut store, &other.id, 1, &other_plan, 2_000).expect("materialize the other");

    let error = materialize(&mut store, &project, 1, &v1, 3_000).expect_err("refused");
    assert!(
        matches!(error, MaterializeError::ForeignTask { .. }),
        "{error}"
    );
}

/// **Positive control that cannot fail first, and is labelled as one.**
///
/// "Materialisation refuses a plan that has not been validated" is enforced by
/// [`materialize`]'s signature: it takes a [`plan::ValidatedPlan`], which only
/// [`plan::validate`] mints, so an unvalidated plan is not a value that can be
/// passed. There is no runtime path to watch fail. What this test *can* check
/// is the other end of the same claim — that a plan §3.7 refuses never becomes
/// one — and it does.
#[test]
fn a_plan_that_section_3_7_refuses_never_becomes_a_validated_plan_to_materialize() {
    // §3.7's headline rule: an acceptance criterion bound to no check.
    let unbound = PLAN_V1.replace("verified_by: [\"unit-tests\"]", "verified_by: []");
    let document = plan::parse(&unbound).expect("it still parses");
    let report = plan::validate(&document, &catalogue()).expect_err("§3.7 refuses it");
    assert!(
        report.to_string().contains("AC-1"),
        "the report names the criterion: {report}"
    );
}
