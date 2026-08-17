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

/// Where the fixture's verification profile lives, relative to the registered
/// tree — §3.1's location, and what the fixture's task row names.
pub const VERIFICATION_PROFILE: &str = ".conductor/verification.yaml";

/// The fixture's §4.4 policy, and the reason it is not empty.
///
/// # A finding about these tests, recorded at S12
///
/// The fixture used to pin a policy snapshot whose `canonical_blob` was `'{}'` —
/// which **does not decode**. Nothing read it back, so nothing noticed. Two
/// scenarios then asserted acceptance rows 13 and 14 (`AWAITING_APPROVAL` for a
/// dependency change and for a remote change) and **passed for the wrong reason**:
/// the policy gate cannot decide against a policy it cannot read, so it failed
/// closed to a human. The right answer, reached by a mechanism that had nothing to
/// do with the rules.
///
/// It surfaced when §6.5's packet started reading the pinned policy to fill in its
/// `boundaries` — an undecodable snapshot became an attempt with nothing to tell
/// the agent, and fixing *that* removed the accidental fail-closed and dropped both
/// tests to `VERIFYING`.
///
/// `VERIFYING` is correct for an empty policy, and not a fail-open:
/// `Action::floor()` allows a **known** taxonomy action that no rule names and
/// denies an unknown one, which is S7's documented reading of §4.4. So the fixture
/// now declares the rules the rows are about, and the tests assert the mechanism
/// instead of its absence.
pub const FIXTURE_POLICY: &str = "\
policy:
  rules:
    # Acceptance row 13: an agent adds a dependency.
    - id: fixture.dependency
      action: dependency.add.runtime
      effect: require_approval
    # Acceptance row 14: an agent rewrites the remote. Repository structure is a
    # human's decision even when the tree is byte-identical to baseline.
    - id: fixture.remote
      action: git.remote.modify
      effect: require_approval
    - id: fixture.lockfile
      action: dependency.lockfile.modify
      effect: require_approval
";

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

        let mut store = Store::open_or_create(root.join("conductor.db")).expect("store");
        // The **source repository** is the registered tree (§3.3 control 2), and
        // it is where §6.5's packet reads the plan document from.
        seed_parents_at(&mut store, &source);
        // **After** seeding, because seeding commits `.conductor/`. Reading it
        // first would pin the run to the commit before project truth existed, and
        // integration would then correctly report that `main` moved while the run
        // was in flight (acceptance row 16) — a real divergence, invented by the
        // fixture.
        let base_commit = head(&source);
        store
            .create_task(
                &NewTask {
                    id: TaskId::new(TASK).expect("task id"),
                    plan_version_id: "pv-1".to_string(),
                    slice_id: "S5".to_string(),
                    scope_globs: vec!["src/**".to_string(), "docs/**".to_string()],
                    verification_profile: VERIFICATION_PROFILE.to_string(),
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

        World {
            dir,
            source,
            base_commit,
        }
    }

    /// Replace the verification profile.
    pub fn with_profile(self, yaml: &str) -> World {
        std::fs::write(self.profile(), yaml).expect("write profile");
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

    /// The verification profile's path — §3.1's location, in the **registered
    /// tree**.
    ///
    /// It moved here at S12, and the move is the point. There used to be two
    /// profiles: this one, at the tempdir root, which the verification runner was
    /// pointed at; and whatever `task.verification_profile` named, which nothing
    /// read. §6.5's packet reads the second, and §4.5's clarification 3 settles it
    /// as a path relative to the repository root — so the fixture now has exactly
    /// one profile, in the one place the product looks, and `with_profile`
    /// overwrites the same file both the runner and the packet read.
    pub fn profile(&self) -> PathBuf {
        self.source.join(VERIFICATION_PROFILE)
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
    seed_parents_at(store, Path::new("/fixture"));
}

/// The same, for a project whose repository is really on disk at `root`.
///
/// # Why the root stopped being a fiction (S12)
///
/// This used to register `root_path: '/fixture'` — a path that does not exist —
/// and a `plan_version` pointing at a `plan.yaml` that was never written. That
/// was harmless while the parent rows existed only to satisfy §5.1's foreign
/// keys. S12 made it load-bearing: §6.5's packet is *"generated from durable
/// state"*, and the durable state it reads includes the **plan document in the
/// registered tree**. A fixture with no document cannot produce a packet, and
/// after ADR-0017 a run with no approved plan behind it is not a state the
/// product can reach at all — so a fixture that created one was testing a
/// situation that no longer exists.
///
/// The plan written here is the smallest one that satisfies §3.7: one milestone,
/// one slice, one task ([`TASK`]) with an objective, a scope matching what the
/// vertical's scenarios touch, and one acceptance criterion bound to a check
/// [`PASSING_PROFILE`] declares.
pub fn seed_parents_at(store: &mut Store, root: &Path) {
    seed_parents_with_objective(store, root, DEFAULT_OBJECTIVE);
}

/// What the fixture plan asks for, when the caller does not care.
///
/// The scenarios the fake agent runs write `src/added.rs`, so this is what they
/// are nominally doing. A test driving a **real** agent has to pass its own, or
/// the packet would ask for one thing and the assertion would look for another —
/// which is exactly the class of mismatch §6.5 exists to remove.
pub const DEFAULT_OBJECTIVE: &str = "Add a greeting helper to the library.";

/// The same, for a fixture whose task must ask for something specific.
pub fn seed_parents_with_objective(store: &mut Store, root: &Path, objective: &str) {
    if root.is_dir() {
        write_conductor_layout(root, objective);
    }
    let root_path = root.to_string_lossy().to_string();
    conductor_store::with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT OR IGNORE INTO project
               (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES ('p-1', ?1, 'blake3:repo', 'main', 'blake3:cfg', 0)",
            rusqlite::params![root_path],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO plan_version
               (id, project_id, version, content_hash, state, source_path)
             VALUES ('pv-1', 'p-1', 1, 'blake3:plan', 'APPROVED',
                     '.conductor/plans/v1/plan.yaml')",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO policy_snapshot (hash, canonical_blob, created_at)
             VALUES (?1, ?2, 0)",
            rusqlite::params![POLICY_HASH, fixture_policy_blob()],
        )?;
        Ok(())
    })
    .expect("seed");
}

/// The canonical blob for [`FIXTURE_POLICY`] — one that **decodes**.
///
/// See [`FIXTURE_POLICY`] for what the old `'{}'` was hiding.
///
/// Produced by the real loader and serializer rather than hand-written, so a
/// change to the canonical form updates the fixture instead of breaking it — and
/// so the snapshot a run is judged by is the same document
/// `write_conductor_layout` puts in the tree.
///
/// The *hash* stays [`POLICY_HASH`]: every fixture run row references that
/// constant, and nothing here is testing the digest of a policy.
fn fixture_policy_blob() -> String {
    use conductor_run::policy::load;
    use conductor_run::policy::model::Origin;

    let document =
        load::parse_document(FIXTURE_POLICY, Origin::Project).expect("the fixture policy parses");
    let resolved =
        load::resolve_documents(None, Some(document), None).expect("the fixture policy resolves");
    load::snapshot(&resolved).canonical_blob
}

/// `.conductor/` as §3.1 lays it out, with the one plan [`TASK`] comes from.
///
/// **Committed**, because that is what §3.2 makes it: *"a plan is a file you
/// write"*, tracked, so that project truth travels with the repository. Leaving it
/// untracked would also make the fixture's own repository permanently dirty, and
/// `the_user_repository_keeps_its_own_branch_and_checkout` asserts a clean
/// `git status` after a run — correctly, since §4.1's whole promise is that the
/// operator's tree is untouched. An uncommitted `.conductor/` would have made that
/// assertion fail for a reason that has nothing to do with the run.
///
/// Being in the base commit costs nothing: baseline and workspace agree on it, so
/// it appears in no diff the fixtures reconcile. §3.3's rejection rule is about a
/// change *arriving on a run branch*, which is a different thing and has its own
/// tests.
fn write_conductor_layout(root: &Path, objective: &str) {
    // YAML-quoted through serde rather than by hand: a caller's objective may
    // contain the backticks and colons a real instruction does, and a hand-built
    // `"{objective}"` would produce a document that does not parse.
    let objective = serde_json::to_string(objective).expect("a string serializes");
    let plan = format!(
        "plan:\n  id: p-1\n  version: 1\n  objective: \"Prove the vertical.\"\n  \
         milestones:\n    - id: M-01\n      title: \"The vertical\"\n      slices:\n        \
         - id: S-05\n          title: \"First vertical\"\n          tasks:\n            \
         - id: {TASK}\n              objective: {objective}\n              \
         rationale: \"The vertical needs a task that changes a file.\"\n              \
         depends_on: []\n              scope:\n                \
         allowed_globs: [\"src/**\", \"docs/**\"]\n                \
         forbidden_globs: [\".conductor/**\"]\n              \
         verification_profile: .conductor/verification.yaml\n              \
         attempt_budget: 3\n              acceptance_criteria:\n                \
         - id: AC-1\n                  statement: \"The library gains a function.\"\n                  \
         verified_by: [unit-tests]\n"
    );
    for (relative, contents) in [
        (
            ".conductor/project.yaml",
            "project:\n  id: p-1\n  default_branch: main\n  adapter: fake\n  \
             scope_defaults:\n    allowed_globs: [\"src/**\", \"docs/**\"]\n    \
             forbidden_globs: [\".conductor/**\"]\n"
                .to_string(),
        ),
        (".conductor/verification.yaml", PASSING_PROFILE.to_string()),
        (".conductor/policy.yaml", FIXTURE_POLICY.to_string()),
        (".conductor/plans/v1/plan.yaml", plan),
    ] {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir .conductor");
        std::fs::write(&path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }
    // Only when this really is a git repository: `seed_parents_at` is also called
    // with roots that are plain directories.
    if root.join(".git").exists() {
        git(root, &["add", "-A", ".conductor"]);
        // `--allow-empty` because a caller may seed the same root twice; a second
        // call finds nothing to add and must not fail on it.
        git(
            root,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "conductor: project truth (fixture)",
            ],
        );
    }
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
