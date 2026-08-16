//! `conductor recover` — master plan §3.2, §3.5, §7.1, §7.2.
//!
//! # Why this file exists when `conductor-run/tests/plan_reconstruct.rs` already
//! proves the same invariant
//!
//! That file proves the *path* is correct. This one proves it survives the
//! boundary that matters for §3.5's claim, which is a claim about a machine that
//! lost its database rather than about a function that was called twice.
//!
//! The setup process registers the project, approves a plan through a real §4.3
//! grant, materialises tasks and then **drops its `Store` and deletes every file
//! the store consists of**. Reconstruction then runs in a *separate operating
//! system process* — the real `conductor` binary, found through
//! `CARGO_BIN_EXE_conductor` — whose address space has never held any of it. A
//! rebuild that quietly depended on a cached connection, a memoised row or a
//! `static` would be a rebuild this test cannot pass, and an in-process test
//! could not tell the difference.
//!
//! What it does **not** claim: the setup half runs in this test's own process,
//! so this is "a fresh process reconstructs" and not "two independent processes
//! hand state to each other through the repository alone". The second is
//! strictly stronger, and it is not what §3.5 asks for — §3.5's actor is an
//! operator standing in front of a repository with no database.

use std::path::Path;
use std::process::Command;

use conductor_core::{PlanVersionState, ProjectId};
use conductor_git::run_git_ok;
use conductor_run::approval::{
    self, Authorization, Expiry, GrantOptions, NewApprovalRequest, Subject,
};
use conductor_run::plan::{ledger, materialize};
use conductor_run::policy::model::{FactSet, Scope};
use conductor_run::verify::profile;
use conductor_store::Store;

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

const PROJECT_YAML: &str = "\
project:
  id: p-recover-cli
  default_branch: main
  adapter: codex
";

const VERIFICATION_YAML: &str = "\
verification:
  required:
    - id: unit-tests
      command: cargo test
";

const DECISION_MD: &str = "\
---
id: D-0001
status: ACCEPTED
date: 2026-08-15
---
The store is disposable. The repository is not.
";

fn plan_yaml(version: u32) -> String {
    format!(
        "plan:\n  id: p-recover-cli\n  version: {version}\n  objective: \"Survive store loss.\"\n  \
         milestones:\n    - id: M-01\n      title: \"Recovery\"\n      slices:\n        \
         - id: S-11\n          title: \"Plan ledger\"\n          tasks:\n            \
         - id: T-0001\n              objective: \"Rebuild from the repository.\"\n              \
         acceptance_criteria:\n                - id: AC-1\n                  \
         statement: \"Project truth outlives execution state.\"\n                  \
         verified_by: [unit-tests]\n"
    )
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the parent directory");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Delete every file the store consists of — see
/// `conductor-run/tests/plan_reconstruct.rs` for why `-wal` and `-shm` matter.
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

#[test]
fn a_fresh_process_rebuilds_project_truth_from_the_repository_after_the_store_is_deleted() {
    let repo = tempfile::tempdir().expect("tempdir");
    let root = repo.path();
    run_git_ok(root, &["init", "-q", "-b", "main"]).expect("git init");
    run_git_ok(root, &["config", "user.email", "recover@example.invalid"]).expect("email");
    run_git_ok(root, &["config", "user.name", "Recover Test"]).expect("name");
    write(root, ".conductor/project.yaml", PROJECT_YAML);
    write(root, ".conductor/verification.yaml", VERIFICATION_YAML);
    write(
        root,
        ".conductor/decisions/D-0001-store-is-disposable.md",
        DECISION_MD,
    );
    write(root, ".conductor/plans/v1/plan.yaml", &plan_yaml(1));
    run_git_ok(root, &["add", "-A"]).expect("add");
    run_git_ok(root, &["commit", "-q", "-m", "initial"]).expect("commit");

    let store_dir = tempfile::tempdir().expect("store dir");
    let db = store_dir.path().join("conductor.db");

    // -- the state a live Conductor would hold --------------------------------
    let approved_hash = {
        let mut store = Store::open_or_create(&db).expect("open");
        let project = ledger::register_project(&mut store, root, 1_000).expect("register");
        let project_id = project.id.clone();
        let catalogue = {
            let loaded = profile::parse(VERIFICATION_YAML).expect("parse");
            conductor_run::plan::check_ids(&loaded.profile)
        };
        let registered = ledger::register_plan_version(&mut store, &project_id, 1, &catalogue)
            .expect("register v1");

        let request_id = "AR-1".to_string();
        approval::request(
            store.conn_mut(),
            &NewApprovalRequest {
                id: request_id.clone(),
                subject: Subject::PlanVersion {
                    plan_version_id: registered.row.id.as_str().to_string(),
                },
                run_id: None,
                facts: FactSet::new(),
                policy_hash: "blake3:policy".to_string(),
                matched_rules: Vec::new(),
                explanation: "make v1 authoritative".to_string(),
                evidence_ref: None,
                expires: Expiry::Never,
            },
            1_000,
        )
        .expect("request");
        let grant = approval::grant(
            store.conn_mut(),
            &request_id,
            &GrantOptions {
                id: "AG-1".to_string(),
                scope: Scope::from_pairs([(
                    "plan_version".to_string(),
                    registered.row.id.as_str().to_string(),
                )]),
                reuse: false,
                expires: Expiry::Never,
                granted_by: "alice".to_string(),
                channel: "socket".to_string(),
                nonce_hash: None,
            },
            2_000,
        )
        .expect("grant");

        let id = ledger::plan_version_id(&project_id, 1);
        store
            .set_plan_state(&id, PlanVersionState::AwaitingApproval)
            .expect("await");
        let approval = ledger::approve(
            &mut store,
            &project_id,
            1,
            &Authorization::Authorized { grant_id: grant.id },
            3_000,
        )
        .expect("approve");
        materialize::materialize(&mut store, &project_id, 1, &registered.plan, 4_000)
            .expect("materialise");
        approval.content_hash.as_str().to_string()
        // `store` drops here: the connection this process held is closed.
    };

    // -- the destructive step -------------------------------------------------
    destroy_store(&db);

    // -- a separate process, given the repository and nothing else ------------
    let output = Command::new(CONDUCTOR)
        .args([
            "recover",
            "--repo",
            &root.display().to_string(),
            "--store",
            &db.display().to_string(),
            "--json",
            "--now-ms",
            "10000",
        ])
        .output()
        .expect("run conductor recover");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "conductor recover must succeed.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("recover --json must emit JSON ({e}):\n{stdout}"));
    assert_eq!(report["project"], "p-recover-cli");
    assert_eq!(report["approved_version"], 1);
    assert_eq!(report["versions"][0]["state"], "APPROVED");
    assert_eq!(
        report["versions"][0]["approval_restored"], true,
        "the approval must be restored from the receipt, not found already present"
    );
    assert_eq!(report["tasks_rebuilt"][0], "T-0001");
    assert_eq!(report["decisions"][0], "D-0001");

    // -- and the rebuilt store agrees with the receipt ------------------------
    let store = Store::open_existing(&db).expect("the rebuilt store");
    let project_id = ProjectId::new("p-recover-cli").expect("id");
    let approval = ledger::verify_approval(&store, &project_id, 1)
        .expect("store and sidecar agree after the rebuild");
    assert_eq!(
        approval.content_hash.as_str(),
        approved_hash,
        "the rebuilt approval must be over the same document the human approved"
    );
}

#[test]
fn recover_refuses_a_repository_that_is_not_a_project_and_says_so_with_code_two() {
    // §7.2's `2` is "no project / not initialized". A recover that created a
    // project out of an empty directory would be inventing the identity §3.1
    // says a human decides once.
    let empty = tempfile::tempdir().expect("tempdir");
    let store_dir = tempfile::tempdir().expect("store dir");
    let output = Command::new(CONDUCTOR)
        .args([
            "recover",
            "--repo",
            &empty.path().display().to_string(),
            "--store",
            &store_dir.path().join("conductor.db").display().to_string(),
            "--json",
        ])
        .output()
        .expect("run conductor recover");
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn recover_has_no_scan_flag_rather_than_a_scan_flag_that_does_nothing() {
    // §7.1 spells the command `conductor recover [--scan]`, and the descriptor
    // scan it names is S14's. Accepting the flag and ignoring it would be a knob
    // that does nothing; refusing it is the honest answer until S14 wires it.
    let output = Command::new(CONDUCTOR)
        .args(["recover", "--scan"])
        .output()
        .expect("run conductor recover --scan");
    assert_eq!(
        output.status.code(),
        Some(64),
        "an unimplemented flag must be a usage error, not a silent no-op"
    );
}
