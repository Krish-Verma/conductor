//! A disposable world for the S5 vertical: source repository, store, seeded
//! task and run, workspaces and artifacts roots.
//!
//! One copy per test binary; each uses a subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use conductor_core::{RunId, TaskId};
use conductor_store::{NewRun, NewTask, Store};

use super::agent::{POLICY_HASH, head, source_repo};

/// The run every fixture uses.
pub const RUN: &str = "r-0041";
/// The task every fixture uses.
pub const TASK: &str = "T-0012";
/// §4.1's branch model.
pub const RUN_BRANCH: &str = "conductor/T-0012/r-0041";
/// The branch the run integrates into.
pub const TARGET_BRANCH: &str = "main";

/// The verification profile the vertical runs. Deliberately trivial and
/// hermetic: what is under test is the spine, not the checks.
pub const PASSING_PROFILE: &str = r#"
verification:
  toolchain_fingerprint:
    - ["/bin/echo", "toolchain-v1"]
  required:
    - id: unit-tests
      command: ["/bin/sh", "-c", "echo ok"]
      timeout_seconds: 60
  invariants:
    - id: no-secrets
      command: ["/bin/sh", "-c", "exit 0"]
      timeout_seconds: 60
"#;

/// The same profile with a check that fails — acceptance row 7's shape, used
/// here only to prove the gate refuses.
pub const FAILING_PROFILE: &str = r#"
verification:
  toolchain_fingerprint:
    - ["/bin/echo", "toolchain-v1"]
  required:
    - id: unit-tests
      command: ["/bin/sh", "-c", "echo boom; exit 1"]
      timeout_seconds: 60
"#;

/// Everything one vertical test needs, in one temporary directory.
pub struct World {
    /// Keeps the directory alive.
    pub dir: tempfile::TempDir,
    /// The operator's repository.
    pub source: PathBuf,
    /// The commit the run branches from.
    pub base_commit: String,
}

impl World {
    /// Build the world and seed a `PENDING` task with a `READY` run.
    pub fn new() -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        // macOS temp dirs live behind /var -> /private/var. Canonicalise once so
        // a path recorded in the store and a path observed on disk are the same
        // spelling of the same place.
        let root = dir.path().canonicalize().expect("canonicalize");
        let source = source_repo(&root);
        let base_commit = head(&source);

        let mut store = Store::open_or_create(root.join("conductor.db")).expect("store");
        seed_parents(&mut store);
        store
            .create_task(
                &NewTask {
                    id: TaskId::new(TASK).expect("task id"),
                    plan_version_id: "pv-1".to_string(),
                    slice_id: "S5".to_string(),
                    scope_globs: vec!["src/**".to_string(), "docs/**".to_string()],
                    verification_profile: "verification.yaml".to_string(),
                    attempt_budget: 3,
                },
                0,
            )
            .expect("create task");
        store
            .create_run(
                &NewRun {
                    id: RunId::new(RUN).expect("run id"),
                    task_id: TaskId::new(TASK).expect("task id"),
                    policy_hash: POLICY_HASH.to_string(),
                    base_commit: base_commit.clone(),
                    run_branch: RUN_BRANCH.to_string(),
                    target_branch: TARGET_BRANCH.to_string(),
                },
                0,
            )
            .expect("create run");
        drop(store);

        std::fs::write(root.join("verification.yaml"), PASSING_PROFILE).expect("write profile");

        World {
            dir,
            source,
            base_commit,
        }
    }

    /// Replace the verification profile.
    pub fn with_profile(self, yaml: &str) -> World {
        std::fs::write(self.root().join("verification.yaml"), yaml).expect("write profile");
        self
    }

    /// The temporary root.
    pub fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canonicalize")
    }

    /// A fresh handle on the store.
    pub fn store(&self) -> Store {
        Store::open_or_create(self.root().join("conductor.db")).expect("store")
    }

    /// Where run workspaces live.
    pub fn workspaces(&self) -> PathBuf {
        self.root().join("workspaces")
    }

    /// This run's workspace.
    pub fn workspace(&self) -> PathBuf {
        self.workspaces().join(RUN)
    }

    /// Where artifacts live.
    pub fn artifacts(&self) -> PathBuf {
        self.root().join("artifacts")
    }

    /// Where orphan workspaces are quarantined.
    pub fn quarantine(&self) -> PathBuf {
        self.root().join("quarantine")
    }

    /// The verification profile's path.
    pub fn profile(&self) -> PathBuf {
        self.root().join("verification.yaml")
    }

    /// The run's state, as the database holds it.
    pub fn run_state(&self) -> conductor_core::RunState {
        self.store()
            .run(&RunId::new(RUN).expect("id"))
            .expect("run")
            .expect("a row")
            .state
    }

    /// The task's state, as the database holds it.
    pub fn task_state(&self) -> conductor_core::TaskState {
        self.store()
            .task(&TaskId::new(TASK).expect("id"))
            .expect("task")
            .expect("a row")
            .state
    }

    /// Move the user's own `main` forward — acceptance row 16.
    pub fn user_commits_to_main(&self) -> String {
        std::fs::write(self.source.join("README.md"), "the user moved on\n").expect("write");
        git(&self.source, &["add", "-A"]);
        git(&self.source, &["commit", "-q", "-m", "user's own work"]);
        head(&self.source)
    }
}

/// Seed the rows a task and run reference.
pub fn seed_parents(store: &mut Store) {
    conductor_store::with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT OR IGNORE INTO project
               (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES ('p-1', '/fixture', 'blake3:repo', 'main', 'blake3:cfg', 0)",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO plan_version
               (id, project_id, version, content_hash, state, source_path)
             VALUES ('pv-1', 'p-1', 1, 'blake3:plan', 'APPROVED', 'plan.yaml')",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO policy_snapshot (hash, canonical_blob, created_at)
             VALUES (?1, '{}', 0)",
            rusqlite::params![POLICY_HASH],
        )?;
        Ok(())
    })
    .expect("seed");
}

/// How many commits sit on `branch` above `base`.
///
/// The counting assertion acceptance row 22 asks for: "**exactly one commit**".
pub fn commits_above(repo: &Path, branch: &str, base: &str) -> usize {
    let out = Command::new("git")
        .args(["rev-list", "--count", &format!("{base}..{branch}")])
        .current_dir(repo)
        .output()
        .expect("git rev-list");
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// How many times a ref has been updated, from its reflog.
///
/// Not "where does the ref point" — git appends one reflog line per ref update,
/// so this counts **updates**, which is what row 22's "one ref update" means.
pub fn ref_updates(repo: &Path, reference: &str) -> usize {
    let out = Command::new("git")
        .args(["reflog", "show", "--format=%H", reference])
        .current_dir(repo)
        .output()
        .expect("git reflog");
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout).lines().count()
}

/// Run git, insisting it succeeded.
pub fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
