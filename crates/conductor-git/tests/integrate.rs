//! The two Conductor-owned git effects — §3.4's trailers, §4.1's integration.
//!
//! These are the *primitives*: making a commit, checking whether one already
//! exists, moving a ref from a clone into the source repository, and deciding
//! whether the target moved. The ledger that decides **whether** to call them
//! lives in `conductor-run` (§4.7); keeping the two apart is what lets the
//! idempotency tests inject a crash between them.

mod common;

use std::path::Path;

use conductor_git::integrate::{
    Divergence, Trailer, Trailers, commit_exists, commit_workspace, fetch_run_branch, ref_sha,
    target_divergence,
};

use common::{clean_repo, commit_all, git, git_out, head, write};

const BRANCH: &str = "conductor/T-0012/r-0041";

/// A source repository and a run clone of it, branched per §4.1.
struct World {
    source: common::Fixture,
    clone: common::Fixture,
}

fn world() -> World {
    let source = clean_repo();
    let clone = common::init_repo();
    // A real §4.1 clone: `--no-hardlinks`, no checkout, then the run branch.
    std::fs::remove_dir_all(clone.path()).expect("clear");
    let status = std::process::Command::new("git")
        .args([
            "clone",
            "--no-hardlinks",
            "--no-checkout",
            &source.path().to_string_lossy(),
            &clone.path().to_string_lossy(),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("clone");
    assert!(status.success());
    git(
        clone.path(),
        &["checkout", "-q", "-b", BRANCH, &head(source.path())],
    );
    git(clone.path(), &["remote", "remove", "origin"]);
    git(clone.path(), &["config", "user.name", "Conductor Agent"]);
    git(
        clone.path(),
        &["config", "user.email", "conductor@localhost"],
    );
    git(clone.path(), &["config", "commit.gpgsign", "false"]);
    World { source, clone }
}

fn trailers() -> Trailers {
    Trailers::new([
        Trailer::new("Conductor-Run", "r-0041"),
        Trailer::new("Conductor-Policy", "blake3:41ef"),
        Trailer::new("Conductor-Verification", "blake3:5b8e"),
    ])
}

#[test]
fn a_conductor_commit_carries_the_trailers_it_was_given() {
    // §3.4: the trailers exist so "the audit trail for anything consequential
    // survives total local state loss and travels with the repository". A
    // trailer that is not in the commit is a trailer that does not survive
    // anything.
    let w = world();
    write(
        w.clone.path(),
        "src/added.rs",
        "pub fn added() -> u32 { 1 }\n",
    );

    let made = commit_workspace(w.clone.path(), BRANCH, "conductor: T-0012", &trailers())
        .expect("commit")
        .expect("something to commit");

    let message = git_out(w.clone.path(), &["log", "-1", "--format=%B", &made.sha]);
    assert!(message.starts_with("conductor: T-0012"));
    for line in [
        "Conductor-Run: r-0041",
        "Conductor-Policy: blake3:41ef",
        "Conductor-Verification: blake3:5b8e",
    ] {
        assert!(
            message.contains(line),
            "commit message is missing {line:?}:\n{message}"
        );
    }

    // git itself must read them back as trailers, not merely as text that
    // happens to be at the end — otherwise `git log --format=%(trailers)`, which
    // is how a human recovers the audit trail, sees nothing.
    let parsed = git_out(
        w.clone.path(),
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Conductor-Run,valueonly)",
            &made.sha,
        ],
    );
    assert_eq!(parsed.trim(), "r-0041");
}

#[test]
fn the_commit_lands_on_the_run_branch_and_carries_the_whole_tree() {
    let w = world();
    write(
        w.clone.path(),
        "src/added.rs",
        "pub fn added() -> u32 { 1 }\n",
    );
    write(w.clone.path(), "docs/new.md", "notes\n");

    let base = head(w.clone.path());
    let made = commit_workspace(w.clone.path(), BRANCH, "conductor: T-0012", &trailers())
        .expect("commit")
        .expect("something to commit");

    assert_eq!(git_out(w.clone.path(), &["rev-parse", BRANCH]), made.sha);
    assert_eq!(
        git_out(w.clone.path(), &["rev-parse", &format!("{}^", made.sha)]),
        base,
        "the commit must sit on top of the branch it was made on"
    );
    assert_eq!(
        git_out(
            w.clone.path(),
            &["rev-parse", &format!("{}^{{tree}}", made.sha)]
        ),
        made.tree
    );
    let files = git_out(
        w.clone.path(),
        &["show", "--name-only", "--format=", &made.sha],
    );
    assert!(files.contains("src/added.rs"), "{files}");
    assert!(files.contains("docs/new.md"), "{files}");
}

#[test]
fn a_clean_workspace_produces_no_commit_at_all() {
    // An empty commit would be a Conductor-owned effect that records nothing,
    // and it would move the branch — so a later "did it happen?" check would
    // answer about a commit that meant nothing.
    let w = world();
    let before = head(w.clone.path());
    let made =
        commit_workspace(w.clone.path(), BRANCH, "conductor: T-0012", &trailers()).expect("commit");
    assert!(
        made.is_none(),
        "nothing changed, so nothing should be committed"
    );
    assert_eq!(head(w.clone.path()), before);
}

#[test]
fn commit_exists_answers_the_question_section_4_7_asks() {
    // §4.7's table: "does a commit with this tree and message exist on the run
    // branch?" — the precondition a restart re-checks instead of retrying.
    let w = world();
    write(
        w.clone.path(),
        "src/added.rs",
        "pub fn added() -> u32 { 1 }\n",
    );

    let tree_before_commit = {
        // Nothing committed yet: the question must answer "no".
        assert!(
            !commit_exists(w.clone.path(), BRANCH, "0000", "Conductor-Run: r-0041").expect("query"),
            "no commit exists yet"
        );
        "0000"
    };
    let _ = tree_before_commit;

    let made = commit_workspace(w.clone.path(), BRANCH, "conductor: T-0012", &trailers())
        .expect("commit")
        .expect("something to commit");

    assert!(
        commit_exists(w.clone.path(), BRANCH, &made.tree, "Conductor-Run: r-0041").expect("query"),
        "the commit that was just made must be found"
    );
    // The marker matters: a commit with the right tree made by a *different*
    // run is not this run's effect.
    assert!(
        !commit_exists(w.clone.path(), BRANCH, &made.tree, "Conductor-Run: r-9999").expect("query"),
        "another run's commit must not satisfy this run's precondition"
    );
    // …and so does the tree.
    assert!(
        !commit_exists(w.clone.path(), BRANCH, "deadbeef", "Conductor-Run: r-0041").expect("query"),
        "a different tree must not satisfy the precondition"
    );
}

#[test]
fn commit_exists_does_not_look_outside_the_run_branch() {
    // The precondition names the run branch on purpose. A commit sitting on some
    // other branch is not this run's integration.
    let w = world();
    write(
        w.clone.path(),
        "src/added.rs",
        "pub fn added() -> u32 { 1 }\n",
    );
    git(w.clone.path(), &["checkout", "-q", "-b", "elsewhere"]);
    let sha = commit_all(
        w.clone.path(),
        "conductor: T-0012\n\nConductor-Run: r-0041\n",
    );
    let tree = git_out(w.clone.path(), &["rev-parse", &format!("{sha}^{{tree}}")]);
    git(w.clone.path(), &["checkout", "-q", BRANCH]);

    assert!(
        !commit_exists(w.clone.path(), BRANCH, &tree, "Conductor-Run: r-0041").expect("query"),
        "a commit on another branch is not on the run branch"
    );
}

// ---------------------------------------------------------------------------
// Integration: fetch the run branch **from** the clone **into** the real repo.
// ---------------------------------------------------------------------------

#[test]
fn the_run_branch_is_fetched_into_the_source_repository() {
    // §4.1: the agent "never holds a handle" on the real repository, so the
    // direction is one-way — Conductor pulls, the workspace never pushes. The
    // clone has had its `origin` removed precisely so it cannot.
    let w = world();
    write(
        w.clone.path(),
        "src/added.rs",
        "pub fn added() -> u32 { 1 }\n",
    );
    let made = commit_workspace(w.clone.path(), BRANCH, "conductor: T-0012", &trailers())
        .expect("commit")
        .expect("a commit");

    let reference = format!("refs/heads/{BRANCH}");
    assert_eq!(
        ref_sha(w.source.path(), &reference).expect("query"),
        None,
        "the source must not know the branch yet"
    );

    let fetched = fetch_run_branch(w.source.path(), w.clone.path(), BRANCH).expect("fetch");
    assert_eq!(fetched.sha, made.sha);
    assert_eq!(fetched.reference, reference);
    assert_eq!(
        ref_sha(w.source.path(), &reference)
            .expect("query")
            .as_deref(),
        Some(made.sha.as_str())
    );

    // The object really arrived, not just the ref.
    assert!(
        git_out(w.source.path(), &["cat-file", "-t", &made.sha]) == "commit",
        "the commit object must be in the source repository"
    );
}

#[test]
fn fetching_does_not_touch_the_target_branch_or_the_working_tree() {
    // §4.1: "Never pushed. Never auto-merged into the default branch." A fetch
    // that moved `main`, or that disturbed the user's checkout, would be exactly
    // the automatic integration the design refuses.
    let w = world();
    write(
        w.clone.path(),
        "src/added.rs",
        "pub fn added() -> u32 { 1 }\n",
    );
    commit_workspace(w.clone.path(), BRANCH, "conductor: T-0012", &trailers())
        .expect("commit")
        .expect("a commit");

    let main_before = git_out(w.source.path(), &["rev-parse", "main"]);
    let status_before = git_out(w.source.path(), &["status", "--porcelain"]);

    fetch_run_branch(w.source.path(), w.clone.path(), BRANCH).expect("fetch");

    assert_eq!(
        git_out(w.source.path(), &["rev-parse", "main"]),
        main_before
    );
    assert_eq!(
        git_out(w.source.path(), &["status", "--porcelain"]),
        status_before
    );
    assert_eq!(
        git_out(w.source.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main",
        "the user's checkout must be where they left it"
    );
}

#[test]
fn fetching_the_same_branch_twice_moves_the_ref_once() {
    // The idempotency the ledger relies on at the git level: re-fetching an
    // unchanged branch is a no-op, so a duplicate would have to come from the
    // ledger, which is where the crash matrix looks for it.
    let w = world();
    write(
        w.clone.path(),
        "src/added.rs",
        "pub fn added() -> u32 { 1 }\n",
    );
    commit_workspace(w.clone.path(), BRANCH, "conductor: T-0012", &trailers())
        .expect("commit")
        .expect("a commit");

    let first = fetch_run_branch(w.source.path(), w.clone.path(), BRANCH).expect("fetch");
    let second = fetch_run_branch(w.source.path(), w.clone.path(), BRANCH).expect("fetch again");
    assert_eq!(first.sha, second.sha);
    assert_eq!(reflog_len(w.source.path(), &first.reference), 1);
}

/// How many times a ref has been updated, from its reflog.
///
/// The honest count: git appends one reflog line per ref update, so this is
/// "how many ref updates happened", not "what does the ref say now".
fn reflog_len(repo: &Path, reference: &str) -> usize {
    let out = std::process::Command::new("git")
        .args(["reflog", "show", "--format=%H", reference])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git reflog");
    if !out.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&out.stdout).lines().count()
}

// ---------------------------------------------------------------------------
// Acceptance row 16 — the target branch moved.
// ---------------------------------------------------------------------------

#[test]
fn a_target_branch_that_has_not_moved_shows_no_divergence() {
    let w = world();
    let base = head(w.source.path());
    assert_eq!(
        target_divergence(w.source.path(), "main", &base).expect("query"),
        None
    );
}

#[test]
fn a_target_branch_that_moved_is_reported_with_both_shas() {
    // Row 16: "Target branch moved | user commits to `main` | divergence at
    // integration | **no rebase, no merge** | … | `AWAITING_REVIEW`".
    let w = world();
    let base = head(w.source.path());
    write(
        w.source.path(),
        "README.md",
        "# fixture\nthe user moved on\n",
    );
    let moved = commit_all(w.source.path(), "user's own commit");
    assert_ne!(moved, base);

    let divergence = target_divergence(w.source.path(), "main", &base)
        .expect("query")
        .expect("the target moved");
    assert_eq!(
        divergence,
        Divergence {
            target_branch: "main".to_string(),
            expected: base,
            actual: moved,
        }
    );
    // The report has to carry both shas: "it moved" without saying from what and
    // to what is not a divergence a human can act on.
    let rendered = divergence.to_string();
    assert!(rendered.contains(&divergence.expected));
    assert!(rendered.contains(&divergence.actual));
}

#[test]
fn a_target_branch_that_does_not_exist_is_an_error_not_a_silent_no_divergence() {
    // Answering "no divergence" for a ref that is not there would let a run
    // integrate into a branch nobody has.
    let w = world();
    let base = head(w.source.path());
    assert!(
        target_divergence(w.source.path(), "no-such-branch", &base).is_err(),
        "a missing target branch must be an error"
    );
}
