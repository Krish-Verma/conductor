//! Baseline capture and observation — master plan §4.1 and the §4.8 surface.
//!
//! Capture is the *only* I/O half of reconciliation. Everything it records has
//! to be something a later delta can be computed against, because §4.8 says any
//! unexplained delta raises a finding — and a delta cannot be computed against a
//! field nobody captured.

mod common;

use std::path::Path;

use conductor_core::{PolicyHash, RunId, TaskId};
use conductor_git::baseline::{capture_baseline, observe};
use conductor_git::clone::{WorkspaceRequest, create_workspace};

use common::{clean_repo, dirty_repo, git, git_out, nested_repo, submodule_repo, write};

fn request(source: &Path, workspace: &Path, base_commit: &str) -> WorkspaceRequest {
    WorkspaceRequest {
        source: source.to_path_buf(),
        workspace: workspace.to_path_buf(),
        run_id: RunId::new("r-0041").expect("valid"),
        task_id: TaskId::new("T-0012").expect("valid"),
        base_commit: base_commit.to_string(),
        policy_hash: PolicyHash::new("blake3:deadbeef").expect("valid"),
    }
}

/// A workspace cloned from a clean source at its HEAD.
fn workspace() -> (common::Fixture, tempfile::TempDir, conductor_git::Workspace) {
    let source = clean_repo();
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);
    let created = create_workspace(&request(source.path(), &ws, &head)).expect("create");
    (source, root, created)
}

#[test]
fn the_baseline_records_the_commit_and_tree_the_run_is_pinned_to() {
    let (_source, _root, ws) = workspace();
    let baseline = &ws.baseline;

    assert_eq!(
        baseline.base_commit,
        git_out(&ws.path, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        baseline.tree_hash,
        git_out(&ws.path, &["rev-parse", "HEAD^{tree}"])
    );
    assert_eq!(baseline.head_branch.as_deref(), Some(ws.branch.as_str()));
}

#[test]
fn the_descriptor_is_invisible_to_the_baseline_so_it_is_never_mistaken_for_agent_work() {
    // Two mechanisms guard this, and the test asserts the outcome rather than
    // either one: the descriptor is excluded via `.git/info/exclude`, and the
    // baseline is captured after it is written. Were it visible, Conductor would
    // raise a finding against itself on every run.
    let (_source, _root, ws) = workspace();
    assert!(
        !ws.baseline
            .untracked
            .iter()
            .any(|p| p == conductor_git::DESCRIPTOR_FILENAME),
        "descriptor leaked into the baseline untracked list: {:?}",
        ws.baseline.untracked
    );
    assert!(
        !ws.baseline
            .status
            .iter()
            .any(|e| e.path == conductor_git::DESCRIPTOR_FILENAME),
        "descriptor leaked into the baseline status: {:?}",
        ws.baseline.status
    );
    assert!(ws.path.join(conductor_git::DESCRIPTOR_FILENAME).exists());
}

#[test]
fn the_baseline_records_local_config_including_the_isolation_settings() {
    let (_source, _root, ws) = workspace();
    let config = &ws.baseline.config;

    assert_eq!(
        config.get("core.hookspath").map(Vec::as_slice),
        Some(["/dev/null".to_string()].as_slice())
    );
    assert_eq!(
        config.get("user.email").map(Vec::as_slice),
        Some(["conductor@localhost".to_string()].as_slice())
    );
    assert!(
        !config.contains_key("remote.origin.url"),
        "origin was removed, so its config must be gone too: {config:?}"
    );
}

#[test]
fn the_baseline_records_remotes_and_a_workspace_has_none() {
    let (_source, _root, ws) = workspace();
    assert!(ws.baseline.remotes.is_empty(), "{:?}", ws.baseline.remotes);
}

#[test]
fn the_baseline_records_every_ref_with_its_object_id() {
    let (_source, _root, ws) = workspace();
    let refs = &ws.baseline.refs;

    let branch_ref = format!("refs/heads/{}", ws.branch);
    assert_eq!(
        refs.get(&branch_ref),
        Some(&ws.baseline.base_commit),
        "run branch missing from refs: {refs:?}"
    );
    assert!(
        refs.keys().any(|r| r == "refs/heads/main"),
        "the cloned source branches are refs too: {refs:?}"
    );
}

#[test]
fn the_baseline_lists_the_hooks_directory() {
    let (_source, _root, ws) = workspace();
    assert!(
        !ws.baseline.hooks.is_empty(),
        "a fresh clone has sample hooks; an empty listing means nothing was looked at"
    );
    assert!(
        ws.baseline.hooks.values().all(|h| !h.content_id.is_empty()),
        "each hook needs a content id, or a rewritten hook is invisible"
    );
}

#[test]
fn the_baseline_counts_stash_entries() {
    let source = dirty_repo();
    let before = capture_baseline(source.path()).expect("capture");
    assert_eq!(before.stash_count, 0);

    git(source.path(), &["stash", "push", "-q", "-m", "user work"]);

    let after = capture_baseline(source.path()).expect("capture");
    assert_eq!(after.stash_count, 1);
}

#[test]
fn the_baseline_records_a_dirty_repository_as_dirty() {
    // §4.1: "Baseline records the dirty state so a later finding can distinguish
    // 'user had this modified' from 'the agent did it'."
    let source = dirty_repo();
    let baseline = capture_baseline(source.path()).expect("capture");

    assert!(
        baseline.status.iter().any(|e| e.path == "README.md"),
        "modified tracked file missing: {:?}",
        baseline.status
    );
    assert!(
        baseline.untracked.iter().any(|p| p == "scratch.txt"),
        "untracked file missing: {:?}",
        baseline.untracked
    );
}

#[test]
fn the_baseline_records_submodule_status() {
    let source = submodule_repo();
    let baseline = capture_baseline(source.path()).expect("capture");
    assert!(
        baseline
            .submodules
            .iter()
            .any(|s| s.contains("vendor/inner")),
        "{:?}",
        baseline.submodules
    );
}

#[test]
fn the_baseline_detects_nested_repositories() {
    // §4.1: detected at baseline, excluded from scope checks, finding if modified.
    let source = nested_repo();
    let baseline = capture_baseline(source.path()).expect("capture");

    let nested = baseline
        .nested_repos
        .iter()
        .find(|n| n.path == "vendor/nested")
        .unwrap_or_else(|| panic!("nested repo not detected: {:?}", baseline.nested_repos));
    assert!(
        nested.head.is_some(),
        "a nested repo's HEAD is its identity"
    );
    assert!(!nested.dirty);
}

#[test]
fn observation_reports_new_commits_with_their_parents() {
    let (_source, _root, ws) = workspace();
    write(&ws.path, "src/new.rs", "pub fn added() {}\n");
    git(&ws.path, &["add", "-A"]);
    git(&ws.path, &["commit", "-q", "-m", "agent commit"]);

    let observed = observe(&ws.path, &ws.baseline).expect("observe");

    assert_eq!(observed.new_commits.len(), 1, "{:?}", observed.new_commits);
    let commit = &observed.new_commits[0];
    assert_eq!(commit.parents, vec![ws.baseline.base_commit.clone()]);
    assert_eq!(commit.subject, "agent commit");
    assert!(
        observed
            .committed_changes
            .iter()
            .any(|c| c.path == "src/new.rs"),
        "{:?}",
        observed.committed_changes
    );
}

#[test]
fn observation_separates_staged_from_unstaged_changes() {
    let (_source, _root, ws) = workspace();
    write(&ws.path, "README.md", "staged edit\n");
    git(&ws.path, &["add", "README.md"]);
    write(&ws.path, "src/lib.rs", "// unstaged edit\n");

    let observed = observe(&ws.path, &ws.baseline).expect("observe");

    assert!(
        observed.staged.iter().any(|c| c.path == "README.md"),
        "{:?}",
        observed.staged
    );
    assert!(
        observed.unstaged.iter().any(|c| c.path == "src/lib.rs"),
        "{:?}",
        observed.unstaged
    );
}

#[test]
fn observation_captures_the_reflog() {
    // §4.8 lists the reflog in the reconciled surface: it is the only record of
    // a ref that was moved and moved back.
    let (_source, _root, ws) = workspace();
    write(&ws.path, "a.txt", "a\n");
    git(&ws.path, &["add", "-A"]);
    git(&ws.path, &["commit", "-q", "-m", "one"]);
    git(&ws.path, &["reset", "-q", "--hard", "HEAD~1"]);

    let observed = observe(&ws.path, &ws.baseline).expect("observe");
    assert!(
        observed.reflog.iter().any(|e| e.contains("reset")),
        "reflog must show the reset: {:?}",
        observed.reflog
    );
}

#[test]
fn a_healthy_workspace_observes_as_healthy() {
    let (_source, _root, ws) = workspace();
    let observed = observe(&ws.path, &ws.baseline).expect("observe");
    let health = &observed.health;

    assert!(health.workspace_present);
    assert!(health.git_dir_readable);
    assert!(health.head_resolvable);
    assert!(!health.detached_head);
    assert!(!health.merge_in_progress);
    assert!(!health.index_lock_present);
    assert!(health.object_store_ok);
    assert!(health.is_healthy(), "{health:?}");
}
