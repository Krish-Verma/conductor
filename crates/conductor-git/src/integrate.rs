//! The two Conductor-owned git effects, and the check that guards them.
//!
//! §4.7 names exactly two git effects Conductor may own, and gives each a
//! post-hoc precondition:
//!
//! | kind | did it happen? |
//! |---|---|
//! | `git.commit.local` | does a commit with this tree and message exist on the run branch? |
//! | `git.fetch_into_main` | does the target ref point at the expected sha? |
//!
//! This module is the *world* half of that: it performs the effects and answers
//! the questions. The ledger that decides **whether** to perform them lives in
//! `conductor-run`, because §4.7's intent/receipt cycle needs the store. Keeping
//! them apart is what makes it possible to inject a crash between the two.
//!
//! # Why the precondition is "tree and message", not "sha"
//!
//! A commit's sha depends on its committer timestamp and its parent. A restarted
//! Conductor can reproduce neither, so it cannot predict the sha of the commit
//! it was about to make — and asking "is sha X on the branch?" would answer *no*
//! for a commit it had in fact just made, and the effect would happen twice.
//! §4.7 chose the question this module implements: the **tree** is the content
//! the effect was for, and the **message marker** is §3.4's `Conductor-Run`
//! trailer, which says whose effect it was.
//!
//! # Direction, and what integration is not
//!
//! §4.1: the agent "never holds a handle" on the real repository. Integration is
//! therefore a `git fetch` **from** the clone **into** the source repo — the
//! clone has had its `origin` removed and has nowhere to push. And Conductor
//! "never rebases or merges automatically": the fetch creates a ref and moves
//! nothing else, so a human decides what becomes of the branch.

use std::fmt;
use std::path::Path;

use serde::Serialize;

use crate::error::{GitError, GitResult};
use crate::git::{run_git, run_git_ok};

/// One `Key: value` line at the end of a commit message (§3.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trailer {
    /// The key, e.g. `Conductor-Run`.
    pub key: String,
    /// The value.
    pub value: String,
}

impl Trailer {
    /// Build a trailer.
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Trailer {
        Trailer {
            key: key.into(),
            value: value.into(),
        }
    }

    /// The rendered line.
    pub fn line(&self) -> String {
        format!("{}: {}", self.key, self.value)
    }
}

/// The trailers a Conductor-owned commit carries.
///
/// A list rather than a struct with five named fields, because §3.4's five are
/// not all available in every slice and an `Option` per field would invite
/// emitting `Conductor-Plan: <none>`. What is real is present; what is not is
/// absent. §3.5's recovery story — "read commit trailers to reconstruct which
/// runs produced which commits" — is served by an absent trailer and actively
/// damaged by a fabricated one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Trailers {
    lines: Vec<Trailer>,
}

impl Trailers {
    /// Build from trailers that have a real source.
    pub fn new(trailers: impl IntoIterator<Item = Trailer>) -> Trailers {
        Trailers {
            lines: trailers.into_iter().collect(),
        }
    }

    /// The trailers, in order.
    pub fn lines(&self) -> &[Trailer] {
        &self.lines
    }

    /// The value of one key, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines
            .iter()
            .find(|t| t.key == key)
            .map(|t| t.value.as_str())
    }

    /// Render the trailer block that follows the message body.
    pub fn render(&self) -> String {
        self.lines
            .iter()
            .map(|t| t.line())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A commit Conductor made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MadeCommit {
    /// The commit id.
    pub sha: String,
    /// `commit^{tree}` — the content the effect recorded.
    pub tree: String,
}

/// A ref update Conductor made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FetchedRef {
    /// Fully-qualified ref name in the source repository.
    pub reference: String,
    /// What it now points at.
    pub sha: String,
}

/// The target ref moved while the run was in flight (§4.1, acceptance row 16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Divergence {
    /// The branch the run was going to integrate into.
    pub target_branch: String,
    /// What it pointed at when the run started — the run's `base_commit`.
    pub expected: String,
    /// What it points at now.
    pub actual: String,
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} moved from {} to {} while the run was in flight; Conductor never \
             rebases or merges automatically (§4.1)",
            self.target_branch, self.expected, self.actual
        )
    }
}

/// The tree a commit would record, staged and ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StagedTree {
    /// `git write-tree` over the staged index.
    pub tree: String,
    /// Whether the index differs from `HEAD` — i.e. whether committing would
    /// record anything.
    pub changes_staged: bool,
}

/// Stage everything in the workspace and report the tree a commit would record.
///
/// **Always answers, even when there is nothing to commit.** That is not an
/// oversight, it is the fix for a real bug this function's first shape had:
/// returning `None` for a clean index made the tree unknowable *after* the
/// commit had been made, so a restart in the window between the commit and its
/// receipt could not name the effect it had to resolve — and the run got as far
/// as "no human needed" while being unable to progress. `changes_staged` is the
/// separate question, and the caller decides what to do with each answer.
///
/// **Separate from [`commit_staged`] on purpose.** §4.7's ledger has to write an
/// `INTENDED` row describing the effect *before* performing it, and the
/// description is "a commit with this tree and this message". The tree is
/// knowable before the commit — `write-tree` is a pure function of the index —
/// while the commit's sha is not, because it depends on a timestamp a restart
/// cannot reproduce. Staging first is what makes the precondition writable.
pub fn stage_all(workspace: &Path, branch: &str) -> GitResult<StagedTree> {
    let current = run_git_ok(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])?.stdout_trimmed();
    if current != branch {
        return Err(GitError::Domain(format!(
            "the workspace is on {current}, not on the run branch {branch}; Conductor \
             will not commit somebody else's checkout"
        )));
    }

    run_git_ok(workspace, &["add", "-A"])?;

    // `--cached`: the question is whether the *index* differs from HEAD, which
    // is exactly what the commit would record. Exit 1 means "there are
    // differences", which is the ordinary case, so this cannot use `run_git_ok`.
    let diff = run_git(workspace, &["diff", "--cached", "--quiet"])?;
    let changes_staged = match diff.code {
        Some(0) => false,
        Some(1) => true,
        other => {
            return Err(GitError::Domain(format!(
                "git diff --cached answered {other:?}, which is neither 'clean' nor \
                 'has changes'; refusing to guess"
            )));
        }
    };

    let tree = run_git_ok(workspace, &["write-tree"])?.stdout_trimmed();
    Ok(StagedTree {
        tree,
        changes_staged,
    })
}

/// Commit what [`stage_all`] staged, with §3.4's trailers.
///
/// `--no-verify` and an explicit identity: §4.1 already points the clone's
/// `core.hooksPath` at `/dev/null`, and this states the same rule where it is
/// relied on — a repository hook is the agent's code, and Conductor's own commit
/// must not run it.
pub fn commit_staged(
    workspace: &Path,
    subject: &str,
    trailers: &Trailers,
) -> GitResult<MadeCommit> {
    let message = if trailers.lines().is_empty() {
        format!("{subject}\n")
    } else {
        // A blank line before the trailer block, or git does not parse it as
        // trailers at all — and §3.5's recovery reads them with
        // `--format=%(trailers)`.
        format!("{subject}\n\n{}\n", trailers.render())
    };

    run_git_ok(
        workspace,
        &[
            "-c",
            "user.name=Conductor",
            "-c",
            "user.email=conductor@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--no-verify",
            "--quiet",
            "-m",
            &message,
        ],
    )?;

    let sha = run_git_ok(workspace, &["rev-parse", "HEAD"])?.stdout_trimmed();
    let tree = run_git_ok(workspace, &["rev-parse", "HEAD^{tree}"])?.stdout_trimmed();
    Ok(MadeCommit { sha, tree })
}

/// Stage and commit in one step.
///
/// `Ok(None)` when there is nothing to commit — not an error and not an empty
/// commit: an empty Conductor-owned commit would move the run branch while
/// recording nothing, and the precondition re-check would then be answering
/// about a commit that meant nothing.
pub fn commit_workspace(
    workspace: &Path,
    branch: &str,
    subject: &str,
    trailers: &Trailers,
) -> GitResult<Option<MadeCommit>> {
    if !stage_all(workspace, branch)?.changes_staged {
        return Ok(None);
    }
    Ok(Some(commit_staged(workspace, subject, trailers)?))
}

/// §4.7: "does a commit with this tree and message exist on the run branch?"
///
/// Returns the commit's sha, because the caller needs it: after a crash between
/// the commit and its receipt, the sha of the commit that *was* made is the only
/// way to describe the fetch that follows.
///
/// Walks the branch rather than only looking at its tip: a later attempt of the
/// same run may have committed on top, and the effect this asks about still
/// happened.
pub fn find_commit(
    workspace: &Path,
    branch: &str,
    tree: &str,
    message_marker: &str,
) -> GitResult<Option<String>> {
    // `%H` the commit, `%T` the tree, `%B` the raw body. Field-separated by
    // `\x1f` and NUL-delimited between records, so a message containing newlines
    // — which every trailered message does — cannot be mistaken for a boundary.
    let out = run_git(
        workspace,
        &["log", "--format=%H%x1f%T%x1f%B%x00", "--no-color", branch],
    )?;
    if !out.ok() {
        // A branch that does not exist has no commits on it. That is a decisive
        // "no", not an error: it is precisely the state after a crash between
        // intent and effect.
        return Ok(None);
    }
    let text = out.stdout_lossy();
    for record in text.split('\0') {
        let mut fields = record.trim_start_matches('\n').splitn(3, '\x1f');
        let (Some(sha), Some(record_tree), Some(body)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if record_tree.trim() == tree && body.contains(message_marker) {
            return Ok(Some(sha.trim().to_string()));
        }
    }
    Ok(None)
}

/// Whether [`find_commit`] finds anything.
pub fn commit_exists(
    workspace: &Path,
    branch: &str,
    tree: &str,
    message_marker: &str,
) -> GitResult<bool> {
    Ok(find_commit(workspace, branch, tree, message_marker)?.is_some())
}

/// §4.1's integration: fetch the run branch **from** the clone **into** the
/// source repository.
///
/// One ref is created or moved: `refs/heads/<branch>` in the source repo. The
/// target branch is not touched, the working tree is not touched, and nothing is
/// merged or rebased.
pub fn fetch_run_branch(
    source_repo: &Path,
    workspace: &Path,
    branch: &str,
) -> GitResult<FetchedRef> {
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    // `--no-tags`: the run branch is what is being integrated, and dragging the
    // clone's tags into the operator's repository is an effect nobody asked for.
    run_git_ok(
        source_repo,
        &[
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            &workspace.to_string_lossy(),
            &refspec,
        ],
    )?;

    let reference = format!("refs/heads/{branch}");
    let sha = ref_sha(source_repo, &reference)?.ok_or_else(|| {
        GitError::Domain(format!(
            "{reference} does not exist after a fetch that reported success"
        ))
    })?;
    Ok(FetchedRef { reference, sha })
}

/// §4.7: "does the target ref point at the expected sha?"
///
/// `None` when the ref does not exist, which is a decisive "the fetch has not
/// happened" rather than an error.
pub fn ref_sha(repo: &Path, reference: &str) -> GitResult<Option<String>> {
    let out = run_git(repo, &["rev-parse", "--verify", "--quiet", reference])?;
    if !out.ok() {
        return Ok(None);
    }
    let sha = out.stdout_trimmed();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha))
    }
}

/// Has the branch this run will integrate into moved since the run started?
///
/// `Ok(None)` means it is exactly where the run left it. `Ok(Some(_))` is
/// acceptance row 16 — the run goes to `AWAITING_REVIEW` with the divergence
/// attached, and Conductor rebases nothing.
///
/// A missing target branch is an **error**, not "no divergence": answering "no"
/// for a ref nobody has would let a run integrate into a branch that does not
/// exist.
pub fn target_divergence(
    source_repo: &Path,
    target_branch: &str,
    base_commit: &str,
) -> GitResult<Option<Divergence>> {
    let reference = format!("refs/heads/{target_branch}");
    let actual = ref_sha(source_repo, &reference)?.ok_or_else(|| {
        GitError::Domain(format!(
            "the run's target branch {target_branch} does not exist in {}",
            source_repo.display()
        ))
    })?;
    if actual == base_commit {
        Ok(None)
    } else {
        Ok(Some(Divergence {
            target_branch: target_branch.to_string(),
            expected: base_commit.to_string(),
            actual,
        }))
    }
}
