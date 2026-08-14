//! Hashing the **working tree** — the identity §4.5 binds every check result to.
//!
//! `Baseline::tree_hash` is `HEAD^{tree}`: what was committed. That is the
//! wrong identity for verification, because an agent's edits are uncommitted
//! and the mutation §4.5 exists to catch changes the working tree without
//! touching `HEAD`. These tests pin the properties the `VOID` outcome rests on.

use std::path::Path;
use std::process::Command;

use conductor_git::tree::TreeHasher;

/// A repository with one commit, a `.gitignore` and an ignored directory.
fn repo(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    std::fs::create_dir_all(repo.join("target")).expect("mkdir");
    std::fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n").expect("write");
    std::fs::write(repo.join(".gitignore"), "/target/\n").expect("write");
    std::fs::write(repo.join("target/build.log"), "before\n").expect("write");
    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@localhost"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "initial"]);
    repo
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn porcelain(repo: &Path) -> String {
    let out = Command::new("git")
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
        ])
        .current_dir(repo)
        .output()
        .expect("git status");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn hasher(dir: &Path, repo: &Path) -> TreeHasher {
    TreeHasher::new(repo, &dir.join("scratch").join("index"))
        .expect("a scratch index outside the workspace is valid")
}

#[test]
fn the_same_tree_hashes_the_same_way_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo(dir.path());
    let h = hasher(dir.path(), &repo);
    assert_eq!(h.hash().expect("hash"), h.hash().expect("hash"));
}

#[test]
fn editing_a_tracked_file_moves_the_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo(dir.path());
    let h = hasher(dir.path(), &repo);

    let before = h.hash().expect("hash");
    std::fs::write(repo.join("src/lib.rs"), "pub fn a() { }\n").expect("write");
    assert_ne!(before, h.hash().expect("hash"));
}

#[test]
fn an_untracked_file_moves_the_hash() {
    // The case that matters: a file an agent creates mid-check has never been
    // committed and never been staged, and it must still count.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo(dir.path());
    let h = hasher(dir.path(), &repo);

    let before = h.hash().expect("hash");
    std::fs::write(repo.join("src/injected.rs"), "// stray\n").expect("write");
    assert_ne!(before, h.hash().expect("hash"));
}

#[test]
fn deleting_a_tracked_file_moves_the_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo(dir.path());
    let h = hasher(dir.path(), &repo);

    let before = h.hash().expect("hash");
    std::fs::remove_file(repo.join("src/lib.rs")).expect("remove");
    assert_ne!(before, h.hash().expect("hash"));
}

#[test]
fn writing_to_a_gitignored_path_does_not_move_the_hash() {
    // Load-bearing, not a nicety. Nearly every real check writes to an ignored
    // path — cargo fills `target/`, npm fills `node_modules/`. If those counted,
    // every check would VOID itself the instant it did any work and §4.5's
    // mutation detector would fire constantly while detecting nothing.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo(dir.path());
    let h = hasher(dir.path(), &repo);

    let before = h.hash().expect("hash");
    std::fs::write(repo.join("target/build.log"), "a much later build\n").expect("write");
    std::fs::write(repo.join("target/new-artifact"), vec![0u8; 4096]).expect("write");
    assert_eq!(
        before,
        h.hash().expect("hash"),
        "a build artefact must not void a verification result"
    );
}

#[test]
fn hashing_leaves_the_workspace_exactly_as_it_found_it() {
    // Observation must not move what it observes: `git add` in the workspace's
    // own index would stage the agent's work as a side effect of looking at it,
    // and §4.8 reconciles staged against unstaged.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo(dir.path());
    std::fs::write(repo.join("src/lib.rs"), "pub fn a() { /* edited */ }\n").expect("write");
    std::fs::write(repo.join("src/untracked.rs"), "// new\n").expect("write");

    let before = porcelain(&repo);
    let h = hasher(dir.path(), &repo);
    h.hash().expect("hash");
    h.hash().expect("hash");
    assert_eq!(before, porcelain(&repo), "hashing disturbed the workspace");
    assert!(
        !repo.join(".git").join("index.lock").exists(),
        "hashing left an index lock behind"
    );
}

#[test]
fn a_scratch_index_inside_the_workspace_is_refused() {
    // An index file written inside the working tree is itself an untracked
    // file, so it would enter the next `git add -A` and change the very hash it
    // is being used to compute.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo(dir.path());
    let error = TreeHasher::new(&repo, &repo.join("conductor-index"))
        .expect_err("an index inside the workspace must be refused");
    assert!(
        error.to_string().contains("inside the workspace"),
        "unhelpful message: {error}"
    );
}

#[test]
fn the_working_tree_hash_differs_from_the_committed_tree_when_the_tree_is_dirty() {
    // The whole reason this exists: `HEAD^{tree}` cannot see uncommitted work.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo(dir.path());
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn a() { /* uncommitted */ }\n",
    )
    .expect("write");

    let committed = conductor_git::capture_baseline(&repo)
        .expect("baseline")
        .tree_hash;
    let working = hasher(dir.path(), &repo).hash().expect("hash");
    assert_ne!(
        committed,
        working.as_str(),
        "a working-tree hash that equals HEAD's tree cannot see the agent's edits"
    );
}
