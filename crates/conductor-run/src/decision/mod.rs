//! `.conductor/decisions/D-*.md`, read from a repository and synced into
//! §5.1's `decision` table — master plan §3.1, §3.6, §5.1 (slice S11 task 5).
//!
//! # What this module is, next to `plan`
//!
//! [`model`] is the git half: parsing one Markdown-with-frontmatter file into
//! a [`model::Decision`], pure over text, the way `plan::model` is the git
//! half of a plan. [`load_all`] and [`register_decisions`] here are the
//! bridge — the git half and `conductor_store::ledger`'s `decision` table
//! meeting, the same role `plan::ledger` plays for plan versions, reasoned the
//! same way: every store-touching function here takes a
//! [`ProjectId`](conductor_core::ProjectId) and reads `root_path` back out of
//! the store, never a caller-supplied path. §3.3 control 2 — *"Conductor
//! reads plan approval only from the registered repository's working tree,
//! never from a run branch"* — was written about plans, but the reasoning is
//! general: a run workspace has nowhere to put a path this module never
//! accepts as a parameter.
//!
//! # `load_all` is pure; `register_decisions` is the only writer
//!
//! [`load_all`] reads a directory, parses every `D-*.md` file it finds, sorts
//! by id, and refuses two files that declare the same id — naming both paths,
//! because "which one is really `D-0007`?" has no answer a reader can supply
//! without both. It never touches a [`Store`].
//!
//! [`register_decisions`] is the write path: it calls [`load_all`], then
//! upserts every decision found (`conductor_store::ledger::upsert_decision` —
//! insert `OPEN`, or refresh content facts on a resync, never touching
//! `status`), then applies exactly one transition per declared `supersedes:`
//! — `→ SUPERSEDED` on the *target*, via `set_decision_status`. That is the
//! **only** store mutation this module makes beyond the initial insert. See
//! [`model`]'s "`status` is validated, not obeyed" for why a decision's own
//! declared `status` never drives a transition on itself: `ACCEPTED` and
//! `REJECTED` are a human's call, made through some mechanism this task does
//! not build (for a future task to wire up), not a file edit `.conductor/`'s
//! own write access would make free.
//!
//! # Three passes, so one bad file cannot half-register a batch and no
//! sibling reference can outrun its own foreign key
//!
//! Every declared `supersedes:` is checked against the union of "every id in
//! this batch" and "every id the store already knows for this project"
//! **before** a single row is written. A reference to nothing is refused,
//! naming both ends — the decision's own id and the target it named — and
//! nothing from the batch is persisted.
//!
//! Writing the batch is then two passes rather than one, because
//! `decision.supersedes` carries its own foreign key onto `decision(id)`: a
//! decision that supersedes a *sibling from this same batch* — one that
//! happens to sort after it by id — cannot be upserted with its real
//! `supersedes` value until that sibling's row exists. So every decision is
//! first upserted with `supersedes` cleared (bringing every row into
//! existence, regardless of order), then upserted again with its real value —
//! safe by then, because the whole batch now exists as rows. A decision
//! naming a sibling from the very same sync therefore resolves in either id
//! order.
//!
//! Superseding itself — the store transition — is a final pass, applied only
//! after every row in the batch carries its real `supersedes` value.
//!
//! # Superseding is idempotent
//!
//! `SUPERSEDED` is terminal in the store's own machine (Ruling 10;
//! `conductor_store::ledger::decision_status_successors`), so calling
//! `set_decision_status(target, SUPERSEDED)` a second time refuses. A re-run
//! of [`register_decisions`] — §3.5's recovery path re-syncs `.conductor/`
//! after total local loss — must not fail on a target it already superseded
//! last time, so the second pass skips a target that is already `SUPERSEDED`
//! rather than calling the transition again.
//!
//! # What this module does not do
//!
//! * **Drive `ACCEPTED`/`REJECTED`.** See above.
//! * **The `.conductor/**` rejection rule (§3.3's control 1).** Needs a run
//!   branch and a reconciler.
//! * **Render a decision into a review packet.** S12.

pub mod model;

pub use model::{DECISION_HASH_DOMAIN, Decision, DecisionError, parse};

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use conductor_core::ProjectId;
use conductor_store::{DecisionRow, DecisionStatus, NewDecision, Store};

/// Where a project's decisions live, relative to the repository root — §3.1.
pub const DECISIONS_DIR: &str = ".conductor/decisions";

/// Anything that stops decisions from being read from a repository or synced
/// into the store.
///
/// Every variant is a refusal, and every variant names its subject — the same
/// discipline `plan::ledger::LedgerError` states: *"a ledger error is read by
/// someone deciding whether their repository has been tampered with."*
#[derive(Debug, thiserror::Error)]
pub enum DecisionSyncError {
    /// A file, or the decisions directory itself, could not be read.
    /// **Absence of the directory** is not this — see [`load_all`].
    #[error("{path}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// One file's contents did not parse as a decision.
    #[error("{path}: {source}")]
    Decision {
        /// The file.
        path: PathBuf,
        /// Why.
        source: DecisionError,
    },
    /// Two files in the same directory declare the same `id`.
    #[error(
        "decision {id} is declared twice, at {first} and {second}; a stable \
         id is assigned once and never reused (§3.6), so two files cannot \
         both be it"
    )]
    DuplicateId {
        /// The id both files claim.
        id: String,
        /// The first file found with it.
        first: String,
        /// The second.
        second: String,
    },
    /// `supersedes:` names an id nothing in the batch or the store has.
    #[error(
        "decision {id} supersedes {target}, but {target} is not a decision \
         this project knows about — neither in this sync nor already in the \
         store; a supersedes chain must resolve at both ends"
    )]
    UnknownSupersedes {
        /// The decision declaring `supersedes:`.
        id: String,
        /// What it named.
        target: String,
    },
    /// The store said no.
    #[error(transparent)]
    Store(#[from] conductor_store::StoreError),
    /// Nothing is registered under this project id.
    #[error(
        "no project {id} is registered; decisions are synced against a \
         registered project's root, and there is none to read — run project \
         registration first"
    )]
    UnknownProject {
        /// The id asked for.
        id: ProjectId,
    },
}

/// Read and parse every `.conductor/decisions/D-*.md` file under a
/// repository root, in id order.
///
/// A **missing** decisions directory is not a refusal and yields an empty
/// list — a project with no decisions yet is an ordinary starting state, the
/// same way `plan::model`'s minimal plan (no milestones) is a valid plan
/// rather than a defect. Only ambiguity is refused: two files claiming one
/// id, or a file that does not parse.
pub fn load_all(repo_root: &Path) -> Result<Vec<Decision>, DecisionSyncError> {
    let dir = repo_root.join(DECISIONS_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(DecisionSyncError::Io { path: dir, source }),
    };

    let mut decisions: Vec<Decision> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| DecisionSyncError::Io {
            path: dir.clone(),
            source,
        })?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        // §3.1's own example: `D-0007-clone-not-worktree.md`.
        if !(name.starts_with("D-") && name.ends_with(".md")) {
            continue;
        }
        let path = entry.path();
        let text = std::fs::read_to_string(&path).map_err(|source| DecisionSyncError::Io {
            path: path.clone(),
            source,
        })?;
        let mut decision = model::parse(&text).map_err(|source| DecisionSyncError::Decision {
            path: path.clone(),
            source,
        })?;
        decision.source_path = format!("{DECISIONS_DIR}/{name}");

        if let Some(existing) = decisions.iter().find(|d| d.id == decision.id) {
            return Err(DecisionSyncError::DuplicateId {
                id: decision.id.clone(),
                first: existing.source_path.clone(),
                second: decision.source_path.clone(),
            });
        }
        decisions.push(decision);
    }

    decisions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(decisions)
}

/// Sync every decision under a registered project's `.conductor/decisions/`
/// into §5.1's `decision` table.
///
/// See this module's docs for the two-pass write and the idempotent
/// supersession. Returns every decision the store now has for the project —
/// including any this sync did not touch, because decisions are append-only
/// and a store row is never deleted by a later sync.
pub fn register_decisions(
    store: &mut Store,
    project_id: &ProjectId,
) -> Result<Vec<DecisionRow>, DecisionSyncError> {
    let project = store
        .project(project_id)?
        .ok_or_else(|| DecisionSyncError::UnknownProject {
            id: project_id.clone(),
        })?;
    let decisions = load_all(Path::new(&project.root_path))?;

    // Pass 1: validate every `supersedes:` resolves before writing anything.
    // A single unresolvable reference in one file must not leave the other,
    // unrelated files in the same sync half-registered.
    let mut known: BTreeSet<String> = decisions.iter().map(|d| d.id.clone()).collect();
    for existing in store.decisions_for_project(project_id)? {
        known.insert(existing.id);
    }
    for decision in &decisions {
        if let Some(target) = &decision.supersedes
            && !known.contains(target)
        {
            return Err(DecisionSyncError::UnknownSupersedes {
                id: decision.id.clone(),
                target: target.clone(),
            });
        }
    }

    // Pass 2: every row in the batch is created before any `supersedes:` is
    // written to it. `decision.supersedes` carries its own foreign key onto
    // `decision(id)`, so a decision that supersedes a *sibling from this same
    // batch* — one that sorts after it by id — cannot be upserted with its
    // real `supersedes` value until that sibling's row exists too. Each
    // decision is therefore upserted twice: once with `supersedes` cleared,
    // purely to bring the row into existence, then again with its real value
    // once every row in the batch does. `upsert_decision`'s own `ON CONFLICT`
    // always rewrites `supersedes`, so the second pass is exactly the resync
    // it already documents. A crash between the two leaves a decision's
    // `supersedes` transiently `NULL`, which the next — idempotent — sync
    // repairs; the same class of window `plan::ledger::approve` documents
    // between its own store write and its sidecar.
    for decision in &decisions {
        store.upsert_decision(&NewDecision {
            id: decision.id.clone(),
            project_id: project_id.clone(),
            supersedes: None,
            content_hash: decision.content_hash().to_string(),
            source_path: decision.source_path.clone(),
        })?;
    }
    for decision in &decisions {
        if decision.supersedes.is_some() {
            store.upsert_decision(&NewDecision {
                id: decision.id.clone(),
                project_id: project_id.clone(),
                supersedes: decision.supersedes.clone(),
                content_hash: decision.content_hash().to_string(),
                source_path: decision.source_path.clone(),
            })?;
        }
    }

    // Pass 3: apply supersession. Skipped when the target is already
    // `SUPERSEDED` — see this module's docs on idempotency.
    for decision in &decisions {
        let Some(target) = &decision.supersedes else {
            continue;
        };
        let row = store.decision(target)?.unwrap_or_else(|| {
            panic!("supersedes target {target} was validated as known in pass 1")
        });
        if row.status != DecisionStatus::Superseded {
            store.set_decision_status(target, DecisionStatus::Superseded)?;
        }
    }

    Ok(store.decisions_for_project(project_id)?)
}
