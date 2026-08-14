//! Conductor-owned git effects, through §4.7's side-effect ledger.
//!
//! ```text
//! operation_id = blake3(kind ‖ run_id ‖ attempt_ordinal ‖ tree_hash)
//!
//! BEGIN IMMEDIATE; INSERT side_effect(operation_id, kind, 'INTENDED', precondition); COMMIT;
//!     perform the effect                                    ← crash window
//! BEGIN IMMEDIATE; UPDATE side_effect SET state='CONFIRMED', receipt=?; COMMIT;
//! ```
//!
//! # The one rule this module exists to hold
//!
//! On restart an `INTENDED` row is resolved by **re-checking the precondition
//! against the world, never by blind retry**. That is not an optimisation: a
//! blind retry after a crash between the effect and its receipt produces a
//! second commit and a second ref update, which is exactly the duplicate
//! acceptance row 22 forbids. So [`integrate`] never performs an effect without
//! first asking [`check_precondition`] whether it has already happened — the
//! same question `recovery.rs` asks, through the same function, so a restart and
//! a re-entry cannot disagree.
//!
//! # Why the answer has three values
//!
//! S3's `precondition_holds` returned a `bool`, and `recovery.rs` recovered the
//! third value with a heuristic — "the target path exists but does not match ⇒
//! `AMBIGUOUS`". That heuristic is right for a file and wrong for a commit: a
//! workspace directory exists whether or not the commit inside it was made, so a
//! run that crashed before committing would have been declared ambiguous and
//! stopped for a human. [`PreconditionAnswer`] makes the third value explicit and
//! lets each precondition decide for itself which observations are decisive.
//!
//! # Integration is one-directional (§4.1)
//!
//! The agent "never holds a handle" on the real repository. Conductor fetches
//! the run branch **from** the clone **into** the source repo, creates one ref,
//! and merges nothing. If the target ref moved while the run was in flight, the
//! run goes to `AWAITING_REVIEW` with the divergence attached and **no effect is
//! attempted at all** — not even an intent, so the human is looking at a
//! repository Conductor has not touched.

use std::path::Path;

use conductor_core::effect::{OperationId, Precondition, SideEffectKind, SideEffectState};
use conductor_core::{Fence, RunId};
use conductor_git::integrate::{Divergence, FetchedRef, MadeCommit, Trailers};
use conductor_store::Store;

use crate::worker::WorkerError;

/// Whether an effect has already happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreconditionAnswer {
    /// The world says the effect happened. Confirm it; do **not** repeat it.
    Held,
    /// The world says decisively that it did not happen.
    NotHeld,
    /// The world will not say. §4.7: mark `AMBIGUOUS`, halt, raise a finding,
    /// require a human. **Never guess.**
    Indeterminate(String),
}

/// Ask the world whether an effect already happened — §4.7's table.
///
/// | kind | question |
/// |---|---|
/// | `artifact.write` | does the file exist with the expected content hash? |
/// | `workspace.create` | does the path exist with the expected `HEAD`? |
/// | `git.commit.local` | does a commit with this tree and message exist on the run branch? |
/// | `git.fetch_into_main` | does the target ref point at the expected sha? |
pub fn check_precondition(precondition: &Precondition) -> PreconditionAnswer {
    match precondition {
        Precondition::FileWithHash {
            path,
            content_hash: expected,
        } => match std::fs::read(path) {
            Ok(bytes) => {
                if &conductor_core::effect::content_hash(&bytes) == expected {
                    PreconditionAnswer::Held
                } else {
                    // The file is there and is not what Conductor meant to
                    // write. Somebody wrote something Conductor cannot account
                    // for; overwriting it would destroy evidence.
                    PreconditionAnswer::Indeterminate(format!(
                        "{path} exists but its content hash is not {expected}"
                    ))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                PreconditionAnswer::NotHeld
            }
            Err(error) => PreconditionAnswer::Indeterminate(format!("cannot read {path}: {error}")),
        },

        Precondition::WorkspaceAtHead { path, head } => {
            let workspace = Path::new(path);
            if !workspace.exists() {
                return PreconditionAnswer::NotHeld;
            }
            match conductor_git::run_git(workspace, &["rev-parse", "HEAD"]) {
                Ok(output) if output.ok() && output.stdout_trimmed() == *head => {
                    PreconditionAnswer::Held
                }
                Ok(_) => PreconditionAnswer::Indeterminate(format!(
                    "{path} exists but its HEAD is not {head}"
                )),
                Err(error) => {
                    PreconditionAnswer::Indeterminate(format!("cannot read {path}: {error}"))
                }
            }
        }

        Precondition::CommitOnBranch {
            path,
            branch,
            tree,
            message_marker,
        } => {
            let repo = Path::new(path);
            // A repository that is not there cannot answer. Unlike a file, whose
            // absence *is* the answer, an unreadable repository says nothing
            // about whether the commit was made — and §4.7 forbids guessing.
            if !repo.exists() {
                return PreconditionAnswer::Indeterminate(format!(
                    "the workspace {path} is gone, so whether the commit was made \
                     cannot be established"
                ));
            }
            match conductor_git::integrate::find_commit(repo, branch, tree, message_marker) {
                // Found: the effect happened, whatever the ledger says.
                Ok(Some(_)) => PreconditionAnswer::Held,
                // The repository is readable and the branch does not carry such
                // a commit. That is decisive.
                Ok(None) => PreconditionAnswer::NotHeld,
                Err(error) => PreconditionAnswer::Indeterminate(format!(
                    "cannot read the run branch in {path}: {error}"
                )),
            }
        }

        Precondition::RefAtSha {
            path,
            reference,
            sha,
        } => {
            let repo = Path::new(path);
            if !repo.exists() {
                return PreconditionAnswer::Indeterminate(format!(
                    "the repository {path} is gone, so the ref cannot be read"
                ));
            }
            match conductor_git::integrate::ref_sha(repo, reference) {
                Ok(Some(found)) if found == *sha => PreconditionAnswer::Held,
                // The ref exists and points somewhere Conductor did not put it.
                // Not "the fetch has not happened" — a fact Conductor cannot
                // account for.
                Ok(Some(found)) => PreconditionAnswer::Indeterminate(format!(
                    "{reference} points at {found}, not at {sha}"
                )),
                Ok(None) => PreconditionAnswer::NotHeld,
                Err(error) => PreconditionAnswer::Indeterminate(format!(
                    "cannot read {reference} in {path}: {error}"
                )),
            }
        }
    }
}

/// The places a crash during integration changes what recovery must do.
///
/// Six, not four. The slice's failure injection asks for a kill "between intent
/// and effect, and between effect and confirm, for both commit and fetch" —
/// which is four. The two boundary points are here as well, because the gap
/// *before* the first intent and the gap *between the two effects* are places a
/// crash leaves the ledger and the world in different states, and a kill matrix
/// that skips a gap is the S3 mistake repeated (its twelve prescribed points
/// missed the one gap that held a real bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntegrationPoint {
    /// The tree is staged; **nothing durable knows an effect is coming.**
    BeforeCommitIntent,
    /// `side_effect` says `INTENDED`; the commit does not exist.
    AfterCommitIntended,
    /// The commit exists; the receipt is not recorded.
    AfterCommitCreated,
    /// The commit's receipt is durable; the fetch is not intended.
    AfterCommitConfirmed,
    /// `side_effect` says `INTENDED`; the ref has not moved.
    AfterFetchIntended,
    /// The ref has moved; the receipt is not recorded.
    AfterFetchPerformed,
}

impl IntegrationPoint {
    /// Every point, in the order integration reaches them.
    pub const ALL: &'static [IntegrationPoint] = &[
        IntegrationPoint::BeforeCommitIntent,
        IntegrationPoint::AfterCommitIntended,
        IntegrationPoint::AfterCommitCreated,
        IntegrationPoint::AfterCommitConfirmed,
        IntegrationPoint::AfterFetchIntended,
        IntegrationPoint::AfterFetchPerformed,
    ];

    /// The name used on the command line and in reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            IntegrationPoint::BeforeCommitIntent => "before-commit-intent",
            IntegrationPoint::AfterCommitIntended => "after-commit-intended",
            IntegrationPoint::AfterCommitCreated => "after-commit-created",
            IntegrationPoint::AfterCommitConfirmed => "after-commit-confirmed",
            IntegrationPoint::AfterFetchIntended => "after-fetch-intended",
            IntegrationPoint::AfterFetchPerformed => "after-fetch-performed",
        }
    }

    /// Parse a point by name.
    pub fn parse(name: &str) -> Option<IntegrationPoint> {
        IntegrationPoint::ALL
            .iter()
            .copied()
            .find(|p| p.as_str() == name)
    }
}

/// Notified as integration passes each [`IntegrationPoint`].
pub trait IntegrationObserver {
    /// Integration has reached `point`.
    fn at(&mut self, point: IntegrationPoint);
}

/// The production observer: integration is not observed at all.
impl IntegrationObserver for () {
    fn at(&mut self, _point: IntegrationPoint) {}
}

/// What integration needs to know.
#[derive(Debug, Clone)]
pub struct IntegrationConfig {
    /// The operator's repository — the fetch destination.
    pub source_repo: std::path::PathBuf,
    /// The run clone — the fetch source.
    pub workspace: std::path::PathBuf,
    /// §4.1's `conductor/<task-id>/<run-id>`.
    pub run_branch: String,
    /// The branch the run integrates into.
    pub target_branch: String,
    /// What the target branch pointed at when the run started.
    pub base_commit: String,
    /// `attempt.ordinal` — part of the operation identity.
    pub attempt_ordinal: i64,
    /// The working-tree hash the verification was bound to — the rest of the
    /// operation identity, and what makes the effect specific to this content.
    pub tree_hash: String,
    /// The commit subject.
    pub subject: String,
    /// §3.4's trailers, as far as they have a real source (see
    /// [`conductor_git::Trailers`]).
    pub trailers: Trailers,
}

/// What integration did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integration {
    /// The target ref moved. Nothing was attempted (acceptance row 16).
    TargetMoved(Divergence),
    /// The attempt changed nothing, so there was nothing to record.
    NothingToCommit,
    /// The run branch carries the work, and the source repository has the ref.
    Integrated {
        /// The commit (made now, or found already made).
        commit: MadeCommit,
        /// The ref update.
        fetched: FetchedRef,
    },
}

/// Commit the workspace and fetch the run branch into the source repository,
/// both through §4.7's ledger.
pub fn integrate(
    store: &mut Store,
    fence: &Fence,
    config: &IntegrationConfig,
    observer: &mut dyn IntegrationObserver,
) -> Result<Integration, WorkerError> {
    // §4.1, row 16. **Before** anything durable is written: a run that will not
    // integrate must leave the repository exactly as the human left it.
    if let Some(divergence) = conductor_git::integrate::target_divergence(
        &config.source_repo,
        &config.target_branch,
        &config.base_commit,
    )? {
        return Ok(Integration::TargetMoved(divergence));
    }

    // Staging is idempotent and produces the tree the commit *would* record, so
    // it is what the precondition can be written about before the commit exists.
    let staged = conductor_git::integrate::stage_all(&config.workspace, &config.run_branch)?;
    let marker = run_marker(fence.run_id(), &config.trailers);

    // **"Nothing staged" is not "nothing to integrate."** Once the commit has
    // been made the index matches `HEAD` again, so a re-entry after a crash in
    // the window between the commit and its receipt sees a clean index — and
    // returning early there would leave the ledger row `INTENDED` for ever and
    // the fetch never done. The run would then be somewhere no human is needed
    // and no progress is possible, which is precisely the failure S3 recorded at
    // its thirteenth kill point.
    //
    // So the world is asked first. Only when the commit is *also* absent does a
    // clean index mean there was never anything to do — and then no ledger row
    // is opened at all, because an effect that will not be attempted must not
    // leave an intent behind.
    let already_committed = conductor_git::integrate::find_commit(
        &config.workspace,
        &config.run_branch,
        &staged.tree,
        &marker,
    )?
    .is_some();
    if !already_committed && !staged.changes_staged {
        return Ok(Integration::NothingToCommit);
    }

    let commit = commit_effect(store, fence, config, &staged.tree, &marker, observer)?;
    let fetched = fetch_effect(store, fence, config, &commit, observer)?;
    Ok(Integration::Integrated { commit, fetched })
}

/// `git.commit.local`, through intent → precondition → act → receipt.
fn commit_effect(
    store: &mut Store,
    fence: &Fence,
    config: &IntegrationConfig,
    tree: &str,
    marker: &str,
    observer: &mut dyn IntegrationObserver,
) -> Result<MadeCommit, WorkerError> {
    let operation_id = OperationId::compute(
        SideEffectKind::GitCommitLocal,
        fence.run_id(),
        config.attempt_ordinal,
        &config.tree_hash,
    );
    let precondition = Precondition::CommitOnBranch {
        path: config.workspace.display().to_string(),
        branch: config.run_branch.clone(),
        tree: tree.to_string(),
        message_marker: marker.to_string(),
    };

    observer.at(IntegrationPoint::BeforeCommitIntent);
    let state = store.intend_effect(
        fence,
        &operation_id,
        SideEffectKind::GitCommitLocal,
        &precondition,
        now_ms(),
    )?;
    observer.at(IntegrationPoint::AfterCommitIntended);

    if state == SideEffectState::Ambiguous {
        return Err(WorkerError::AmbiguousEffect {
            operation_id: operation_id.to_string(),
            detail: "a previous pass left this commit undecided".to_string(),
        });
    }

    // Ask the world, always — including when the ledger already says
    // `CONFIRMED`, because the receipt is a record of a past belief and the
    // repository is the fact. The two disagreeing is itself information.
    let sha = match check_precondition(&precondition) {
        PreconditionAnswer::Held => conductor_git::integrate::find_commit(
            &config.workspace,
            &config.run_branch,
            tree,
            marker,
        )?
        .ok_or_else(|| WorkerError::AmbiguousEffect {
            operation_id: operation_id.to_string(),
            detail: "the precondition held and then the commit could not be found".to_string(),
        })?,
        PreconditionAnswer::Indeterminate(detail) => {
            store.mark_effect_ambiguous(fence, &operation_id, &detail, now_ms())?;
            return Err(WorkerError::AmbiguousEffect {
                operation_id: operation_id.to_string(),
                detail,
            });
        }
        PreconditionAnswer::NotHeld => {
            if state == SideEffectState::Confirmed {
                // The ledger says it happened and the repository says it did
                // not. Re-running would be a blind retry against a world nobody
                // understands.
                let detail = format!(
                    "the ledger records this commit as CONFIRMED, but no commit with \
                     tree {tree} and marker {marker:?} is on {}",
                    config.run_branch
                );
                store.mark_effect_ambiguous(fence, &operation_id, &detail, now_ms())?;
                return Err(WorkerError::AmbiguousEffect {
                    operation_id: operation_id.to_string(),
                    detail,
                });
            }
            let made = conductor_git::integrate::commit_staged(
                &config.workspace,
                &config.subject,
                &config.trailers,
            )?;
            made.sha
        }
    };

    observer.at(IntegrationPoint::AfterCommitCreated);
    store.confirm_effect(
        fence,
        &operation_id,
        &format!("{{\"commit\":{sha:?},\"tree\":{tree:?}}}"),
        now_ms(),
    )?;
    observer.at(IntegrationPoint::AfterCommitConfirmed);

    Ok(MadeCommit {
        sha,
        tree: tree.to_string(),
    })
}

/// `git.fetch_into_main`, through intent → precondition → act → receipt.
fn fetch_effect(
    store: &mut Store,
    fence: &Fence,
    config: &IntegrationConfig,
    commit: &MadeCommit,
    observer: &mut dyn IntegrationObserver,
) -> Result<FetchedRef, WorkerError> {
    let operation_id = OperationId::compute(
        SideEffectKind::GitFetchIntoMain,
        fence.run_id(),
        config.attempt_ordinal,
        &config.tree_hash,
    );
    let reference = format!("refs/heads/{}", config.run_branch);
    let precondition = Precondition::RefAtSha {
        path: config.source_repo.display().to_string(),
        reference: reference.clone(),
        sha: commit.sha.clone(),
    };

    let state = store.intend_effect(
        fence,
        &operation_id,
        SideEffectKind::GitFetchIntoMain,
        &precondition,
        now_ms(),
    )?;
    observer.at(IntegrationPoint::AfterFetchIntended);

    if state == SideEffectState::Ambiguous {
        return Err(WorkerError::AmbiguousEffect {
            operation_id: operation_id.to_string(),
            detail: "a previous pass left this fetch undecided".to_string(),
        });
    }

    match check_precondition(&precondition) {
        PreconditionAnswer::Held => {}
        PreconditionAnswer::Indeterminate(detail) => {
            store.mark_effect_ambiguous(fence, &operation_id, &detail, now_ms())?;
            return Err(WorkerError::AmbiguousEffect {
                operation_id: operation_id.to_string(),
                detail,
            });
        }
        PreconditionAnswer::NotHeld => {
            match conductor_git::integrate::fetch_run_branch(
                &config.source_repo,
                &config.workspace,
                &config.run_branch,
            ) {
                Ok(_) => {}
                Err(error) => {
                    // The fetch ran and refused. That is *known* not to have
                    // happened, which is `FAILED` rather than `AMBIGUOUS`: the
                    // ref did not move, and a human is told why.
                    store.fail_effect(fence, &operation_id, &error.to_string(), now_ms())?;
                    return Err(error.into());
                }
            }
        }
    }

    observer.at(IntegrationPoint::AfterFetchPerformed);
    store.confirm_effect(
        fence,
        &operation_id,
        &format!("{{\"ref\":{reference:?},\"sha\":{:?}}}", commit.sha),
        now_ms(),
    )?;

    Ok(FetchedRef {
        reference,
        sha: commit.sha.clone(),
    })
}

/// The line a commit must contain for it to be *this* run's.
///
/// §3.4's `Conductor-Run` trailer. Taken from the trailers when present rather
/// than formatted independently, so the marker the precondition looks for and
/// the marker the commit carries cannot drift apart.
fn run_marker(run_id: &RunId, trailers: &Trailers) -> String {
    match trailers.get("Conductor-Run") {
        Some(value) => format!("Conductor-Run: {value}"),
        None => format!("Conductor-Run: {run_id}"),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
