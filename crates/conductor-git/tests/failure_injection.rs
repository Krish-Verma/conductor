//! Failure injection — master plan S2's "Failure injection" line.
//!
//! Delete the workspace mid-run · corrupt `.git` · leave `index.lock` · move the
//! source repository. Each must produce a *defined* outcome: a `CORRUPT` verdict,
//! a finding, or a clean error. Never a panic, never a silent success.
//!
//! CLAUDE.md: "Prefer failure injection over happy-path assertions for anything
//! touching security, recovery, or concurrency." Reconciliation is all three.

mod common;

use std::path::Path;

use conductor_core::{PolicyHash, RunId, TaskId};
use conductor_git::baseline::observe;
use conductor_git::clone::{WorkspaceRequest, create_workspace};
use conductor_git::reconcile::{Scope, SensitivePatterns, Verdict, reconcile};

use common::{clean_repo, git, git_out, write};

struct Run {
    source: common::Fixture,
    _root: tempfile::TempDir,
    ws: conductor_git::Workspace,
}

impl Run {
    fn path(&self) -> &Path {
        &self.ws.path
    }

    /// Observe and classify, which must never panic whatever state the
    /// repository is in.
    fn classify(&self) -> conductor_git::Reconciliation {
        let observed = observe(self.path(), &self.ws.baseline).expect("observation must not error");
        reconcile(
            &self.ws.baseline,
            &observed,
            &Scope::new(["**".to_string()]),
            &SensitivePatterns::default(),
            None,
            None,
        )
    }
}

fn run() -> Run {
    let source = clean_repo();
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);
    let ws = create_workspace(&WorkspaceRequest {
        source: source.path().to_path_buf(),
        workspace: path,
        run_id: RunId::new("r-0041").expect("valid"),
        task_id: TaskId::new("T-0012").expect("valid"),
        base_commit: head,
        policy_hash: PolicyHash::new("blake3:deadbeef").expect("valid"),
    })
    .expect("create");
    Run {
        source,
        _root: root,
        ws,
    }
}

#[test]
fn a_workspace_deleted_mid_run_is_corrupt_not_no_change() {
    // The dangerous wrong answer is `NO_CHANGE`: a vanished workspace has the
    // same "no observed edits" shape as an agent that did nothing, and one of
    // those routes to a retry while the other must halt.
    let run = run();
    std::fs::remove_dir_all(run.path()).expect("delete workspace");

    let result = run.classify();

    assert_eq!(result.verdict, Verdict::Corrupt, "{result:?}");
    assert!(
        result
            .findings()
            .iter()
            .any(|f| f.detail.contains("workspace is gone")),
        "{:?}",
        result.findings()
    );
}

#[test]
fn a_git_directory_deleted_out_from_under_the_run_is_corrupt() {
    let run = run();
    std::fs::remove_dir_all(run.path().join(".git")).expect("delete .git");

    let result = run.classify();
    assert_eq!(result.verdict, Verdict::Corrupt, "{result:?}");
}

#[test]
fn a_corrupt_head_file_is_corrupt() {
    let run = run();
    std::fs::write(run.path().join(".git/HEAD"), "not a ref\n").expect("clobber HEAD");

    let result = run.classify();
    assert_eq!(result.verdict, Verdict::Corrupt, "{result:?}");
}

#[test]
fn a_deleted_object_store_is_corrupt() {
    let run = run();
    std::fs::remove_dir_all(run.path().join(".git/objects")).expect("delete objects");

    let result = run.classify();
    assert_eq!(result.verdict, Verdict::Corrupt, "{result:?}");
}

#[test]
fn a_left_behind_index_lock_is_corrupt() {
    // The classic residue of a killed git process. Reconciling *through* it
    // would mean reading an index another process may be halfway through.
    let run = run();
    std::fs::write(run.path().join(".git/index.lock"), "").expect("plant lock");

    let result = run.classify();

    assert_eq!(result.verdict, Verdict::Corrupt, "{result:?}");
    assert!(
        result
            .findings()
            .iter()
            .any(|f| f.detail.contains("index.lock")),
        "{:?}",
        result.findings()
    );
}

#[test]
fn a_detached_head_inside_the_workspace_is_corrupt() {
    let run = run();
    git(run.path(), &["checkout", "-q", "--detach", "HEAD"]);

    let result = run.classify();
    assert_eq!(result.verdict, Verdict::Corrupt, "{result:?}");
}

#[test]
fn moving_the_source_repository_leaves_the_run_completely_unaffected() {
    // The workspace has no `origin` and its own object store, so it should not
    // even notice. This is the property that makes §4.1's "the agent never holds
    // a handle to the real repository" true in the operational sense too.
    let run = run();
    write(run.path(), "src/lib.rs", "agent work\n");
    git(run.path(), &["add", "-A"]);
    git(
        run.path(),
        &["commit", "-q", "-m", "work done before the move"],
    );

    let elsewhere = tempfile::tempdir().expect("tempdir");
    let moved_to = elsewhere.path().join("moved-source");
    std::fs::rename(run.source.path(), &moved_to).expect("move the source");

    let result = run.classify();

    assert_eq!(result.verdict, Verdict::CleanNoReport, "{result:?}");
    assert!(result.findings().is_empty(), "{:?}", result.findings());
    assert_eq!(
        git_out(run.path(), &["log", "-1", "--format=%s"]),
        "work done before the move"
    );
}

#[test]
fn deleting_the_source_repository_leaves_the_run_completely_unaffected() {
    let run = run();
    write(run.path(), "src/lib.rs", "agent work\n");

    std::fs::remove_dir_all(run.source.path()).expect("delete the source");

    let result = run.classify();
    assert_eq!(result.verdict, Verdict::CleanNoReport, "{result:?}");
}

#[test]
fn an_unreadable_git_directory_is_a_defined_outcome_not_a_panic() {
    use std::os::unix::fs::PermissionsExt;

    let run = run();
    let git_dir = run.path().join(".git");
    let original = std::fs::metadata(&git_dir).expect("stat").permissions();
    let mut locked = original.clone();
    locked.set_mode(0o000);
    std::fs::set_permissions(&git_dir, locked).expect("chmod 000");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run.classify()));

    // Restore before asserting, so a failure does not leave an undeletable
    // temporary directory behind.
    std::fs::set_permissions(&git_dir, original).expect("restore permissions");

    let result = result.expect("classification must not panic");
    assert_eq!(result.verdict, Verdict::Corrupt, "{result:?}");
}

#[test]
fn observing_a_directory_that_was_never_a_repository_is_corrupt_not_an_error() {
    let run = run();
    let empty = tempfile::tempdir().expect("tempdir");

    let observed = observe(empty.path(), &run.ws.baseline).expect("must not error");
    let result = reconcile(
        &run.ws.baseline,
        &observed,
        &Scope::new(["**".to_string()]),
        &SensitivePatterns::default(),
        None,
        None,
    );

    assert_eq!(result.verdict, Verdict::Corrupt, "{result:?}");
}

#[test]
fn a_workspace_whose_descriptor_was_deleted_still_reconciles() {
    // The descriptor is what makes recovery possible without a database; losing
    // it must not also cost the reconciliation, which needs only the baseline.
    let run = run();
    std::fs::remove_file(run.path().join(conductor_git::DESCRIPTOR_FILENAME))
        .expect("delete descriptor");

    let result = run.classify();
    assert_eq!(result.verdict, Verdict::NoChange, "{result:?}");
}
