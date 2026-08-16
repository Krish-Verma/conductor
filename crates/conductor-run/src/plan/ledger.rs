//! Registering a project and its plan versions, and §3.3's approval controls —
//! master plan §3.1, §3.3, §3.6, §5.1, §5.2.
//!
//! # The two halves of §3.1, joined here
//!
//! §3.1 splits Conductor's state: git holds *"what we agreed to do, and what we
//! are allowed to do"*, SQLite holds *"what actually happened"*. Everything in
//! [`super::model`], [`super::hash`], [`super::validate`] and
//! [`super::project`] is the git half and touches no row; `conductor_store`'s
//! own `ledger` module is the SQLite half and reads no file. This module is the
//! only place the two meet, and every function here is a statement about one
//! repository *and* one database at once.
//!
//! # §3.3 is the reason this module is shaped the way it is
//!
//! > `.conductor/` lives in the repo, therefore inside the agent's workspace,
//! > therefore an agent **can write** `.conductor/plans/v3/APPROVED` in its own
//! > clone. Three controls, all required:
//! >
//! > 1. `.conductor/**` is in the always-forbidden write scope […] rejected at
//! >    reconciliation, unconditionally, with a finding.
//! > 2. Conductor reads plan approval **only** from the registered repository's
//! >    working tree, never from a run branch.
//! > 3. The store records the approval independently at grant time. If file and
//! >    store disagree, **execution halts** — it is never resynced.
//! >
//! Control 1 belongs to the reconciler and is not here — it needs a run branch.
//! Controls 2 and 3 are, and both are enforced by the shape of the API rather
//! than by callers remembering a rule:
//!
//! * **Control 2 is why no function here takes a repository path except
//!   [`register_project`].** Everything afterwards takes a [`ProjectId`] and
//!   reads `project.root_path` back out of the store. A caller holding a run
//!   workspace has nowhere to put it: there is no parameter for "which tree",
//!   so a run branch cannot be offered as one. A `repo_root` parameter would
//!   have made "never from a run branch" a convention, and §3.3 is written
//!   precisely because the agent is assumed to be trying.
//! * **Control 3 is why [`verify_approval`] takes `&Store`.** An immutable
//!   borrow cannot write a row, so "it is never resynced" is a property of the
//!   signature. The function also writes no file: it reports the disagreement,
//!   names both values, and the caller halts. Healing either side would erase
//!   the only evidence that they ever differed — which is exactly what an agent
//!   that edited an approved plan would want to happen.
//!
//! # Approval takes a witness, not a boolean
//!
//! §5.2: *"`APPROVED` only via a human at the control socket."* [`approve`]
//! therefore takes an [`Authorization`] — S8's own answer to "may this happen?"
//! — and **re-derives** everything from it: the grant row, the request behind
//! it, the request's kind and subject, and the binding recomputed from the
//! request's own material. A caller cannot approve by passing `true`, and
//! cannot approve by inventing an `Authorization::Authorized { grant_id }`
//! either, because a grant id that names no live §4.3 plan grant for *this*
//! plan version authorizes nothing.
//!
//! The approver and the policy hash written into the sidecar are read **out of
//! the grant**, not taken as parameters, for the same reason: a caller that
//! could name the approver could name someone who never approved anything.
//!
//! # What this module does not do
//!
//! * **Task materialisation.** §5.1's `task` rows from a validated plan, and
//!   acceptance row 21's rule that an in-flight run keeps its old plan version.
//!   [`super::materialize`] does that, from the [`ValidatedPlan`]
//!   [`register_plan_version`] hands back — never from a re-read of the file.
//! * **`.conductor/**` rejection at reconciliation** — §3.3's control 1.
//! * **Decisions.** §5.1's `decision` table has a store API and no reader for
//!   `.conductor/decisions/*.md` yet.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use conductor_core::{PlanVersionId, PlanVersionState, ProjectId};
use conductor_git::{run_git, run_git_ok};
use conductor_store::{NewPlanVersion, NewProject, PlanVersionRow, ProjectRow, Store};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::hash::PlanHash;
use super::model::PlanError;
use super::project::{self, ProjectError};
use super::validate::{ValidatedPlan, ValidationReport};
use super::{approved_path, content_hash, plan_path};
use crate::approval::binding::{Binding, BindingHash};
use crate::approval::store::{ApprovalError, Consumption};
use crate::approval::{self, ApprovalKind, Authorization, Subject};
use crate::policy::load::{format_rfc3339_utc, parse_rfc3339_utc};

/// Domain separator for [`repo_identity`].
///
/// §5.1's column comment is `blake3(first_commit ‖ normalized_origin)` and
/// ADR-0007 makes blake3 the only digest, so the separator is what keeps a
/// repository identity from colliding with any other blake3 value Conductor
/// stores — a plan hash, a policy hash, a binding. `approval::binding` gives
/// the same argument for `BINDING_DOMAIN`.
pub const REPO_IDENTITY_DOMAIN: &str = "conductor.project.identity.v1";

/// Anything that stops the ledger from recording, approving or verifying.
///
/// Every variant is a refusal, and every variant names its subject. A ledger
/// error is read by someone deciding whether their repository has been tampered
/// with, and "the plan is invalid" is not something anyone can act on.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// `.conductor/project.yaml` could not be read.
    #[error(transparent)]
    Project(#[from] ProjectError),
    /// A plan document could not be read or parsed.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// §3.7 refused the plan. The whole report travels, not the first defect —
    /// [`super::validate`]'s "every defect, not the first one".
    #[error("plan version v{version} does not validate:\n{report}")]
    Refused {
        /// Which version.
        version: u32,
        /// Every defect §3.7 found.
        report: Box<ValidationReport>,
    },
    /// git said no, or could not be run at all.
    #[error(transparent)]
    Git(#[from] conductor_git::GitError),
    /// The store said no.
    #[error(transparent)]
    Store(#[from] conductor_store::StoreError),
    /// The approval tables said no.
    #[error(transparent)]
    Approval(#[from] ApprovalError),
    /// An identifier read from a file or a row is blank.
    #[error(transparent)]
    Id(#[from] conductor_core::IdError),
    /// A file in the registered working tree could not be read.
    #[error("{what} at {path}: {source}")]
    Io {
        /// What was being read, for the reader who does not recognise the path.
        what: &'static str,
        /// The path in the **registered** tree (§3.3 control 2).
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The repository has no single root commit, so §5.1's `first_commit` has
    /// no answer.
    ///
    /// **Unreachable by construction, and kept anyway.** `git rev-list
    /// --max-parents=0 --first-parent HEAD` walks one parent chain to its end,
    /// so it yields exactly one root for any reachable `HEAD`; a repository
    /// with no commits fails earlier, inside `run_git_ok`. No test covers this
    /// variant because no repository can produce it — that is recorded here so
    /// its absence from the suite is not later read as an oversight. It exists
    /// so that [`repo_identity`] can refuse instead of `unwrap`ping if a future
    /// git changes what that invocation returns: an identity guessed under
    /// ambiguity silently binds a project's whole ledger to the wrong
    /// repository, which is not a failure anyone would notice in time.
    #[error(
        "the repository at {root} has {count} root commits along its first-parent \
         history ({found}); §5.1's repo_identity is blake3(first_commit ‖ \
         normalized_origin) and cannot be computed from an ambiguous first \
         commit — refusing rather than picking one"
    )]
    AmbiguousFirstCommit {
        /// The repository.
        root: PathBuf,
        /// How many were found.
        count: usize,
        /// What git returned, space-joined.
        found: String,
    },
    /// Nothing has been registered under this project id.
    #[error(
        "no project {id} is registered; §3.3 reads plan approval only from the \
         registered repository's working tree, and there is no registered tree \
         to read — run project registration first"
    )]
    UnknownProject {
        /// The id asked for.
        id: ProjectId,
    },
    /// No `plan_version` row exists for this version.
    #[error("no plan version {id} is registered for project {project}")]
    UnknownPlanVersion {
        /// The plan version id that was derived.
        id: PlanVersionId,
        /// The project it belongs to.
        project: ProjectId,
    },
    /// The version already has a row, and a plan version is immutable.
    #[error(
        "plan version {id} is already registered at {stored}; a plan version is \
         immutable once written, so re-registering it (now {offered}) is a \
         mistake upstream and not a resync — publish the change as a new version"
    )]
    AlreadyRegistered {
        /// The plan version id.
        id: PlanVersionId,
        /// The hash the row holds.
        stored: String,
        /// The hash the file now has.
        offered: String,
    },
    /// `.conductor/plans/vN/plan.yaml` declares a different `version:`.
    #[error(
        "{path} declares version {declared} but sits in the directory for version \
         {directory}; §3.4's `Conductor-Plan: v{directory}@…` trailer, §5.1's \
         UNIQUE(project_id, version) and supersession each need one answer to \
         \"which version is this?\""
    )]
    VersionMismatch {
        /// The plan file, in the registered tree.
        path: PathBuf,
        /// What the document says.
        declared: u32,
        /// What the directory says.
        directory: u32,
    },
    /// The witness does not authorize approving this plan version.
    #[error(
        "nothing authorizes approving plan version {plan_version}: {detail}; §5.2 \
         grants APPROVED \"only via a human at the control socket\", so a witness \
         that does not resolve to a live §4.3 plan approval for this exact \
         version approves nothing"
    )]
    NotAuthorized {
        /// Which version was being approved.
        plan_version: PlanVersionId,
        /// What S8 said, or what the re-derivation found.
        detail: String,
    },
    /// The row is not `APPROVED`, so there is no approval to verify.
    #[error(
        "plan version {id} is {state}, not APPROVED; there is no approval to \
         verify, and reporting one would be the self-approval §3.7's \
         clarification 4 rules out"
    )]
    NotApproved {
        /// The plan version.
        id: PlanVersionId,
        /// Where §5.2's machine actually has it.
        state: PlanVersionState,
    },
    /// §3.3 control 3: the sidecar and the store do not agree.
    #[error(
        "plan version {plan_version}: the APPROVED sidecar and the store disagree \
         about {field} — the file says {in_file}, the store says {in_store}. §3.3: \
         \"If file and store disagree, execution halts — it is never resynced.\" \
         Neither side has been changed; re-run plan approve on the document you \
         mean, or restore the one you did not"
    )]
    Disagreement {
        /// Which version.
        plan_version: PlanVersionId,
        /// Which fact they differ on.
        field: &'static str,
        /// What the sidecar says.
        in_file: String,
        /// What the store says.
        in_store: String,
    },
    /// §5.2's restart clause: the approved document is not the document on
    /// disk.
    #[error(
        "plan version {plan_version} was approved at {approved} but {path} now \
         hashes to {actual}; §5.2 makes a content-hash mismatch on an APPROVED \
         plan a hard error, cleared by re-running `conductor plan approve \
         {plan_version}` on the changed document. §3.6 excludes formatting and \
         comments from the hash, so this is a change of content, not of layout"
    )]
    Edited {
        /// Which version.
        plan_version: PlanVersionId,
        /// The plan file, in the registered tree.
        path: PathBuf,
        /// The hash the approval was granted over.
        approved: String,
        /// The hash the file has now.
        actual: String,
    },
    /// The `APPROVED` sidecar is not a sidecar.
    #[error("the APPROVED sidecar at {path} cannot be read: {detail}")]
    UnreadableSidecar {
        /// The sidecar, in the registered tree.
        path: PathBuf,
        /// What is wrong with it.
        detail: String,
    },
    /// The row says `APPROVED` and records no approver or no timestamp.
    ///
    /// The crash window [`approve`] documents: the state moved and the approval
    /// content was never written. Refused rather than defaulted, because a
    /// missing approver read as `""` and a missing timestamp read as
    /// `1970-01-01T00:00:00Z` would let a sidecar claiming those values
    /// *verify* against a store that records nothing.
    #[error(
        "plan version {id} is APPROVED but the store records no approver or no \
         timestamp for it; that is a half-written approval, not an approval — \
         re-run `conductor plan approve {id}` to complete it"
    )]
    IncompleteApproval {
        /// The plan version.
        id: PlanVersionId,
    },
}

/// What [`register_plan_version`] recorded — §5.1's row, and the document it
/// was recorded from.
///
/// Two values rather than one because the two halves of §3.1 both matter to the
/// caller and neither can be derived from the other: `row.content_hash` is what
/// an approval will be granted over, and `plan` is what task materialisation
/// reads. Handing back only the row would make the materializer re-read the
/// file, and a file re-read is a file that may have changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredPlanVersion {
    /// The `plan_version` row, in `VALIDATED`.
    pub row: PlanVersionRow,
    /// The document that row's `content_hash` was taken over, after §3.7.
    pub plan: ValidatedPlan,
}

/// One plan version's approval, as the store and the sidecar both record it.
///
/// The return value of both [`approve`] and [`verify_approval`], and equal
/// across the two by construction: that is what "the store records the approval
/// independently" means when the two halves agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// `plan_version.id`.
    pub plan_version_id: PlanVersionId,
    /// `N` from `.conductor/plans/vN/`.
    pub version: u32,
    /// The content hash the approval was granted over.
    pub content_hash: PlanHash,
    /// Who granted it — `approval_grant.granted_by`, never a caller's claim.
    pub approver: String,
    /// When, in epoch milliseconds — §5.1's `plan_version.approved_at`.
    pub approved_at_ms: i64,
    /// The policy snapshot the human was shown, from the approval request.
    pub policy_hash: String,
}

/// `.conductor/plans/vN/APPROVED`, as written and read.
///
/// §3.1 names four contents — *"plan content hash · approver · timestamp ·
/// policy hash"*. `plan_version` and `version` are carried as well, so that a
/// sidecar copied from one version's directory into another's is detected
/// rather than silently accepted; §3.3 assumes an agent with write access to
/// exactly this directory.
///
/// **The timestamp is RFC 3339 UTC**, rendered by
/// [`format_rfc3339_utc`] — the inverse of the parser §4.4's config
/// timestamps already go through. §4.3 writes its own approval artifacts the
/// same way (`requested_at: 2026-08-12T14:03:00Z`), and this file is the same
/// category of thing: a **committed** artifact that §3.2 keeps in git so it is
/// *"reviewable as a diff, in a PR, by a human"* and *"readable without
/// Conductor installed"*. Epoch milliseconds satisfy the letter of "readable"
/// and defeat its purpose.
///
/// §5.1's `plan_version.approved_at` stays an `INTEGER` — only the file
/// changes. [`verify_approval`] therefore parses this field back to an instant
/// and compares **instants, not text**, so that a sidecar re-serialized with
/// `.000` on the seconds is not read as a disagreement about when a human
/// approved something. A field that cannot be parsed at all is a refusal, not
/// a zero: `1970-01-01T00:00:00Z` is a real instant and must never be what a
/// corrupt file silently means.
#[derive(Debug, Serialize, Deserialize)]
struct Sidecar {
    plan_version: String,
    version: u32,
    content_hash: String,
    approver: String,
    approved_at: String,
    policy_hash: String,
}

/// The sidecar's file shape — one `approved:` key, matching `plan:`,
/// `policy:`, `verification:` and `project:`.
#[derive(Debug, Serialize, Deserialize)]
struct ApprovedDocument {
    approved: Sidecar,
}

/// `plan_version.id` for one version of one project.
///
/// Derived rather than allocated, and that is load-bearing rather than tidy:
/// §4.3's [`Subject::PlanVersion`] carries a `plan_version_id`, so the control
/// socket has to be able to name the row a human is being asked about
/// *before* — and independently of — anything this module does. A generated id
/// would mean the approval request and the ledger could name two different
/// rows for one version.
///
/// The `expect` cannot fire: [`ProjectId`] refuses a blank value at
/// construction, so this string always has a non-blank prefix.
pub fn plan_version_id(project: &ProjectId, version: u32) -> PlanVersionId {
    PlanVersionId::new(format!("{project}-v{version}"))
        .expect("a project id is never blank, so neither is a plan version id built from one")
}

/// `blake3(first_commit ‖ normalized_origin)` — §5.1's `project.repo_identity`.
///
/// # Why the first commit is taken along the first-parent history
///
/// `git rev-list --max-parents=0 HEAD` returns *every* root commit, and a
/// repository can legitimately have several — merging an unrelated history in
/// adds one. Taking "all of them" or "the oldest of them" would mean a
/// repository's identity changed on the day someone merged a subtree, which is
/// the one thing an identity must not do. `--first-parent` follows the line the
/// repository was actually built along, so it yields exactly one root and keeps
/// yielding the same one afterwards.
///
/// If it somehow does not yield exactly one, this refuses
/// ([`LedgerError::AmbiguousFirstCommit`]) rather than picking. An identity
/// guessed under ambiguity is an identity that silently binds a project's whole
/// ledger to the wrong repository.
///
/// # No origin is not an error
///
/// A repository with no `origin` is ordinary — Conductor is local-first — and
/// §5.1 requires a value regardless. The normalized origin is then the empty
/// string and the root commit carries the identity alone. Refusing here would
/// make "has a remote" a precondition for using Conductor, which no section
/// asks for.
pub fn repo_identity(repo_root: &Path) -> Result<String, LedgerError> {
    let out = run_git_ok(
        repo_root,
        &["rev-list", "--max-parents=0", "--first-parent", "HEAD"],
    )?;
    let roots: Vec<String> = out
        .stdout_trimmed()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let [first_commit] = roots.as_slice() else {
        return Err(LedgerError::AmbiguousFirstCommit {
            root: repo_root.to_path_buf(),
            count: roots.len(),
            found: roots.join(" "),
        });
    };

    // `run_git`, not `run_git_ok`: `config --get` exits non-zero when the key is
    // absent, and "this repository has no origin" is a fact, not a failure.
    let origin = run_git(repo_root, &["config", "--get", "remote.origin.url"])?;
    let normalized = if origin.ok() {
        normalize_origin(&origin.stdout_trimmed())
    } else {
        String::new()
    };

    let mut hasher = blake3::Hasher::new();
    absorb(&mut hasher, REPO_IDENTITY_DOMAIN);
    absorb(&mut hasher, first_commit);
    absorb(&mut hasher, &normalized);
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// §5.1's *normalized* origin.
///
/// Five rules, each folding together spellings **git itself** treats as one
/// remote, and no more than that:
///
/// 1. surrounding whitespace,
/// 2. scp-style `user@host:path` becomes `ssh://user@host/path`, which is what
///    git documents it as shorthand for,
/// 3. userinfo is dropped — a credential or a login name in a URL says who is
///    fetching, not which repository is being fetched, and a token has no
///    business in a preimage,
/// 4. the scheme and host are lowercased; the path is not, because path case is
///    significant on some forges and not others and guessing which is worse
///    than keeping it,
/// 5. one trailing `/` and one trailing `.git` are dropped.
///
/// **Deliberately not folded: two different schemes.** `https://host/x` and
/// `ssh://host/x` normalize apart. They *are* usually the same repository, but
/// deciding that requires knowing the forge, and §3.5 already documents the
/// remedy for a repository that has changed identity — re-register it. An
/// over-eager normalizer that merged two genuinely different remotes would be
/// the failure that cannot be undone.
fn normalize_origin(url: &str) -> String {
    let trimmed = url.trim();
    // scp-style: a colon before any slash, and no scheme. `git@github.com:o/r`.
    let expanded = match trimmed.split_once(':') {
        Some((authority, path))
            if !trimmed.contains("://")
                && !authority.contains('/')
                && !authority.is_empty()
                && !path.is_empty() =>
        {
            format!("ssh://{authority}/{}", path.trim_start_matches('/'))
        }
        _ => trimmed.to_string(),
    };

    let rebuilt = match expanded.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = match rest.split_once('/') {
                Some((authority, path)) => (authority, path),
                None => (rest, ""),
            };
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            format!(
                "{}://{}/{path}",
                scheme.to_ascii_lowercase(),
                host.to_ascii_lowercase()
            )
        }
        None => expanded,
    };

    rebuilt
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_end_matches('/')
        .to_string()
}

/// Length-prefixed absorption: `<byte length> 0x1f <bytes>`.
///
/// §5.1 writes `‖` and does not say what it is. Plain concatenation makes
/// `("ab", "c")` and `("a", "bc")` one preimage, and both components here are
/// operator-influenced text — a remote URL especially. Same encoding, and same
/// reasoning, as `approval::binding`.
fn absorb(hasher: &mut blake3::Hasher, component: &str) {
    hasher.update(component.len().to_string().as_bytes());
    hasher.update(&[0x1f]);
    hasher.update(component.as_bytes());
}

/// Register a repository as a project — §5.1's `project` row, from
/// `.conductor/project.yaml`.
///
/// **Idempotent.** §3.5's recovery path is *"re-register the project → read
/// `.conductor/` → rebuild the task list"*, so registering an
/// already-registered repository must be an ordinary thing to do, not an error
/// discovered mid-recovery. The row's mutable facts are refreshed from the file
/// and `created_at` is left at the first registration —
/// `conductor_store::ledger::upsert_project`'s rule, whose collision check also
/// refuses a *different* id at an already-registered root path by name.
///
/// The stored `root_path` is canonicalized, because it is the key every later
/// call resolves §3.3 control 2 through: two spellings of one directory would
/// be two projects, and one of them would be reading a tree nobody registered.
pub fn register_project(
    store: &mut Store,
    repo_root: &Path,
    now_ms: i64,
) -> Result<ProjectRow, LedgerError> {
    let root = repo_root.canonicalize().map_err(|source| LedgerError::Io {
        what: "the repository root",
        path: repo_root.to_path_buf(),
        source,
    })?;
    let config = project::load(&root)?;
    let row = store.upsert_project(
        &NewProject {
            id: ProjectId::new(config.id.clone())?,
            root_path: root.to_string_lossy().into_owned(),
            repo_identity: repo_identity(&root)?,
            default_branch: config.default_branch.clone(),
            config_hash: config.config_hash().to_string(),
        },
        now_ms,
    )?;
    Ok(row)
}

/// Read, validate and record one plan version — §5.2's `DRAFT → VALIDATED`.
///
/// `catalogue` is the check id set from `verification.yaml`, assembled by the
/// caller. §3.7's clarification 3 is explicit that this is not the validator's
/// job — *"the validator takes the catalogue as a parameter and the caller
/// assembles it"* — and the same holds one level up: a ledger that reached into
/// the filesystem to resolve per-task profiles would be deciding the question
/// §3.7 defers.
///
/// The document is read from the **registered** tree (§3.3 control 2) and its
/// declared `version:` must match the directory it was read from, because
/// §3.4's trailer, §5.1's `UNIQUE(project_id, version)` and supersession all
/// need one answer to "which version is this?".
///
/// The row is created in `DRAFT` and moved to `VALIDATED` through the store's
/// legality table rather than inserted as `VALIDATED` directly, so that §5.2's
/// machine is the only thing that decides what a plan version's state may be.
///
/// # Why the [`ValidatedPlan`] comes back with the row
///
/// Task materialisation needs both: the row to hang §5.1's `task.plan_version_id`
/// off, and the validated document to build the task rows from. Returning only
/// the row would force the materializer to re-read and re-validate — two
/// validations of one document with a **window between them** in which the file
/// can change, which is precisely the class of gap §3.3 exists to close. The
/// document that was hashed into `content_hash` is the document handed back.
pub fn register_plan_version(
    store: &mut Store,
    project_id: &ProjectId,
    version: u32,
    catalogue: &BTreeSet<String>,
) -> Result<RegisteredPlanVersion, LedgerError> {
    let project = require_project(store, project_id)?;
    let relative = plan_path(version);
    let path = PathBuf::from(&project.root_path).join(&relative);
    let text = read_text("the plan document", &path)?;

    let document = super::parse(&text)?;
    if document.version != version {
        return Err(LedgerError::VersionMismatch {
            path,
            declared: document.version,
            directory: version,
        });
    }
    let validated =
        super::validate(&document, catalogue).map_err(|report| LedgerError::Refused {
            version,
            report: Box::new(report),
        })?;
    let hash = content_hash(&text)?;

    let id = plan_version_id(project_id, version);
    if let Some(existing) = store.plan_version(&id)? {
        return Err(LedgerError::AlreadyRegistered {
            id,
            stored: existing.content_hash,
            offered: hash.as_str().to_string(),
        });
    }
    store.create_plan_version(&NewPlanVersion {
        id: id.clone(),
        project_id: project_id.clone(),
        version: i64::from(version),
        content_hash: hash.as_str().to_string(),
        source_path: relative,
    })?;
    store.set_plan_state(&id, PlanVersionState::Validated)?;
    Ok(RegisteredPlanVersion {
        row: require_plan_version(store, project_id, &id)?,
        plan: validated,
    })
}

/// Approve one plan version — §5.2's `AWAITING_APPROVAL → APPROVED`, plus
/// §3.1's `APPROVED` sidecar and §3.3's independent store record.
///
/// # The order of operations, and what each failure leaves behind
///
/// 1. **The witness is re-derived first, and reads nothing but rows.** A
///    witness that does not resolve to a live §4.3 plan approval for this exact
///    version refuses here, before any state moves and before the grant is
///    spent.
/// 2. **The state moves next**, through
///    `conductor_store::ledger::set_plan_state`, so §5.2's legality table is
///    what refuses `VALIDATED → APPROVED`. A plan already `APPROVED` skips this
///    step entirely: re-approval changes the row's *content*, not its state
///    (§5.2's invalid list forbids `APPROVED → *` except `SUPERSEDED`, and its
///    restart clause still says a mismatch is *"cleared by re-running
///    `conductor plan approve`"*). Both hold, because those are two different
///    writes.
/// 3. **The grant is consumed**, immediately before the durable approval —
///    §4.3's rule, and what makes one human decision authorize exactly one
///    approval. Re-approving an edited plan therefore needs a *new* grant,
///    which is precisely what §5.2's restart clause asks a human to provide.
/// 4. **The store records the approval** (§3.3 control 3), then earlier
///    versions are superseded, then the sidecar is written.
///
/// A crash between 4's store write and the sidecar leaves the two disagreeing,
/// which [`verify_approval`] reports and which halts execution — the fail-closed
/// outcome, cleared by re-running approval. The store is written first
/// deliberately: it is the half that gates execution, so a half-finished
/// approval must be the one that *stops* work rather than the one that permits
/// it.
///
/// The approver and the policy hash come out of the grant and its request.
/// Taking either as a parameter would let a caller name an approver who
/// approved nothing.
pub fn approve(
    store: &mut Store,
    project_id: &ProjectId,
    version: u32,
    witness: &Authorization,
    now_ms: i64,
) -> Result<Approval, LedgerError> {
    let project = require_project(store, project_id)?;
    let id = plan_version_id(project_id, version);
    let row = require_plan_version(store, project_id, &id)?;
    let granted = rederive_witness(store.conn(), &id, witness, now_ms)?;

    let root = PathBuf::from(&project.root_path);
    let plan_file = root.join(plan_path(version));
    let hash = content_hash(&read_text("the plan document", &plan_file)?)?;

    if row.state != PlanVersionState::Approved {
        store.set_plan_state(&id, PlanVersionState::Approved)?;
    }
    match approval::consume(
        store.conn_mut(),
        &granted.grant_id,
        &granted.binding,
        now_ms,
    )? {
        Consumption::Consumed { .. } | Consumption::Reusable { .. } => {}
        Consumption::Refused(refusal) => {
            return Err(LedgerError::NotAuthorized {
                plan_version: id,
                detail: refusal.to_string(),
            });
        }
    }
    store.record_plan_approval_content(&id, hash.as_str(), &granted.approver, now_ms)?;
    supersede(store, project_id, version)?;

    let approval = Approval {
        plan_version_id: id,
        version,
        content_hash: hash,
        approver: granted.approver,
        approved_at_ms: now_ms,
        policy_hash: granted.policy_hash,
    };
    write_sidecar(&root.join(approved_path(version)), &approval)?;
    Ok(approval)
}

/// §3.3 control 3: does the registered tree's `APPROVED` sidecar agree with the
/// store?
///
/// Reads two things and changes neither. `&Store` rather than `&mut Store` is
/// the mechanism, not the style: an immutable borrow cannot write a row, so
/// *"it is never resynced"* is enforced by the compiler rather than by this
/// function remembering. Nothing here writes a file either — not the sidecar
/// from the store, and not the store from the sidecar.
///
/// Every disagreement names **both** values, because the question the reader is
/// about to answer is "which of these two is the one I meant?", and an error
/// that names one of them cannot be answered without going and looking up the
/// other.
///
/// The checks run store-against-file first (§3.3's control 3) and
/// file-against-document last (§5.2's restart clause). That order matters: a
/// tampered sidecar and an edited plan are different attacks with different
/// remedies, and reporting the second when the first is true would send the
/// reader to the wrong file.
pub fn verify_approval(
    store: &Store,
    project_id: &ProjectId,
    version: u32,
) -> Result<Approval, LedgerError> {
    let project = require_project(store, project_id)?;
    let id = plan_version_id(project_id, version);
    let row = require_plan_version(store, project_id, &id)?;
    if row.state != PlanVersionState::Approved {
        return Err(LedgerError::NotApproved {
            id,
            state: row.state,
        });
    }

    let root = PathBuf::from(&project.root_path);
    let sidecar_path = root.join(approved_path(version));
    let sidecar = read_sidecar(&sidecar_path)?;

    let disagree = |field: &'static str, in_file: String, in_store: String| {
        if in_file == in_store {
            return Ok(());
        }
        Err(LedgerError::Disagreement {
            plan_version: id.clone(),
            field,
            in_file,
            in_store,
        })
    };
    disagree(
        "the plan version it belongs to",
        sidecar.plan_version.clone(),
        id.as_str().to_string(),
    )?;
    disagree(
        "the version number",
        sidecar.version.to_string(),
        version.to_string(),
    )?;
    disagree(
        "the approved content hash",
        sidecar.content_hash.clone(),
        row.content_hash.clone(),
    )?;
    // An `APPROVED` row with no approver or no timestamp is the crash window
    // [`approve`] documents: the state moved and the approval content was never
    // recorded. Refused rather than read as `""` and `0`, because
    // `1970-01-01T00:00:00Z` is a real instant and a sidecar claiming it would
    // then *agree* with a row that records nothing at all.
    let (approved_by, approved_at) = match (&row.approved_by, row.approved_at) {
        (Some(by), Some(at)) => (by.clone(), at),
        _ => return Err(LedgerError::IncompleteApproval { id: id.clone() }),
    };
    disagree("the approver", sidecar.approver.clone(), approved_by)?;

    // Instants, not text. The sidecar's field is RFC 3339 and the column is an
    // integer, so the two are compared after conversion — otherwise a file
    // rewritten with `.000` on the seconds, or by a later Conductor that spells
    // the same instant differently, would read as a human disagreeing with
    // themselves. A field that does not parse at all is refused, never zeroed.
    let approved_at_ms = parse_sidecar_instant(&sidecar_path, &sidecar.approved_at)?;
    disagree(
        "when it was approved",
        format_rfc3339_utc(approved_at_ms),
        format_rfc3339_utc(approved_at),
    )?;

    let plan_file = root.join(plan_path(version));
    let actual = content_hash(&read_text("the plan document", &plan_file)?)?;
    if actual.as_str() != row.content_hash {
        return Err(LedgerError::Edited {
            plan_version: id,
            path: plan_file,
            approved: row.content_hash,
            actual: actual.as_str().to_string(),
        });
    }

    Ok(Approval {
        plan_version_id: id,
        version,
        content_hash: actual,
        approver: sidecar.approver,
        approved_at_ms,
        policy_hash: sidecar.policy_hash,
    })
}

/// §3.5's recovery path: restore an approval from the receipt it left behind,
/// after `conductor.db` has been lost.
///
/// # Why this is not a second door onto `APPROVED`
///
/// §5.2 says `APPROVED` is reachable *"only via a human at the control
/// socket"*, and this function has no [`Authorization`] parameter. The two are
/// reconciled by what the `APPROVED` sidecar **is**: [`approve`] writes it last,
/// *after* a real §4.3 grant has been re-derived and consumed, so the file is a
/// **receipt for a human decision that already happened at the socket** — the
/// same argument §3.4 makes for commit trailers. This function re-reads a
/// receipt; it does not mint authority. That is why it takes no witness and
/// spends no grant: there is no second decision to authorize.
///
/// It is also why §3.2 can promise that *"an approved plan must survive loss of
/// `conductor.db`"* at all. A grant lives only in the store, so after the store
/// is gone no witness exists anywhere; if approval were reconstructible only by
/// re-approving, §3.5's *"Not lost: every approved plan"* would be false, and a
/// machine that lost its database would need a human to re-approve every plan it
/// had ever approved.
///
/// # What makes the receipt safe to believe
///
/// Only §3.3's control 1: a change to `.conductor/**` arriving on a run branch
/// is rejected at reconciliation and never fetched, so an agent cannot put a
/// sidecar into the **registered** tree. The trust boundary is therefore
/// unchanged by this function — anyone who could forge this file could already
/// approve. What this function must not do is widen it, so every field of the
/// receipt is checked against something the receipt does not control:
///
/// * the version it names, against the directory it was found in;
/// * its content hash, against a **fresh** hash of the plan document — so an
///   edited plan is [`LedgerError::Edited`] here exactly as it is in
///   [`verify_approval`], rather than being re-approved at its new content;
/// * its timestamp, against RFC 3339 — never defaulted to `0`.
///
/// Any disagreement refuses **before** the state moves, so a refused adoption
/// leaves the version exactly where [`register_plan_version`] put it.
///
/// # Idempotent
///
/// A version already `APPROVED` at the same content re-records the same values
/// and moves no state, so re-running recovery is safe. §5.2 forbids `APPROVED →
/// APPROVED`, which is why the transition is skipped rather than re-applied.
pub fn adopt_approval(
    store: &mut Store,
    project_id: &ProjectId,
    version: u32,
) -> Result<Approval, LedgerError> {
    let project = require_project(store, project_id)?;
    let id = plan_version_id(project_id, version);
    let row = require_plan_version(store, project_id, &id)?;

    let root = PathBuf::from(&project.root_path);
    let sidecar_path = root.join(approved_path(version));
    let sidecar = read_sidecar(&sidecar_path)?;

    // A receipt is bound to one version. Copying one plan version's `APPROVED`
    // into another's directory must approve nothing.
    if sidecar.plan_version != id.as_str() {
        return Err(LedgerError::Disagreement {
            plan_version: id.clone(),
            field: "the plan version it belongs to",
            in_file: sidecar.plan_version.clone(),
            in_store: id.as_str().to_string(),
        });
    }
    if sidecar.version != version {
        return Err(LedgerError::Disagreement {
            plan_version: id.clone(),
            field: "the version number",
            in_file: sidecar.version.to_string(),
            in_store: version.to_string(),
        });
    }

    // ...and to one document. Hashed fresh rather than taken from the row, so
    // that a plan edited while the store was gone is refused rather than
    // silently re-approved at whatever it now says.
    let plan_file = root.join(plan_path(version));
    let actual = content_hash(&read_text("the plan document", &plan_file)?)?;
    if sidecar.content_hash != actual.as_str() {
        return Err(LedgerError::Edited {
            plan_version: id.clone(),
            path: plan_file,
            approved: sidecar.content_hash.clone(),
            actual: actual.as_str().to_string(),
        });
    }
    // The row this recovery just wrote came from the same file, so a mismatch
    // here means the file changed between registration and adoption — the
    // window §3.3 exists to close, and not something to paper over.
    if row.content_hash != actual.as_str() {
        return Err(LedgerError::Disagreement {
            plan_version: id.clone(),
            field: "the plan content hash",
            in_file: actual.as_str().to_string(),
            in_store: row.content_hash.clone(),
        });
    }

    let approved_at_ms = parse_sidecar_instant(&sidecar_path, &sidecar.approved_at)?;

    // §5.2's machine, walked rather than jumped: `VALIDATED → AWAITING_APPROVAL
    // → APPROVED` are the edges it draws, and `set_plan_state` is what refuses
    // anything else.
    if row.state != PlanVersionState::Approved {
        if row.state == PlanVersionState::Validated {
            store.set_plan_state(&id, PlanVersionState::AwaitingApproval)?;
        }
        store.set_plan_state(&id, PlanVersionState::Approved)?;
    }
    store.record_plan_approval_content(&id, actual.as_str(), &sidecar.approver, approved_at_ms)?;

    Ok(Approval {
        plan_version_id: id,
        version,
        content_hash: actual,
        approver: sidecar.approver,
        approved_at_ms,
        policy_hash: sidecar.policy_hash,
    })
}

/// Mark every version earlier than `version` `SUPERSEDED` — §5.2's *"by a later
/// `APPROVED`"*.
///
/// Strictly earlier, and only versions that are not already `SUPERSEDED`. Later
/// versions are left alone: §5.2's arrow points from an older plan to a newer
/// one, and superseding a *later* draft because an earlier version was
/// re-approved would delete work nobody asked to delete.
///
/// Called by [`approve`], and public because §3.5's recovery path rebuilds a
/// ledger from `.conductor/` and has to be able to re-establish the same
/// relation without re-approving anything.
pub fn supersede(
    store: &mut Store,
    project_id: &ProjectId,
    version: u32,
) -> Result<Vec<PlanVersionId>, LedgerError> {
    let mut superseded = Vec::new();
    for row in store.plan_versions_for_project(project_id)? {
        if row.version >= i64::from(version) || row.state == PlanVersionState::Superseded {
            continue;
        }
        store.supersede_plan_version(&row.id)?;
        superseded.push(row.id);
    }
    Ok(superseded)
}

// ---------------------------------------------------------------------------
// the witness
// ---------------------------------------------------------------------------

/// What a witness resolves to once nothing in it is taken on trust.
struct Granted {
    grant_id: String,
    approver: String,
    policy_hash: String,
    binding: BindingHash,
}

/// Re-derive an [`Authorization`] into the facts an approval may be written
/// from.
///
/// Nothing that arrives in the witness is believed except the grant *id*, and
/// even that is only used to find a row. Everything the approval records — who
/// approved, against which policy snapshot, over which binding — is read from
/// `approval_request` and `approval_grant`, and the binding is **recomputed**
/// from the request's own material and compared against the stored one. That
/// is `approval::authorize`'s doctrine, applied to a code path that does not
/// go through it: *"a row whose digest disagrees with its own inputs (a hand
/// edit, a partial write, a serializer change) authorizes nothing."*
///
/// `approval::authorize` itself cannot be used here, and that is not an
/// oversight: it answers a **policy** question — it takes a
/// `policy::evaluate::Decision` and refuses anything whose effect is not
/// `require_approval`. A plan approval has no policy action and no effect;
/// §4.3's table gives it its own kind and its own subject. Reusing `authorize`
/// would mean inventing a fake policy decision for a plan, which is exactly the
/// collapse §4.3 forbids: *"Collapsing them would let a plan approval satisfy a
/// deployment gate."*
fn rederive_witness(
    conn: &Connection,
    plan_version: &PlanVersionId,
    witness: &Authorization,
    now_ms: i64,
) -> Result<Granted, LedgerError> {
    let refuse = |detail: String| LedgerError::NotAuthorized {
        plan_version: plan_version.clone(),
        detail,
    };

    let grant_id = match witness {
        Authorization::Refused(reason) => return Err(refuse(reason.to_string())),
        Authorization::Authorized { grant_id } => grant_id,
    };
    let grant = approval::store::grant_row(conn, grant_id)?
        .ok_or_else(|| refuse(format!("there is no grant {grant_id}")))?;
    if let Some(reason) = approval::store::refuse_unusable(&grant, now_ms) {
        return Err(refuse(reason.to_string()));
    }
    let request = approval::store::request_row(conn, &grant.request_id)?
        .ok_or_else(|| refuse(format!("grant {grant_id} answers no request")))?;
    if request.kind != ApprovalKind::Plan {
        return Err(refuse(format!(
            "grant {grant_id} is a {} and this needs a {} (§4.3: the four kinds \
             never collapse)",
            request.kind,
            ApprovalKind::Plan
        )));
    }
    match &request.subject {
        Subject::PlanVersion { plan_version_id } if plan_version_id == plan_version.as_str() => {}
        other => {
            return Err(refuse(format!(
                "grant {grant_id} authorizes {other}, not {plan_version}"
            )));
        }
    }

    let binding = Binding {
        subject: request.subject.clone(),
        facts: request.facts.clone(),
        policy_hash: request.policy_hash.clone(),
        scope: grant.scope.clone(),
    }
    .hash();
    if binding != grant.stored_binding {
        return Err(refuse(format!(
            "grant {grant_id} stores binding {} but its own inputs recompute to \
             {binding}; §4.3 authorizes on the recomputed hash, so a row that \
             disagrees with itself authorizes nothing",
            grant.stored_binding
        )));
    }

    Ok(Granted {
        grant_id: grant.id,
        approver: grant.granted_by,
        policy_hash: request.policy_hash,
        binding,
    })
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The registered project, or a refusal naming the id.
///
/// Every function except [`register_project`] goes through here, and that is
/// how §3.3 control 2 is enforced: `root_path` comes from this row and from
/// nowhere else.
fn require_project(store: &Store, id: &ProjectId) -> Result<ProjectRow, LedgerError> {
    store
        .project(id)?
        .ok_or_else(|| LedgerError::UnknownProject { id: id.clone() })
}

fn require_plan_version(
    store: &Store,
    project: &ProjectId,
    id: &PlanVersionId,
) -> Result<PlanVersionRow, LedgerError> {
    store
        .plan_version(id)?
        .ok_or_else(|| LedgerError::UnknownPlanVersion {
            id: id.clone(),
            project: project.clone(),
        })
}

fn read_text(what: &'static str, path: &Path) -> Result<String, LedgerError> {
    std::fs::read_to_string(path).map_err(|source| LedgerError::Io {
        what,
        path: path.to_path_buf(),
        source,
    })
}

/// Read the sidecar. Unknown keys load, as everywhere in `.conductor/`; a
/// sidecar that does not parse is a refusal, because the alternative to reading
/// it is not "assume it is fine".
fn read_sidecar(path: &Path) -> Result<Sidecar, LedgerError> {
    let text = read_text("the APPROVED sidecar", path)?;
    let document: ApprovedDocument =
        serde_yaml::from_str(&text).map_err(|error| LedgerError::UnreadableSidecar {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
    Ok(document.approved)
}

/// Read the sidecar's RFC 3339 timestamp back into the instant §5.1's column
/// holds.
///
/// A value that does not parse is [`LedgerError::UnreadableSidecar`], never a
/// default. The tempting default is `0`, and `0` is `1970-01-01T00:00:00Z` — a
/// perfectly real instant, which is exactly why it must not be what a corrupt
/// or hand-edited field silently means.
fn parse_sidecar_instant(path: &Path, text: &str) -> Result<i64, LedgerError> {
    parse_rfc3339_utc(text).ok_or_else(|| LedgerError::UnreadableSidecar {
        path: path.to_path_buf(),
        detail: format!(
            "`approved_at` is {text:?}, which is not an RFC 3339 UTC timestamp \
             (§4.4: UTC only, `Z` required)"
        ),
    })
}

fn write_sidecar(path: &Path, approval: &Approval) -> Result<(), LedgerError> {
    let document = ApprovedDocument {
        approved: Sidecar {
            plan_version: approval.plan_version_id.as_str().to_string(),
            version: approval.version,
            content_hash: approval.content_hash.as_str().to_string(),
            approver: approval.approver.clone(),
            approved_at: format_rfc3339_utc(approval.approved_at_ms),
            policy_hash: approval.policy_hash.clone(),
        },
    };
    let text =
        serde_yaml::to_string(&document).map_err(|error| LedgerError::UnreadableSidecar {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| LedgerError::Io {
            what: "the plan version directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(path, text).map_err(|source| LedgerError::Io {
        what: "the APPROVED sidecar",
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_version_id_names_its_project_and_its_version() {
        let project = ProjectId::new("p-x").expect("id");
        assert_eq!(plan_version_id(&project, 3).as_str(), "p-x-v3");
    }

    #[test]
    fn the_spellings_of_one_remote_that_git_treats_as_one_normalize_together() {
        let canonical = "ssh://github.com/conductor/fixture";
        for spelling in [
            "git@github.com:conductor/fixture.git",
            "git@github.com:conductor/fixture",
            "ssh://git@github.com/conductor/fixture.git",
            "ssh://git@GitHub.com/conductor/fixture",
            "  ssh://github.com/conductor/fixture/  ",
        ] {
            assert_eq!(normalize_origin(spelling), canonical, "{spelling}");
        }
    }

    #[test]
    fn two_remotes_that_are_not_the_same_repository_do_not_normalize_together() {
        // The direction that matters: an over-eager normalizer would merge two
        // projects into one ledger.
        assert_ne!(
            normalize_origin("git@github.com:conductor/fixture.git"),
            normalize_origin("git@github.com:conductor/other.git")
        );
        // Path case is kept — see `normalize_origin`'s docs.
        assert_ne!(
            normalize_origin("ssh://github.com/conductor/Fixture"),
            normalize_origin("ssh://github.com/conductor/fixture")
        );
        // The scheme is deliberately not folded.
        assert_ne!(
            normalize_origin("https://github.com/conductor/fixture"),
            normalize_origin("ssh://github.com/conductor/fixture")
        );
    }

    #[test]
    fn a_repository_identity_and_a_plan_hash_never_share_a_digest_space() {
        // `REPO_IDENTITY_DOMAIN` earns its place only if it is actually absorbed.
        let mut with_domain = blake3::Hasher::new();
        absorb(&mut with_domain, REPO_IDENTITY_DOMAIN);
        absorb(&mut with_domain, "abc");
        absorb(&mut with_domain, "");
        let mut without = blake3::Hasher::new();
        absorb(&mut without, "abc");
        absorb(&mut without, "");
        assert_ne!(with_domain.finalize(), without.finalize());
    }

    #[test]
    fn the_components_of_a_repository_identity_cannot_be_slid_past_one_another() {
        // Without a length prefix these two share a preimage.
        let identity = |commit: &str, origin: &str| {
            let mut hasher = blake3::Hasher::new();
            absorb(&mut hasher, REPO_IDENTITY_DOMAIN);
            absorb(&mut hasher, commit);
            absorb(&mut hasher, origin);
            hasher.finalize()
        };
        assert_ne!(identity("ab", "c"), identity("a", "bc"));
    }
}
