//! Orphan quarantine — master plan §4.1, acceptance row 18.
//!
//! "Orphans found at startup are **quarantined**, never deleted — an orphan may
//! hold the only copy of an hour of work." Every test here is ultimately the
//! same assertion: the bytes still exist afterwards.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use conductor_core::{PolicyHash, RunId, TaskId};
use conductor_git::clone::{WorkspaceRequest, create_workspace};
use conductor_git::quarantine::{OrphanReason, find_orphans, quarantine};

use common::{clean_repo, git, git_out, write};

/// Build a real workspace under `root` for `run_id`.
fn workspace_for(root: &Path, run_id: &str) -> std::path::PathBuf {
    let source = clean_repo();
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);
    let path = root.join(run_id);
    create_workspace(&WorkspaceRequest {
        source: source.path().to_path_buf(),
        workspace: path.clone(),
        run_id: RunId::new(run_id).expect("valid"),
        task_id: TaskId::new("T-0012").expect("valid"),
        base_commit: head,
        policy_hash: PolicyHash::new("blake3:deadbeef").expect("valid"),
    })
    .expect("create");
    // The source fixture is dropped here; a workspace does not need it, which is
    // itself part of the isolation claim.
    path
}

fn active(ids: &[&str]) -> BTreeSet<RunId> {
    ids.iter().map(|i| RunId::new(*i).expect("valid")).collect()
}

#[test]
fn a_workspace_whose_run_is_still_active_is_not_an_orphan() {
    let root = tempfile::tempdir().expect("tempdir");
    workspace_for(root.path(), "r-0001");

    let orphans = find_orphans(root.path(), &active(&["r-0001"])).expect("scan");
    assert!(orphans.is_empty(), "{orphans:?}");
}

#[test]
fn a_workspace_whose_run_is_gone_is_an_orphan_identified_by_its_descriptor() {
    let root = tempfile::tempdir().expect("tempdir");
    workspace_for(root.path(), "r-0002");

    let orphans = find_orphans(root.path(), &active(&["r-0001"])).expect("scan");

    assert_eq!(orphans.len(), 1, "{orphans:?}");
    assert_eq!(orphans[0].reason, OrphanReason::RunNotActive);
    assert_eq!(
        orphans[0]
            .descriptor
            .as_ref()
            .expect("descriptor")
            .run_id
            .as_str(),
        "r-0002"
    );
}

#[test]
fn a_directory_with_no_descriptor_is_an_orphan_too() {
    // An unidentifiable directory is the case where deleting would be most
    // tempting and most dangerous.
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(root.path().join("mystery")).expect("mkdir");
    std::fs::write(root.path().join("mystery/work.txt"), "an hour of work\n").expect("write");

    let orphans = find_orphans(root.path(), &active(&[])).expect("scan");

    assert_eq!(orphans.len(), 1, "{orphans:?}");
    assert_eq!(orphans[0].reason, OrphanReason::NoDescriptor);
    assert!(orphans[0].descriptor.is_none());
}

#[test]
fn scanning_a_root_that_does_not_exist_is_not_an_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let orphans = find_orphans(&root.path().join("never-created"), &active(&[])).expect("scan");
    assert!(orphans.is_empty());
}

#[test]
fn quarantine_moves_the_workspace_and_keeps_every_byte() {
    let root = tempfile::tempdir().expect("tempdir");
    let quarantine_root = tempfile::tempdir().expect("tempdir");
    let ws = workspace_for(root.path(), "r-0003");
    write(
        &ws,
        "src/agent.rs",
        "an hour of work that exists nowhere else\n",
    );

    let orphans = find_orphans(root.path(), &active(&[])).expect("scan");
    let moved = quarantine(&orphans[0], quarantine_root.path()).expect("quarantine");

    assert!(!ws.exists(), "the orphan must leave the workspaces root");
    assert!(moved.to.starts_with(quarantine_root.path()));
    assert_eq!(
        std::fs::read_to_string(moved.to.join("src/agent.rs")).expect("read"),
        "an hour of work that exists nowhere else\n"
    );
}

#[test]
fn a_quarantined_workspace_is_still_a_working_repository() {
    // Rescuing the bytes is not enough: the value in an orphan is usually a
    // commit, and a commit is only recoverable if the repository still opens.
    let root = tempfile::tempdir().expect("tempdir");
    let quarantine_root = tempfile::tempdir().expect("tempdir");
    let ws = workspace_for(root.path(), "r-0004");
    write(&ws, "src/agent.rs", "work\n");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "the only copy of this work"]);
    let commit = git_out(&ws, &["rev-parse", "HEAD"]);

    let orphans = find_orphans(root.path(), &active(&[])).expect("scan");
    let moved = quarantine(&orphans[0], quarantine_root.path()).expect("quarantine");

    assert_eq!(git_out(&moved.to, &["rev-parse", "HEAD"]), commit);
    assert_eq!(
        git_out(&moved.to, &["log", "-1", "--format=%s"]),
        "the only copy of this work"
    );
}

#[test]
fn quarantining_two_orphans_with_the_same_name_does_not_overwrite_the_first_rescue() {
    let quarantine_root = tempfile::tempdir().expect("tempdir");

    let first_root = tempfile::tempdir().expect("tempdir");
    let first = workspace_for(first_root.path(), "r-0005");
    write(&first, "marker.txt", "first\n");
    let first_orphans = find_orphans(first_root.path(), &active(&[])).expect("scan");
    let first_moved = quarantine(&first_orphans[0], quarantine_root.path()).expect("quarantine");

    let second_root = tempfile::tempdir().expect("tempdir");
    let second = workspace_for(second_root.path(), "r-0005");
    write(&second, "marker.txt", "second\n");
    let second_orphans = find_orphans(second_root.path(), &active(&[])).expect("scan");
    let second_moved = quarantine(&second_orphans[0], quarantine_root.path()).expect("quarantine");

    assert_ne!(first_moved.to, second_moved.to);
    assert_eq!(
        std::fs::read_to_string(first_moved.to.join("marker.txt")).expect("read"),
        "first\n",
        "the first rescue must survive the second"
    );
    assert_eq!(
        std::fs::read_to_string(second_moved.to.join("marker.txt")).expect("read"),
        "second\n"
    );
}

#[test]
fn quarantine_never_removes_a_workspace_it_cannot_move() {
    // Failing to rescue is acceptable. Deleting because the rescue failed is not.
    let root = tempfile::tempdir().expect("tempdir");
    let ws = workspace_for(root.path(), "r-0006");
    write(&ws, "marker.txt", "irreplaceable\n");
    let orphans = find_orphans(root.path(), &active(&[])).expect("scan");

    // A quarantine root that cannot be created, because a file sits where the
    // directory would go.
    let blocked = tempfile::tempdir().expect("tempdir");
    let blocked_path = blocked.path().join("not-a-directory");
    std::fs::write(&blocked_path, "in the way").expect("write");

    let error = quarantine(&orphans[0], &blocked_path).expect_err("must fail");

    assert!(ws.exists(), "the orphan must still be there: {error}");
    assert_eq!(
        std::fs::read_to_string(ws.join("marker.txt")).expect("read"),
        "irreplaceable\n"
    );
}
