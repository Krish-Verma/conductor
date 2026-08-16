//! `.conductor/project.yaml` and the plan ledger — master plan §3.1, §3.3,
//! §3.6, §5.1, §5.2.
//!
//! # What these tests are guarding against
//!
//! §3.3 is the section this file exists for. `.conductor/` lives inside the
//! repository, therefore inside the agent's workspace, therefore *"an agent
//! **can write** `.conductor/plans/v3/APPROVED` in its own clone"*. Two of
//! §3.3's three controls are the ledger's:
//!
//! 2. Conductor reads plan approval **only** from the registered repository's
//!    working tree, never from a run branch. Encoded here by giving no
//!    approval-reading function a path parameter at all — the root comes from
//!    the `project` row — and asserted by
//!    [`approval_is_read_from_the_registered_working_tree_and_not_from_another_checkout`],
//!    which moves a perfectly consistent approval into a second checkout and
//!    shows it buys nothing.
//! 3. The store records the approval independently at grant time, and *"if file
//!    and store disagree, **execution halts** — it is never resynced"*. Asserted
//!    by [`a_sidecar_that_disagrees_with_the_store_halts_and_neither_side_is_resynced`],
//!    which checks the store row **and** the file bytes after the failed check:
//!    a "verifier" that quietly healed either side would pass a weaker test that
//!    only asserted the error.
//!
//! The first control — `.conductor/**` arriving on a run branch is rejected at
//! reconciliation — is not here. It needs a run branch and a reconciler.
//!
//! # The positive controls
//!
//! A ledger that refused everything would pass every rejection test below.
//! [`a_well_formed_project_file_declares_everything_section_3_1_promises`],
//! [`registering_a_plan_version_validates_it_and_records_it_as_validated`] and
//! [`approving_a_plan_records_it_in_the_store_and_writes_the_section_3_1_sidecar`]
//! are the controls that make the refusals mean something; every rejection
//! fixture below is one of those three with exactly one thing changed.

use std::collections::BTreeSet;
use std::path::Path;

use conductor_core::{PlanVersionState, ProjectId};
use conductor_git::run_git_ok;
use conductor_run::approval::{
    self, ApprovalKind, Authorization, Expiry, GrantOptions, NewApprovalRequest, Refusal, Subject,
};
use conductor_run::plan::{self, ledger, project};
use conductor_run::policy::model::{FactSet, Scope};
use conductor_run::verify::profile;
use conductor_store::Store;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PROJECT_YAML: &str = "\
project:
  id: p-fixture
  default_branch: main
  adapter: codex
  scope_defaults:
    allowed_globs: [\"crates/**\"]
    forbidden_globs: [\".conductor/**\"]
  review_cadence:
    boundary: milestone
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

/// A minimal plan that §3.7 accepts, at `version`, whose `objective` is the one
/// field the field-change tests move.
fn plan_yaml(version: u32, objective: &str) -> String {
    format!(
        "plan:\n  id: p-fixture\n  version: {version}\n  objective: \"{objective}\"\n  \
         milestones:\n    - id: M-01\n      title: \"Ledger\"\n      slices:\n        \
         - id: S-11\n          title: \"Plan ledger\"\n          tasks:\n            \
         - id: T-0001\n              objective: \"Register the plan.\"\n              \
         acceptance_criteria:\n                - id: AC-1\n                  \
         statement: \"The ledger records it.\"\n                  verified_by: [unit-tests]\n"
    )
}

/// The same plan, reformatted and commented — §3.6's "reformatting does **not**
/// invalidate approval".
fn plan_yaml_reformatted(version: u32, objective: &str) -> String {
    format!(
        "# A comment §3.6 excludes from the hash.\nplan:\n    objective:   \"{objective}\"\n    \
         milestones:\n        -   title: \"Ledger\"\n            id: M-01\n            \
         slices:\n                -   id: S-11\n                    title: \"Plan ledger\"\n                    \
         tasks:\n                        -   id: T-0001\n                            \
         acceptance_criteria:\n                                -   verified_by:\n                                        \
         - unit-tests\n                                    statement: \"The ledger records it.\"\n                                    \
         id: AC-1\n                            objective: \"Register the plan.\"\n    \
         version: {version}\n    id: p-fixture\n"
    )
}

/// §3.7's catalogue, assembled the way §3.7's clarification 3 says a caller
/// must: by loading `verification.yaml` and handing the ids to `validate`.
fn catalogue() -> BTreeSet<String> {
    let loaded = profile::parse(VERIFICATION_YAML).expect("the verification fixture parses");
    plan::check_ids(&loaded.profile)
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the parent directory");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn read(root: &Path, relative: &str) -> String {
    let path = root.join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A git repository with one commit, an `origin`, and a `.conductor/` holding
/// `project.yaml` and one plan version.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    run_git_ok(root, &["init", "-q", "-b", "main"]).expect("git init");
    run_git_ok(root, &["config", "user.email", "ledger@example.invalid"]).expect("email");
    run_git_ok(root, &["config", "user.name", "Ledger Test"]).expect("name");
    run_git_ok(
        root,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:conductor/fixture.git",
        ],
    )
    .expect("origin");
    write(root, ".conductor/project.yaml", PROJECT_YAML);
    write(root, ".conductor/verification.yaml", VERIFICATION_YAML);
    write(
        root,
        ".conductor/plans/v1/plan.yaml",
        &plan_yaml(1, "Prove the ledger records a plan."),
    );
    run_git_ok(root, &["add", "-A"]).expect("add");
    run_git_ok(root, &["commit", "-q", "-m", "initial"]).expect("commit");
    dir
}

fn store(dir: &tempfile::TempDir) -> Store {
    Store::open_or_create(dir.path().join("conductor.db")).expect("open the store")
}

/// A registered project with `v1` registered and `VALIDATED`.
fn registered(dir: &tempfile::TempDir) -> (Store, ProjectId) {
    let mut store = store(dir);
    let project = ledger::register_project(&mut store, dir.path(), 1_000).expect("register");
    ledger::register_plan_version(&mut store, &project.id, 1, &catalogue()).expect("register v1");
    let id = project.id.clone();
    (store, id)
}

/// One real §4.3 plan-approval grant, and the witness it produces.
///
/// Built through S8's own API rather than by inserting rows, so that
/// [`ledger::approve`]'s witness check is tested against the shape a human at
/// the control socket actually produces, not against a shape this file invented.
fn plan_grant(store: &mut Store, plan_version_id: &str, approver: &str) -> Authorization {
    let request_id = format!("AR-{plan_version_id}");
    let grant_id = format!("AG-{plan_version_id}");
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
            id: grant_id,
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

fn plan_version_row(
    store: &Store,
    project: &ProjectId,
    version: u32,
) -> conductor_store::PlanVersionRow {
    let id = ledger::plan_version_id(project, version);
    store
        .plan_version(&id)
        .expect("read the plan version")
        .unwrap_or_else(|| panic!("no plan version {id}"))
}

// ---------------------------------------------------------------------------
// §3.1 — `.conductor/project.yaml`
// ---------------------------------------------------------------------------

#[test]
fn a_well_formed_project_file_declares_everything_section_3_1_promises() {
    // POSITIVE CONTROL. §3.1: project.yaml is authoritative for "identity,
    // adapter, scope defaults, review cadence, execution_requirements". A
    // loader that refused everything would pass every refusal test below and
    // fail this one.
    let parsed = project::parse(PROJECT_YAML).expect("the project fixture parses");
    assert_eq!(parsed.id, "p-fixture");
    assert_eq!(parsed.default_branch, "main");
    assert_eq!(parsed.adapter, "codex");
    assert_eq!(parsed.scope_defaults.allowed_globs, vec!["crates/**"]);
    assert_eq!(parsed.scope_defaults.forbidden_globs, vec![".conductor/**"]);
    assert!(
        parsed.review_cadence.is_some(),
        "§3.1 puts review cadence in this file"
    );
    // The block is carried in the shape §4.2 writes it and `Task`'s per-task
    // override stores it, so the project default and the override are the same
    // dialect and one parser reads both.
    let requirements = parsed
        .execution_requirements
        .as_deref()
        .expect("§4.2's block is present");
    let parsed_requirements =
        conductor_run::policy::eligibility::ExecutionRequirements::parse_yaml(requirements)
            .expect("§4.2's block parses");
    assert!(!parsed_requirements.is_empty());
}

#[test]
fn a_missing_project_file_is_a_refusal_and_not_a_default_project() {
    // A default project would invent an identity, a default branch and an
    // adapter for a repository that declared none — and §5.1's `project` row is
    // what every plan version, task and run hangs off.
    let dir = tempfile::tempdir().expect("tempdir");
    let error = project::load(dir.path()).expect_err("a missing project.yaml is refused");
    assert!(
        error.to_string().contains(project::PROJECT_CONFIG_PATH),
        "the refusal must name the file it wanted: {error}"
    );
}

#[test]
fn an_unknown_key_in_the_project_file_loads_the_way_an_unknown_plan_key_does() {
    // Same rule as the plan (`plan::model`'s "No `deny_unknown_fields`"): a file
    // written for a later Conductor must still load. The hole that opens — an
    // ignored key is a key an agent adds for free — is closed by `config_hash`,
    // which is taken over the whole document, asserted below.
    let yaml = PROJECT_YAML.replace(
        "  adapter: codex\n",
        "  adapter: codex\n  a_later_conductors_key: true\n",
    );
    let parsed = project::parse(&yaml).expect("an unknown key loads");
    assert_eq!(parsed.id, "p-fixture");
    assert_ne!(
        parsed.config_hash(),
        project::parse(PROJECT_YAML)
            .expect("the control parses")
            .config_hash(),
        "a key this version does not model must still reach config_hash"
    );
}

#[test]
fn reformatting_the_project_file_does_not_change_its_config_hash() {
    // The other half of the pair above, and the reason `config_hash` is taken
    // over canonical semantics rather than file bytes: re-indenting a config is
    // not a configuration change.
    let reformatted = PROJECT_YAML.replace("  id: p-fixture", "  id:    'p-fixture'");
    assert_eq!(
        project::parse(PROJECT_YAML).expect("control").config_hash(),
        project::parse(&reformatted)
            .expect("reformatted")
            .config_hash()
    );
}

#[test]
fn a_project_whose_identity_adapter_or_branch_is_blank_is_refused() {
    for (field, blank) in [
        ("id", PROJECT_YAML.replace("id: p-fixture", "id: \"  \"")),
        (
            "adapter",
            PROJECT_YAML.replace("adapter: codex", "adapter: \"\""),
        ),
        (
            "default_branch",
            PROJECT_YAML.replace("default_branch: main", "default_branch: \"\""),
        ),
    ] {
        let error = match project::parse(&blank) {
            Ok(parsed) => panic!("a blank {field} must be refused, but it loaded as {parsed:?}"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(field),
            "the refusal must name the blank field: {error}"
        );
    }
}

#[test]
fn an_execution_requirements_block_that_gates_nothing_is_refused() {
    // The same refusal `plan::validate` makes for a per-task override and
    // `enforce::launch::requirements_for` makes for the durable column: a block
    // that is present and yields no requirement is indistinguishable from a
    // mis-nested one, and a mis-nested block must not read as "nothing is
    // gated".
    let yaml = PROJECT_YAML.replace("  filesystem_write: restricted\n", "");
    let yaml = yaml.replace("  control_surface: hard\n", "  not_a_dimension: hard\n");
    let error = project::parse(&yaml).expect_err("an unreadable requirement block is refused");
    assert!(
        error.to_string().contains("not_a_dimension"),
        "the refusal must name what it could not read: {error}"
    );
}

// ---------------------------------------------------------------------------
// §5.1 — registering a project
// ---------------------------------------------------------------------------

#[test]
fn registering_the_same_repository_twice_yields_one_row_with_one_id() {
    let dir = repo();
    let mut store = store(&dir);
    let first = ledger::register_project(&mut store, dir.path(), 1_000).expect("first");
    let second = ledger::register_project(&mut store, dir.path(), 9_000).expect("second");
    assert_eq!(first.id, second.id);
    assert_eq!(first.repo_identity, second.repo_identity);
    assert_eq!(
        first.created_at, second.created_at,
        "created_at is a fact about the first time the project was seen"
    );
    let rows: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM project", [], |row| row.get(0))
        .expect("count");
    assert_eq!(rows, 1, "registering twice must not create a second row");
}

#[test]
fn a_repositorys_identity_does_not_move_when_its_head_does() {
    // §5.1: `repo_identity = blake3(first_commit ‖ normalized_origin)`. Both
    // inputs are properties of the repository, not of what has been committed
    // to it since — an identity that moved with `HEAD` would make every commit
    // look like a different repository.
    let dir = repo();
    let mut store = store(&dir);
    let before = ledger::register_project(&mut store, dir.path(), 1_000).expect("before");
    write(dir.path(), "README.md", "later work\n");
    run_git_ok(dir.path(), &["add", "-A"]).expect("add");
    run_git_ok(dir.path(), &["commit", "-q", "-m", "second"]).expect("commit");
    let after = ledger::register_project(&mut store, dir.path(), 2_000).expect("after");
    assert_eq!(before.repo_identity, after.repo_identity);
}

#[test]
fn two_spellings_of_one_remote_are_one_normalized_origin() {
    // §5.1 says *normalized* origin. `git@host:path`, `ssh://git@host/path` and
    // a trailing `.git` are three spellings git itself treats as one remote, so
    // re-spelling the remote must not re-identify the repository.
    let dir = repo();
    let mut store = store(&dir);
    let scp = ledger::register_project(&mut store, dir.path(), 1_000).expect("scp form");
    run_git_ok(
        dir.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "ssh://git@GitHub.com/conductor/fixture",
        ],
    )
    .expect("set-url");
    let url = ledger::register_project(&mut store, dir.path(), 2_000).expect("url form");
    assert_eq!(scp.repo_identity, url.repo_identity);
}

// ---------------------------------------------------------------------------
// §3.7, §5.2 — registering a plan version
// ---------------------------------------------------------------------------

#[test]
fn registering_a_plan_version_validates_it_and_records_it_as_validated() {
    // POSITIVE CONTROL for the ledger's write path.
    let dir = repo();
    let (store, project) = registered(&dir);
    let row = plan_version_row(&store, &project, 1);
    assert_eq!(row.version, 1);
    assert_eq!(row.state, PlanVersionState::Validated);
    assert_eq!(row.source_path, plan::plan_path(1));
    assert_eq!(
        row.content_hash,
        plan::content_hash(&read(dir.path(), &plan::plan_path(1)))
            .expect("hash")
            .as_str()
    );
    assert_eq!(row.approved_at, None);
    assert_eq!(row.approved_by, None);
}

#[test]
fn registration_hands_back_the_validated_document_the_row_was_hashed_from() {
    // Task materialisation needs both halves, and re-reading the file to get
    // the second one would open a window in which it can change — the class of
    // gap §3.3 exists to close. The document that comes back must be the one
    // whose hash is in the row, not merely one that parses from the same path.
    let dir = repo();
    let mut store = store(&dir);
    let project = ledger::register_project(&mut store, dir.path(), 1_000).expect("register");
    let registered =
        ledger::register_plan_version(&mut store, &project.id, 1, &catalogue()).expect("v1");

    assert_eq!(registered.row.state, PlanVersionState::Validated);
    assert_eq!(registered.plan.plan().version, 1);
    assert_eq!(
        registered
            .plan
            .tasks()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        vec!["T-0001"]
    );
    // §3.7's escape hatch survives validation as data, which is the whole
    // reason the materializer wants this value rather than the row alone.
    assert!(!registered.plan.requires_human_review());
    assert_eq!(
        registered.row.content_hash,
        plan::content_hash(&read(dir.path(), &plan::plan_path(1)))
            .expect("hash")
            .as_str()
    );
}

#[test]
fn a_plan_that_section_3_7_refuses_is_not_recorded_at_all() {
    // A `VALIDATED` row for a plan that does not validate would make §5.2's
    // evidence ("`content_hash` + validation report") a claim rather than a
    // fact.
    let dir = repo();
    let mut store = store(&dir);
    let project = ledger::register_project(&mut store, dir.path(), 1_000).expect("register");
    write(
        dir.path(),
        &plan::plan_path(1),
        // One acceptance criterion bound to nothing — §3.7's most important
        // refusal.
        &plan_yaml(1, "unbound").replace("                  verified_by: [unit-tests]\n", ""),
    );
    let error = ledger::register_plan_version(&mut store, &project.id, 1, &catalogue())
        .expect_err("§3.7 refuses it");
    assert!(
        error.to_string().contains("AC-1"),
        "the refusal must carry §3.7's report: {error}"
    );
    assert_eq!(
        store
            .plan_versions_for_project(&project.id)
            .expect("read")
            .len(),
        0,
        "a refused plan must leave no row behind"
    );
}

#[test]
fn a_plan_file_whose_declared_version_is_not_its_directory_is_refused() {
    // `.conductor/plans/v2/plan.yaml` declaring `version: 1` names two
    // different versions at once, and §3.4's `Conductor-Plan: v3@blake3:…`
    // trailer, §5.1's `UNIQUE(project_id, version)` and supersession all need
    // one answer.
    let dir = repo();
    let mut store = store(&dir);
    let project = ledger::register_project(&mut store, dir.path(), 1_000).expect("register");
    write(dir.path(), &plan::plan_path(2), &plan_yaml(1, "mismatched"));
    let error = ledger::register_plan_version(&mut store, &project.id, 2, &catalogue())
        .expect_err("the mismatch is refused");
    let rendered = error.to_string();
    assert!(
        rendered.contains('1') && rendered.contains('2'),
        "the refusal must name both versions: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// §3.3, §5.2 — approval
// ---------------------------------------------------------------------------

/// Move `v1` to `AWAITING_APPROVAL` and approve it with a real §4.3 grant.
fn approve_v1(store: &mut Store, project: &ProjectId, approver: &str) -> ledger::Approval {
    let id = ledger::plan_version_id(project, 1);
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request approval");
    let witness = plan_grant(store, id.as_str(), approver);
    ledger::approve(store, project, 1, &witness, 3_000).expect("approve")
}

#[test]
fn approving_a_plan_records_it_in_the_store_and_writes_the_section_3_1_sidecar() {
    // POSITIVE CONTROL for §3.3's controls 2 and 3 together: after a legitimate
    // approval, the file and the store agree and `verify_approval` says so.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    let approval = approve_v1(&mut store, &project, "krish");

    let row = plan_version_row(&store, &project, 1);
    assert_eq!(row.state, PlanVersionState::Approved);
    assert_eq!(row.approved_by.as_deref(), Some("krish"));
    assert_eq!(row.approved_at, Some(3_000));
    assert_eq!(row.content_hash, approval.content_hash.as_str());

    // §3.1: the sidecar carries "plan content hash · approver · timestamp ·
    // policy hash". The timestamp is RFC 3339 UTC, the way §4.3 writes its own
    // approval artifacts — this file is committed, and §3.2 keeps it in git so
    // it is "reviewable as a diff, in a PR, by a human" and "readable without
    // Conductor installed", which `3000` is not.
    let sidecar = read(dir.path(), &plan::approved_path(1));
    for expected in [
        approval.content_hash.as_str(),
        "krish",
        "1970-01-01T00:00:03Z",
        "blake3:policy",
    ] {
        assert!(
            sidecar.contains(expected),
            "the APPROVED sidecar must carry {expected:?}: {sidecar}"
        );
    }
    assert!(
        !sidecar.contains("3000"),
        "the raw epoch value must not be what a human reads: {sidecar}"
    );

    let verified = ledger::verify_approval(&store, &project, 1).expect("file and store agree");
    assert_eq!(verified, approval);
    assert_eq!(verified.approved_at_ms, 3_000, "the instant is preserved");
}

#[test]
fn a_sidecar_timestamp_respelled_as_the_same_instant_is_not_a_disagreement() {
    // §5.1's column is an integer and the file is text, so the comparison is
    // over *instants*. `…:03Z` and `…:03.000Z` are one instant; a verifier that
    // compared the text would report a human disagreeing with themselves the
    // first time anything re-serialized the file.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    approve_v1(&mut store, &project, "krish");

    let respelled = read(dir.path(), &plan::approved_path(1))
        .replace("1970-01-01T00:00:03Z", "1970-01-01T00:00:03.000Z");
    write(dir.path(), &plan::approved_path(1), &respelled);
    ledger::verify_approval(&store, &project, 1).expect("the same instant, spelled differently");

    // A different instant, however, is exactly the disagreement §3.3 halts on.
    let moved = respelled.replace("1970-01-01T00:00:03.000Z", "1970-01-01T00:00:04Z");
    write(dir.path(), &plan::approved_path(1), &moved);
    let error = ledger::verify_approval(&store, &project, 1).expect_err("a different instant");
    let rendered = error.to_string();
    assert!(
        rendered.contains("1970-01-01T00:00:04Z") && rendered.contains("1970-01-01T00:00:03Z"),
        "the error must name both instants: {rendered}"
    );
}

#[test]
fn an_approved_row_that_records_no_approver_is_a_half_written_approval_and_halts() {
    // `approve` documents a crash window between the state move and the
    // approval content write. What the reader must never do is fill it in from
    // the file: an absent approver read as `""` and an absent timestamp read as
    // `0` would let a sidecar claiming `1970-01-01T00:00:00Z` verify against a
    // store that records nothing about who approved anything.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    approve_v1(&mut store, &project, "krish");
    let id = ledger::plan_version_id(&project, 1);

    store
        .conn()
        .execute(
            "UPDATE plan_version SET approved_by = NULL, approved_at = NULL WHERE id = ?1",
            [id.as_str()],
        )
        .expect("simulate the crash window");

    let error = ledger::verify_approval(&store, &project, 1).expect_err("a half-written approval");
    let rendered = error.to_string();
    assert!(
        rendered.contains("half-written") && rendered.contains(id.as_str()),
        "the refusal must name the version and say what is missing: {rendered}"
    );
}

#[test]
fn a_sidecar_timestamp_that_is_not_rfc_3339_is_refused_and_never_read_as_the_epoch() {
    // Fail closed. The tempting default is `0`, and `0` is a real instant —
    // `1970-01-01T00:00:00Z` — so a corrupt field read as zero would *verify*
    // against a store that happened to record it.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    approve_v1(&mut store, &project, "krish");

    let original = read(dir.path(), &plan::approved_path(1));
    for bad in ["\"3000\"", "\"1970-01-01 00:00:03Z\"", "\"whenever\""] {
        let tampered = original.replace("1970-01-01T00:00:03Z", bad);
        assert_ne!(tampered, original, "the fixture must actually be tampered");
        write(dir.path(), &plan::approved_path(1), &tampered);
        match ledger::verify_approval(&store, &project, 1) {
            Ok(approval) => panic!("{bad} must be refused, but verified as {approval:?}"),
            Err(error) => assert!(
                error.to_string().contains("cannot be read"),
                "the refusal must say the sidecar is unreadable, not default it: {error}"
            ),
        }
    }
}

#[test]
fn an_approval_witness_that_is_a_refusal_authorizes_nothing() {
    // The reason the parameter is a witness and not a `bool`: the only value
    // that approves anything is one S8 produced, and `Refused` is the other
    // half of the type.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    let id = ledger::plan_version_id(&project, 1);
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request approval");
    let witness = Authorization::Refused(Refusal::NoMatchingGrant {
        binding: conductor_run::approval::BindingHash::from_stored("blake3:nothing"),
    });
    let error = ledger::approve(&mut store, &project, 1, &witness, 3_000)
        .expect_err("a refusal approves nothing");
    assert!(
        error.to_string().contains("no live grant"),
        "the refusal must carry S8's reason: {error}"
    );
    assert_eq!(
        plan_version_row(&store, &project, 1).state,
        PlanVersionState::AwaitingApproval,
        "a refused approval must not move the row"
    );
    assert!(!dir.path().join(plan::approved_path(1)).exists());
}

#[test]
fn a_witness_naming_a_grant_for_a_different_plan_version_authorizes_nothing() {
    // §4.3: "Collapsing them would let a plan approval satisfy a deployment
    // gate." The same argument one level down — a plan approval for v2 must not
    // approve v1 — and it is why the witness is re-derived from the grant row
    // rather than believed.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    write(dir.path(), &plan::plan_path(2), &plan_yaml(2, "the second"));
    ledger::register_plan_version(&mut store, &project, 2, &catalogue()).expect("register v2");
    let id = ledger::plan_version_id(&project, 1);
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request approval");

    let elsewhere = plan_grant(
        &mut store,
        ledger::plan_version_id(&project, 2).as_str(),
        "krish",
    );
    let error = ledger::approve(&mut store, &project, 1, &elsewhere, 3_000)
        .expect_err("a grant for v2 does not approve v1");
    let rendered = error.to_string();
    assert!(
        rendered.contains(id.as_str()),
        "the refusal must name the version that was being approved: {rendered}"
    );
    assert_eq!(
        plan_version_row(&store, &project, 1).state,
        PlanVersionState::AwaitingApproval
    );
}

#[test]
fn one_grant_approves_one_plan_version_once() {
    // §4.3's one-shot rule reaching the ledger: re-approval after an edit is a
    // *new* human decision (§5.2's restart clause: "cleared by re-running
    // `conductor plan approve <version>`"), so replaying the spent witness must
    // not clear it.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    let id = ledger::plan_version_id(&project, 1);
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("request approval");
    let witness = plan_grant(&mut store, id.as_str(), "krish");
    ledger::approve(&mut store, &project, 1, &witness, 3_000).expect("the first approval");

    write(
        dir.path(),
        &plan::plan_path(1),
        &plan_yaml(1, "edited after approval"),
    );
    let error = ledger::approve(&mut store, &project, 1, &witness, 4_000)
        .expect_err("a spent grant approves nothing");
    assert!(
        error.to_string().contains("consumed"),
        "the refusal must say the grant was already spent: {error}"
    );
}

#[test]
fn editing_an_approved_plan_is_detected_and_the_error_names_both_hashes() {
    // S11's "approved plan immutable (edit → hard error)", and §5.2's restart
    // clause: "re-hash on load; a mismatch on an `APPROVED` plan is a hard
    // error".
    let dir = repo();
    let (mut store, project) = registered(&dir);
    let approved = approve_v1(&mut store, &project, "krish");

    write(
        dir.path(),
        &plan::plan_path(1),
        &plan_yaml(1, "edited after approval"),
    );
    let edited = plan::content_hash(&read(dir.path(), &plan::plan_path(1))).expect("hash");

    let error = ledger::verify_approval(&store, &project, 1).expect_err("the edit is detected");
    let rendered = error.to_string();
    assert!(
        rendered.contains(approved.content_hash.as_str()),
        "the error must name the approved hash: {rendered}"
    );
    assert!(
        rendered.contains(edited.as_str()),
        "the error must name the hash the file now has: {rendered}"
    );
}

#[test]
fn reformatting_an_approved_plan_keeps_its_approval_and_changing_a_field_loses_it() {
    // §3.6's two halves, end to end through the ledger rather than only through
    // `content_hash`: "reformatting does **not** invalidate approval; changing
    // any field **does**."
    let dir = repo();
    let (mut store, project) = registered(&dir);
    approve_v1(&mut store, &project, "krish");

    write(
        dir.path(),
        &plan::plan_path(1),
        &plan_yaml_reformatted(1, "Prove the ledger records a plan."),
    );
    ledger::verify_approval(&store, &project, 1)
        .expect("reformatting must not invalidate an approval");

    write(
        dir.path(),
        &plan::plan_path(1),
        &plan_yaml_reformatted(1, "a different objective"),
    );
    ledger::verify_approval(&store, &project, 1).expect_err("a field change must invalidate it");
}

#[test]
fn a_sidecar_that_disagrees_with_the_store_halts_and_neither_side_is_resynced() {
    // §3.3 control 3, in full: "If file and store disagree, **execution halts**
    // — it is never resynced." A verifier that healed either side would pass a
    // test that only asserted the error, so both sides are read back.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    let approved = approve_v1(&mut store, &project, "krish");
    let before = plan_version_row(&store, &project, 1);

    let tampered = read(dir.path(), &plan::approved_path(1)).replace(
        approved.content_hash.as_str(),
        "blake3:0000000000000000000000000000000000000000000000000000000000000000",
    );
    write(dir.path(), &plan::approved_path(1), &tampered);

    let error = ledger::verify_approval(&store, &project, 1).expect_err("the disagreement halts");
    let rendered = error.to_string();
    assert!(
        rendered.contains(approved.content_hash.as_str()) && rendered.contains("blake3:0000"),
        "the error must name both values: {rendered}"
    );
    assert_eq!(
        plan_version_row(&store, &project, 1),
        before,
        "the store row must be untouched by a failed check"
    );
    assert_eq!(
        read(dir.path(), &plan::approved_path(1)),
        tampered,
        "the file must be untouched by a failed check"
    );
}

#[test]
fn approval_is_read_from_the_registered_working_tree_and_not_from_another_checkout() {
    // §3.3 control 2: "Conductor reads plan approval **only** from the
    // registered repository's working tree, never from a run branch." No
    // approval-reading function here takes a path, so the only tree that can
    // answer is the one the `project` row names — moving a perfectly consistent
    // approval into a second checkout buys nothing.
    let dir = repo();
    let (mut store, project) = registered(&dir);
    approve_v1(&mut store, &project, "krish");

    let elsewhere = tempfile::tempdir().expect("a second checkout");
    write(
        elsewhere.path(),
        &plan::approved_path(1),
        &read(dir.path(), &plan::approved_path(1)),
    );
    write(
        elsewhere.path(),
        &plan::plan_path(1),
        &read(dir.path(), &plan::plan_path(1)),
    );
    std::fs::remove_file(dir.path().join(plan::approved_path(1))).expect("remove the sidecar");

    let error =
        ledger::verify_approval(&store, &project, 1).expect_err("the registered tree has none");
    assert!(
        error.to_string().contains(
            dir.path()
                .join(plan::approved_path(1))
                .to_string_lossy()
                .as_ref()
        ),
        "the error must name the registered tree's path, not another checkout's: {error}"
    );
}

#[test]
fn approving_v2_supersedes_v1() {
    let dir = repo();
    let (mut store, project) = registered(&dir);
    approve_v1(&mut store, &project, "krish");

    write(dir.path(), &plan::plan_path(2), &plan_yaml(2, "the second"));
    ledger::register_plan_version(&mut store, &project, 2, &catalogue()).expect("register v2");
    let v2 = ledger::plan_version_id(&project, 2);
    store
        .set_plan_state(&v2, PlanVersionState::AwaitingApproval)
        .expect("request approval");
    let witness = plan_grant(&mut store, v2.as_str(), "krish");
    ledger::approve(&mut store, &project, 2, &witness, 5_000).expect("approve v2");

    assert_eq!(
        plan_version_row(&store, &project, 1).state,
        PlanVersionState::Superseded,
        "approving v2 supersedes v1 (§5.2: SUPERSEDED by a later APPROVED)"
    );
    assert_eq!(
        plan_version_row(&store, &project, 2).state,
        PlanVersionState::Approved
    );
}

#[test]
fn a_plan_version_that_no_human_has_reached_cannot_be_verified_as_approved() {
    // §5.2's "Invalid: `DRAFT → APPROVED`" seen from the reader's side: a
    // `VALIDATED` row has no approval to verify, and reporting one would be the
    // self-approval §3.7's clarification 4 rules out.
    let dir = repo();
    let (store, project) = registered(&dir);
    let error = ledger::verify_approval(&store, &project, 1).expect_err("nothing is approved yet");
    assert!(
        error.to_string().contains("VALIDATED"),
        "the refusal must name the state it found: {error}"
    );
}

#[test]
fn the_approval_kind_a_plan_grant_carries_is_section_4_3s_plan_approval() {
    // Guards the fixture, not the ledger: if `plan_grant` ever stopped
    // producing a `PLAN_APPROVAL`, every test above would still pass while
    // testing the wrong kind.
    assert_eq!(
        Subject::PlanVersion {
            plan_version_id: "pv-1".to_string()
        }
        .kind(),
        ApprovalKind::Plan
    );
}
