//! Workspace creation — master plan §4.1, ADR-0001.
//!
//! The exact command sequence is the deliverable, so these tests assert the
//! *observable consequences* of each line of it rather than the lines.

mod common;

use std::path::Path;

use conductor_core::{PolicyHash, RunId, TaskId};
use conductor_git::clone::{WorkspaceRequest, create_workspace};
use conductor_git::error::GitError;

use common::{
    clean_repo, dirty_repo, git, git_out, nested_repo, object_store_contents, submodule_repo,
};

fn request(source: &Path, workspace: &Path, base_commit: &str) -> WorkspaceRequest {
    WorkspaceRequest {
        source: source.to_path_buf(),
        workspace: workspace.to_path_buf(),
        run_id: RunId::new("r-0041").expect("valid run id"),
        task_id: TaskId::new("T-0012").expect("valid task id"),
        base_commit: base_commit.to_string(),
        policy_hash: PolicyHash::new("blake3:deadbeef").expect("valid policy hash"),
    }
}

/// A scratch directory to hold workspaces, kept separate from the source's temp
/// dir so nothing about the source's parent can be mistaken for isolation.
fn workspace_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn no_object_file_in_the_workspace_shares_an_inode_with_the_source() {
    // M1/M2: the default clone hardlinks loose objects *and* packs, and that is
    // exactly how a write inside the clone reaches the source. This asserts the
    // mechanism, not just the outcome.
    use std::os::unix::fs::MetadataExt;

    let source = clean_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    create_workspace(&request(source.path(), &ws, &head)).expect("create workspace");

    let source_inodes: std::collections::BTreeSet<u64> = common::loose_object_files(source.path())
        .into_iter()
        .chain(common::pack_files(source.path()))
        .map(|p| std::fs::metadata(&p).expect("stat").ino())
        .collect();

    let workspace_objects: Vec<_> = common::loose_object_files(&ws)
        .into_iter()
        .chain(common::pack_files(&ws))
        .collect();
    assert!(
        !workspace_objects.is_empty(),
        "the workspace must actually contain objects for this test to mean anything"
    );

    for object in workspace_objects {
        let meta = std::fs::metadata(&object).expect("stat");
        assert_eq!(
            meta.nlink(),
            1,
            "{object:?} has {} links; --no-hardlinks was not applied",
            meta.nlink()
        );
        assert!(
            !source_inodes.contains(&meta.ino()),
            "{object:?} shares an inode with the source object store"
        );
    }
}

#[test]
fn head_is_the_run_branch_at_the_base_commit() {
    let source = clean_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let base = git_out(source.path(), &["rev-list", "--max-parents=0", "HEAD"]);

    let workspace = create_workspace(&request(source.path(), &ws, &base)).expect("create");

    assert_eq!(workspace.branch, "conductor/T-0012/r-0041");
    assert_eq!(
        git_out(&ws, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "conductor/T-0012/r-0041"
    );
    assert_eq!(git_out(&ws, &["rev-parse", "HEAD"]), base);
    // --no-checkout followed by checkout -b means the tree is the base commit's,
    // not the source HEAD's.
    assert!(
        ws.join("README.md").exists(),
        "the base commit's tree must be checked out"
    );
    assert!(
        !ws.join("src/three.rs").exists(),
        "files added after the base commit must not be present"
    );
}

#[test]
fn the_workspace_has_no_remote_to_push_to() {
    let source = clean_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    create_workspace(&request(source.path(), &ws, &head)).expect("create");

    assert_eq!(
        git_out(&ws, &["remote"]),
        "",
        "origin must be removed: §4.9 layer 6 is credential absence, and a run \
         clone with no remote has nothing to push to"
    );
    assert_eq!(
        git_out(
            &ws,
            &["for-each-ref", "--format=%(refname)", "refs/remotes"]
        ),
        "",
        "removing the remote must take its remote-tracking refs with it"
    );
}

#[test]
fn hooks_are_disabled_and_the_committer_is_the_conductor_agent() {
    let source = clean_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    create_workspace(&request(source.path(), &ws, &head)).expect("create");

    assert_eq!(
        git_out(&ws, &["config", "--local", "core.hooksPath"]),
        "/dev/null"
    );
    assert_eq!(
        git_out(&ws, &["config", "--local", "user.name"]),
        "Conductor Agent"
    );
    assert_eq!(
        git_out(&ws, &["config", "--local", "user.email"]),
        "conductor@localhost"
    );
    assert_eq!(
        git_out(&ws, &["config", "--local", "commit.gpgsign"]),
        "false"
    );
}

#[test]
fn a_hook_planted_in_the_workspace_never_runs() {
    // core.hooksPath=/dev/null is only worth setting if it has this effect.
    let source = clean_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);
    create_workspace(&request(source.path(), &ws, &head)).expect("create");

    let hook = ws.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\ntouch \"$(git rev-parse --show-toplevel)/HOOK_FIRED\"\nexit 1\n",
    )
    .expect("write hook");
    let mut perms = std::fs::metadata(&hook).expect("stat").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&hook, perms).expect("chmod");

    std::fs::write(ws.join("touched.txt"), "x").expect("write");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "commit with a planted hook"]);

    assert!(
        !ws.join("HOOK_FIRED").exists(),
        "core.hooksPath=/dev/null must stop a planted hook from executing"
    );
}

#[test]
fn an_agent_committing_everything_does_not_capture_conductors_own_descriptor() {
    // `git add -A` is the single most likely thing an agent does. If the
    // descriptor is merely untracked, that command commits it onto the run
    // branch — and the run branch is what Conductor later fetches into the
    // operator's real repository. Conductor's bookkeeping would end up in the
    // user's history.
    let source = clean_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);
    create_workspace(&request(source.path(), &ws, &head)).expect("create");

    std::fs::write(ws.join("src/agent.rs"), "pub fn work() {}\n").expect("write");
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "agent commits everything"]);

    let tracked = git_out(&ws, &["ls-files"]);
    assert!(
        !tracked.contains(".conductor-run.json"),
        "the descriptor must not be committable by accident; tracked files: {tracked}"
    );
    assert!(
        ws.join(".conductor-run.json").exists(),
        "and it must still be on disk, because recovery reads it"
    );
}

#[test]
fn a_source_with_submodules_is_refused() {
    // §4.1: v1 refuses submodules. A hard error, not a finding.
    let source = submodule_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    let err = create_workspace(&request(source.path(), &ws, &head)).expect_err("must refuse");

    match err {
        GitError::SubmodulesUnsupported(path, status) => {
            assert_eq!(path, source.path());
            assert!(
                status.contains("vendor/inner"),
                "the error must name what it found: {status}"
            );
        }
        other => panic!("expected SubmodulesUnsupported, got {other:?}"),
    }
    assert!(
        !ws.exists(),
        "a refused registration must leave nothing behind"
    );
}

#[test]
fn an_unregistered_nested_repository_does_not_count_as_a_submodule() {
    // §4.1 treats nested repositories differently from submodules: detected at
    // baseline, not refused at registration.
    let source = nested_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    create_workspace(&request(source.path(), &ws, &head)).expect("nested repos are allowed");
}

#[test]
fn an_existing_workspace_path_is_refused() {
    let source = clean_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    std::fs::create_dir_all(&ws).expect("create dir");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    let err = create_workspace(&request(source.path(), &ws, &head)).expect_err("must refuse");
    assert!(matches!(err, GitError::WorkspaceExists(_)), "got {err:?}");
}

#[test]
fn a_source_that_is_not_a_repository_is_refused() {
    let source = tempfile::tempdir().expect("tempdir");
    let root = workspace_root();
    let ws = root.path().join("run");

    let err = create_workspace(&request(source.path(), &ws, "HEAD")).expect_err("must refuse");
    assert!(matches!(err, GitError::NotARepository(_)), "got {err:?}");
}

#[test]
fn a_base_commit_that_does_not_exist_is_a_clean_error_and_leaves_nothing_behind() {
    let source = clean_repo();
    let root = workspace_root();
    let ws = root.path().join("run");

    let err = create_workspace(&request(
        source.path(),
        &ws,
        "0000000000000000000000000000000000000000",
    ))
    .expect_err("must fail");
    assert!(matches!(err, GitError::Command { .. }), "got {err:?}");
    assert!(
        !ws.exists(),
        "a half-built workspace is worse than none: it would be indistinguishable \
         from an orphan holding real work"
    );
}

#[test]
fn a_dirty_source_is_cloned_from_the_commit_and_left_byte_identical() {
    // Acceptance row 17. Worktrees would share the index and the stash; a clone
    // from a commit copies neither.
    let source = dirty_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    let before_objects = object_store_contents(source.path());
    let before_status = git_out(source.path(), &["status", "--porcelain=v2"]);
    let before_config = std::fs::read(source.path().join(".git/config")).expect("read config");

    create_workspace(&request(source.path(), &ws, &head)).expect("create");

    assert_eq!(
        object_store_contents(source.path()),
        before_objects,
        "cloning must not write to the source object store"
    );
    assert_eq!(
        git_out(source.path(), &["status", "--porcelain=v2"]),
        before_status,
        "the user's uncommitted work must be exactly as they left it"
    );
    assert_eq!(
        std::fs::read(source.path().join(".git/config")).expect("read config"),
        before_config
    );

    // And none of the dirt made it into the workspace.
    assert_eq!(
        git_out(&ws, &["status", "--porcelain=v2", "--untracked-files=all"])
            .lines()
            .filter(|l| !l.contains(".conductor-run.json"))
            .count(),
        0,
        "the workspace starts from a commit, so it starts clean"
    );
}

#[test]
fn a_detached_source_can_still_be_cloned_at_an_explicit_base_commit() {
    let source = common::detached_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    let workspace = create_workspace(&request(source.path(), &ws, &head)).expect("create");

    assert_eq!(git_out(&ws, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_out(&ws, &["rev-parse", "--abbrev-ref", "HEAD"]),
        workspace.branch,
        "the workspace is never detached, whatever the source was doing"
    );
}

#[test]
fn a_large_source_clones_within_the_revisit_threshold() {
    // ADR-0001's pre-registered revisit trigger is a clone exceeding 10 s. This
    // is not a benchmark; it is the tripwire that says "go read the ADR".
    let source = common::large_repo();
    let root = workspace_root();
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    let started = std::time::Instant::now();
    create_workspace(&request(source.path(), &ws, &head)).expect("create");
    let elapsed = started.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "clone took {elapsed:?}; ADR-0001's revisit trigger has fired — read it \
         before changing anything, and never revisit toward hardlinks"
    );
}
