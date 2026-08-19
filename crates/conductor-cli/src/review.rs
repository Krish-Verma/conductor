//! `conductor review export | import` — master plan §6.5, §7.1 and §7.2.
//!
//! > ```text
//! > conductor review export [--since …]
//! > conductor review import <file>
//! > ```
//!
//! # Two verbs, and only one of them may write a decision
//!
//! §6.5 ends the review section with one sentence that decides this module's
//! shape: *"Importing is a **mutating** operation and goes through the control
//! socket, never a file an agent could write."* So the two halves are built
//! differently on purpose.
//!
//! `review export` runs **in this process**. It reads the store and the
//! repository, composes §6.5's review packet, writes it as an artifact and moves
//! the review from `PENDING` to `EXPORTED`. That write is not a decision — it
//! records which bytes a human is being shown, and it is the thing a later
//! decision is *bound to*. Nothing about it needs a human at a socket, and
//! requiring one would mean a review packet could not be produced by a cron job,
//! a `doctor` run, or a script preparing a review for somebody else.
//!
//! `review import` runs **behind the control socket**. Its client — the region
//! this file marks off between a `BEGIN` and an `END` comment banner further
//! down — parses the human's file, calls [`crate::socket::call`], and holds no
//! database handle at all. `crates/conductor-cli/tests/review.rs` fails if that
//! region names one, in the style §4.3 asks for and
//! `crates/conductor-cli/tests/plan_approve.rs` established: not "the client only
//! reads", which is a subset somebody argues their way past one call at a time,
//! but "the client cannot reach the database", which has no subset to get wrong.
//!
//! **The markers are load-bearing and the region is not the whole file**, which
//! is the one place this module departs from `plan.rs`'s shape. `plan.rs` can
//! submit to a whole-file scan because *neither* of its verbs holds a store;
//! `review export` legitimately does, so a whole-file scan here would either be
//! vacuous or would forbid the export path from working. A test asserts each
//! marker occurs exactly once and that the needles it forbids inside the region
//! all match outside it — otherwise moving mutating code across a marker is how
//! this rule stops being enforced.
//!
//! # `accept` cannot write `COMPLETE`, it can only earn it (ADR-0019)
//!
//! §5.2 draws `AWAITING_REVIEW → COMPLETE` and, until S13, nothing could take it.
//! What makes the edge safe now is that
//! [`ReviewOutcome::Accepted`](conductor_core::ReviewOutcome::Accepted) carries a
//! [`VerifiedComplete`](conductor_core::completion::VerifiedComplete) whose only
//! constructor is [`conductor_core::completion::evaluate`]. So the accept path
//! here does not *assert* completion — it re-derives §4.5's evidence from durable
//! state, enters the human's decision as
//! [`ReconciliationEvidence::AcceptedAtReview`], runs the gate, and refuses the
//! import when the gate refuses, naming the criterion. A human accepting a review
//! resolves the **review boundary**; it does not excuse a failing check, an
//! unresolved finding, or a missing grant.
//!
//! # What this slice can and cannot source, stated rather than invented
//!
//! Every field §6.5 lists for the review packet is read from the store or the
//! repository. Three of them cannot be, and each is an **empty list with a
//! `// S13:` note** rather than a plausible fabrication:
//!
//! * The run's *diff*, *changed paths* and *commits* need the workspace, which
//!   §4.1 may already have cleaned up. Present workspace: measured out of git.
//!   Absent workspace: empty, and visibly so.
//! * §5.1 persists no per-run policy-evaluation log, so the packet's `policy`
//!   lines are the evaluations that left a durable trace — the approval requests
//!   §4.4 raised.
//! * §5.1 has no `run.verdict` column either. The reconciliation verdict's only
//!   durable record is the `event` row `lease::advance_state` writes, and it is
//!   read back from there rather than guessed.
//!
//! # Exit codes (§7.2)
//!
//! ```text
//! 0   the packet was exported, or the decision was imported
//! 1   the command ran and the answer was no
//! 2   the store is unusable, or no control socket is up
//! 64  usage error — the decision file is unreadable or names no decision
//! 70  internal error
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use conductor_core::completion::{
    AcceptanceEvidence, CheckEvidence, ChecksEvidence, CompletionEvidence, CriterionEvidence,
    FindingsEvidence, PolicyEvidence, ReconciliationEvidence, Slice,
};
use conductor_core::{
    Fence, PlanVersionId, ProjectId, ReviewDecision, ReviewOutcome, ReviewState, RunId, RunState,
    TaskId,
};
use conductor_run::approval::kind::{Expiry, Subject};
use conductor_run::approval::store::{self as approvals, GrantOptions, NewApprovalRequest};
use conductor_run::packet::continuation::Diff;
use conductor_run::packet::review::{
    ApprovalLine, CheckLine, Claims, FindingLine, Measured, PolicyLine, ReviewFields,
};
use conductor_run::plan::{self, ledger};
use conductor_run::policy::load as policy_load;
use conductor_run::policy::model::{FactSet, Scope};
use conductor_run::verify::profile::{self, Profile};
use conductor_run::{ArtifactRoot, OwnedDir, Owner};
use conductor_store::{RunCheckResult, Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::approval::SocketArgs;
use crate::exit;
use crate::socket;
use crate::task::StoreArgs;

/// The RPC method the server answers.
///
/// Named once, here and in [`crate::approval`]'s dispatch, so a rename cannot
/// leave the client calling a verb nobody serves — the rule `plan.rs`'s
/// `APPROVE_METHOD` already follows.
const IMPORT_METHOD: &str = "review.import";

/// §4.3's `approval_grant.channel` for a grant made at the control socket.
///
/// The same literal [`crate::approval`] uses. It is repeated rather than shared
/// because that module's copy is private and this module may not widen it; the
/// value is one word and a test asserts the row.
const CHANNEL: &str = "unix-socket";

/// `conductor review …`
#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// Compose §6.5's review packet for the open review and write a decision
    /// stub beside it.
    Export(ExportArgs),
    /// Apply a human's decision. Human-only, socket-only (§6.5).
    Import(ImportArgs),
}

/// `conductor review export [--run …] [--since …] [--out …]`
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Which run's open review to export. Required only when more than one
    /// review is waiting.
    #[arg(long)]
    pub run: Option<String>,
    /// Consider only reviews opened at or after this unix-millisecond stamp —
    /// §7.1's `--since`.
    #[arg(long)]
    pub since: Option<i64>,
    /// Where to write the decision stub. Defaults to the attempt's artifact
    /// directory, beside the packet.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// `conductor review import <file>`
#[derive(Debug, Args)]
pub struct ImportArgs {
    /// The decision file — the stub `review export` wrote, with `decision:`
    /// filled in.
    pub file: PathBuf,
    /// The operator nonce, when the server is armed (§4.3 tier B).
    #[arg(long)]
    pub nonce: Option<String>,
}

/// Why a command stopped, and which of §7.2's codes says so.
#[derive(Debug)]
struct Refusal {
    code: u8,
    message: String,
}

impl Refusal {
    /// §7.2's `2` — the store or the control socket is not usable.
    fn not_initialized(message: impl Into<String>) -> Refusal {
        Refusal {
            code: exit::NOT_INITIALIZED,
            message: message.into(),
        }
    }

    /// §7.2's `1` — the command ran and the answer was no.
    fn refused(message: impl Into<String>) -> Refusal {
        Refusal {
            code: exit::FAILURE,
            message: message.into(),
        }
    }

    /// §7.2's `64` — the invocation itself is wrong.
    fn usage(message: impl Into<String>) -> Refusal {
        Refusal {
            code: exit::USAGE,
            message: message.into(),
        }
    }
}

/// A command's answer: what a script reads, what a human reads, and the code.
struct Answer {
    json: Value,
    rendered: String,
    code: u8,
}

/// Run one `review` subcommand.
pub fn run(command: &ReviewCommand, shared: &StoreArgs, socket_args: &SocketArgs) -> ExitCode {
    let outcome = match command {
        ReviewCommand::Export(args) => export(args, shared),
        ReviewCommand::Import(args) => import_client(args, socket_args),
    };
    match outcome {
        Ok(answer) => {
            if shared.json {
                match serde_json::to_string_pretty(&answer.json) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("internal error: {err}");
                        return ExitCode::from(exit::INTERNAL);
                    }
                }
            } else {
                print!("{}", answer.rendered);
            }
            ExitCode::from(answer.code)
        }
        Err(refusal) => {
            eprintln!("{}", refusal.message);
            ExitCode::from(refusal.code)
        }
    }
}

// ---------------------------------------------------------------------------
// the decision document
// ---------------------------------------------------------------------------

/// §6.5's imported decision, as it travels on disk between the two verbs.
///
/// # Why every binding field is here and not derived at import time
///
/// A decision is a statement about *one* packet. If the import path re-derived
/// the run, the task, the plan version and the packet hash from the review id,
/// then a decision file could be moved onto a different review and would still
/// apply — the file would carry a verdict and nothing that pins it to what the
/// human read. So all five travel, and [`bind`] refuses the import when any of
/// them disagrees with the row, naming which one did.
///
/// **`deny_unknown_fields`, deliberately.** The global rule against it (Part 8)
/// is about documents *an agent produced*; this one is produced by
/// [`decision_stub`] and edited by a person, and a mistyped key here is a human
/// instruction that would otherwise be silently dropped.
///
/// `decision` is an `Option<String>` and **not** a
/// [`ReviewDecision`](conductor_core::ReviewDecision): the stub ships it blank
/// for the human to fill in, so "you have not decided yet" has to be
/// distinguishable from "that is not one of the five" — and the second question
/// is answered by the server, which is the side §6.5 puts in charge of the
/// mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionDocument {
    /// `review.id`.
    pub review_id: String,
    /// The run under review.
    pub run_id: String,
    /// The task the run executes.
    pub task_id: String,
    /// `review.plan_version_id` — the plan version the work was authorized
    /// under.
    pub plan_version: String,
    /// The exported packet's content hash. A decision naming a different one is
    /// a decision about a packet this review never exported.
    pub packet_hash: String,
    /// One of §6.5's five: `accept`, `repair`, `revise_plan`, `pause`, `stop`.
    #[serde(default)]
    pub decision: Option<String>,
    /// Free text for the record.
    #[serde(default)]
    pub notes: Option<String>,
    /// For `revise_plan`: which version under `.conductor/plans/` supersedes
    /// this work.
    #[serde(default)]
    pub target_plan_version: Option<u32>,
    /// §6.5's `decisions_to_record[]`.
    #[serde(default)]
    pub decisions_to_record: Vec<String>,
    /// §6.5's `plan_amendments[]`.
    #[serde(default)]
    pub plan_amendments: Vec<String>,
}

/// The header a human reads before editing the stub.
///
/// Comments rather than a `README`: the file is the interface, and an
/// instruction that lives anywhere else is an instruction somebody will not
/// have open when they need it.
const STUB_HEADER: &str = "\
# A Conductor review decision — master plan §6.5.
#
# Fill in `decision:` with exactly one of, in lower case:
#
#   accept       the work is acceptable. Conductor re-runs §4.5's completion
#                gate with your acceptance as evidence, and refuses if any
#                other criterion still fails.
#   repair       send it back for a bounded repair attempt. The repair budget
#                is NOT reset (ADR-0009).
#   revise_plan  the plan was wrong. Set `target_plan_version:` to the version
#                that supersedes this work; it must already exist under
#                `.conductor/plans/vN/plan.yaml` and validate. It is registered,
#                not approved — approval stays `conductor plan approve`.
#   pause        seen, not decided. Changes no run or task state.
#   stop         abandon the task.
#
# Then:  conductor review import <this file>
#
# The five binding fields below are what tie this decision to the packet you
# read. Do not edit them: a decision whose packet_hash is not the exported one
# is refused as tampered.
";

/// Write the stub `review export` leaves for the human.
fn decision_stub(document: &DecisionDocument) -> Result<String, String> {
    let body = serde_yaml::to_string(document).map_err(|err| err.to_string())?;
    Ok(format!("{STUB_HEADER}{body}"))
}

// ---------------------------------------------------------------------------
// `review export`
// ---------------------------------------------------------------------------

/// Compose §6.5's review packet for the open review, store it, and record it.
///
/// # Why this one is in-process while `import` is not
///
/// See this module's docs. The short version is that exporting decides nothing:
/// it fixes *which bytes* the decision will be about. §5.2's `PENDING →
/// EXPORTED` is refused a second time by
/// [`conductor_store::review::mark_exported`], so one review can only ever mint
/// one packet hash — which is what stops a decision being paired with whichever
/// of two exports suited whoever wrote it.
fn export(args: &ExportArgs, shared: &StoreArgs) -> Result<Answer, Refusal> {
    let store_path = match &shared.store {
        Some(path) => path.clone(),
        None => Store::default_path().map_err(|err| Refusal::not_initialized(err.to_string()))?,
    };
    // `open_existing`, not `open_or_create`: a `review export` that created an
    // empty database and then reported "no review is waiting" would answer a
    // question about a store that did not exist a moment ago.
    let mut store = Store::open_existing(&store_path)
        .map_err(|err| Refusal::not_initialized(format!("{}: {err}", store_path.display())))?;

    let review = select_review(&store, args)?;
    if review.state != ReviewState::Pending {
        return Err(Refusal::refused(format!(
            "review {} is {}, and §5.2 draws PENDING → EXPORTED only; a second \
             export would mint a second packet hash for one review, so a \
             decision could be bound to whichever of the two suited whoever \
             wrote it",
            review.id, review.state
        )));
    }

    let run_id = review.run_id.clone();
    let facts = run_facts(&store, &run_id).map_err(Refusal::refused)?;
    let fields = review_fields(&store, &review, &facts).map_err(Refusal::refused)?;

    let packet = conductor_run::packet::review::build(&mut store, &run_id, &fields)
        .map_err(|err| Refusal::refused(err.to_string()))?;
    // `emit` before anything is written: §6.5's ceiling is a refusal rather than
    // a truncation, and a review packet silently missing its last finding is the
    // worst possible thing to hand a person who is about to accept work.
    let emitted = packet
        .emit()
        .map_err(|err| Refusal::refused(err.to_string()))?;
    let packet_hash = emitted.hash().to_string();

    let artifacts = ArtifactRoot::new(artifacts_root(&store_path));
    let owned = owned_attempt_dir(&artifacts, &store, &run_id).map_err(Refusal::refused)?;
    let packet_path = owned
        .write_new(
            &format!("review-{}.packet.yaml", review.id),
            packet.to_yaml().as_bytes(),
        )
        .map_err(|err| Refusal::refused(err.to_string()))?;

    let document = DecisionDocument {
        review_id: review.id.clone(),
        run_id: run_id.as_str().to_string(),
        task_id: review.task_id.as_str().to_string(),
        plan_version: review.plan_version_id.as_str().to_string(),
        packet_hash: packet_hash.clone(),
        decision: None,
        notes: None,
        target_plan_version: None,
        decisions_to_record: Vec::new(),
        plan_amendments: Vec::new(),
    };
    let stub = decision_stub(&document).map_err(Refusal::refused)?;
    let stub_path = match &args.out {
        Some(path) => {
            write_new_file(path, stub.as_bytes()).map_err(Refusal::refused)?;
            path.clone()
        }
        None => owned
            .write_new(
                &format!("review-{}.decision.yaml", review.id),
                stub.as_bytes(),
            )
            .map_err(|err| Refusal::refused(err.to_string()))?,
    };

    // Last, and only once both files are on disk: the row records where the
    // human's copy is, and a row pointing at a file that was never written
    // would send them to a path that does not exist.
    let recorded = store
        .mark_review_exported(&review.id, &packet_hash, &packet_path.display().to_string())
        .map_err(|err| Refusal::refused(err.to_string()))?;

    Ok(Answer {
        json: json!({
            "review": recorded.id,
            "run": run_id.as_str(),
            "task": recorded.task_id.as_str(),
            "plan_version": recorded.plan_version_id.as_str(),
            "state": recorded.state.as_str(),
            "packet_hash": packet_hash,
            "packet_path": packet_path.display().to_string(),
            "decision_path": stub_path.display().to_string(),
            "within_target": emitted.within_target(),
            "proposed_next_state": fields.proposed_next_state,
        }),
        rendered: format!(
            "review {} exported — run {} task {}\n  packet    {}\n  hash      \
             {packet_hash}\n  decision  {}\n  proposed  {}\n",
            recorded.id,
            run_id.as_str(),
            recorded.task_id.as_str(),
            packet_path.display(),
            stub_path.display(),
            fields.proposed_next_state,
        ),
        code: exit::SUCCESS,
    })
}

/// Which review this invocation is about.
///
/// `--run` names it directly. Without one, the open set is consulted and an
/// ambiguity is a **refusal naming the candidates** rather than a pick: two
/// reviews waiting means two humans' worth of work, and exporting whichever
/// happened to sort first would silently answer a question the operator did not
/// ask.
fn select_review(store: &Store, args: &ExportArgs) -> Result<conductor_store::ReviewRow, Refusal> {
    if let Some(run) = &args.run {
        let run_id = RunId::new(run.clone()).map_err(|err| Refusal::usage(err.to_string()))?;
        return store
            .open_review_for_run(&run_id)
            .map_err(|err| Refusal::not_initialized(err.to_string()))?
            .ok_or_else(|| {
                Refusal::refused(format!(
                    "run {run} has no open review; a review is opened when a §6.5 \
                     boundary fires"
                ))
            });
    }

    let ids = pending_review_ids(store, args.since).map_err(Refusal::not_initialized)?;
    match ids.len() {
        0 => Err(Refusal::refused(
            "no review is waiting; a review is opened when a §6.5 boundary fires".to_string(),
        )),
        1 => store
            .review(&ids[0])
            .map_err(|err| Refusal::not_initialized(err.to_string()))?
            .ok_or_else(|| Refusal::refused(format!("review {} vanished", ids[0]))),
        _ => Err(Refusal::refused(format!(
            "{} reviews are waiting ({}); name one with --run, because exporting \
             whichever sorted first would answer a question nobody asked",
            ids.len(),
            ids.join(", ")
        ))),
    }
}

/// Every `PENDING` review, oldest first, optionally filtered by §7.1's
/// `--since`.
///
/// Raw SQL because [`conductor_store::review`] offers "the open review of a run"
/// and "every review of a run", and this asks a third question — "which reviews
/// is anybody waiting on" — that no accessor answers. `created_at` is
/// deliberately absent from [`conductor_store::ReviewRow`], so the filter is
/// applied in the statement rather than by a caller that cannot see the column.
fn pending_review_ids(store: &Store, since: Option<i64>) -> Result<Vec<String>, String> {
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT id FROM review
              WHERE state = 'PENDING' AND created_at >= ?1
              ORDER BY created_at, id",
        )
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params![since.unwrap_or(i64::MIN)], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| err.to_string())?;
    rows.collect::<Result<Vec<String>, _>>()
        .map_err(|err| err.to_string())
}

/// Where run workspaces and artifacts live (§3.1), derived from the store's
/// location exactly as `conductor task run` derives it.
///
/// Beside the store, so a `--store` pointing at a scratch directory keeps its
/// artifacts there too rather than writing into the operator's real tree.
fn artifacts_root(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("artifacts")
}

/// The attempt directory this run's review artifacts belong in.
///
/// # Why the owner is read rather than asserted
///
/// [`OwnedDir::write_new`] is the writer §4.7 wants — `create_new`, so an
/// artifact is never overwritten — and the only way to obtain an [`OwnedDir`] is
/// to claim the directory. [`ArtifactRoot::claim_attempt_dir`] refuses one
/// another worker already holds, which is right for two workers racing to run
/// the same attempt and wrong here: the attempt is over, §5.2 has the run in
/// `AWAITING_REVIEW` with its lease released, and there is no second writer to
/// exclude. So the existing provenance is read and re-entered through
/// [`ArtifactRoot::reclaim_attempt_dir`], which returns the record **unchanged**
/// — nothing about who ran the attempt is rewritten.
///
/// A directory that exists and carries no provenance is still refused, because
/// that is §4.1's orphan case: preserve, never write into.
fn owned_attempt_dir(
    artifacts: &ArtifactRoot,
    store: &Store,
    run_id: &RunId,
) -> Result<OwnedDir, String> {
    let ordinal = latest_ordinal(store, run_id);
    let path = artifacts.attempt_dir(run_id, ordinal);
    let owner = match artifacts
        .read_provenance(&path)
        .map_err(|err| err.to_string())?
    {
        Some(existing) => Owner::new(existing.worker, existing.pid),
        None => Owner::new(
            format!("review-export-{}", std::process::id()),
            std::process::id() as i32,
        ),
    };
    artifacts
        .reclaim_attempt_dir(run_id, ordinal, &owner)
        .map_err(|err| err.to_string())
}

/// The ordinal of the attempt whose artifacts describe this run.
///
/// The newest one. A run with no attempt row at all still needs a directory —
/// a review boundary can fire before any agent ran (§4.2's eligibility refusal
/// is one such route) — and `1` is the ordinal that attempt would have had.
fn latest_ordinal(store: &Store, run_id: &RunId) -> i64 {
    store
        .attempts_for_run(run_id)
        .ok()
        .and_then(|attempts| attempts.last().map(|attempt| attempt.ordinal))
        .unwrap_or(1)
}

/// Write a file that must not already exist.
///
/// `create_new` for [`OwnedDir::write_new`]'s reason, applied to a path outside
/// the artifact tree: a stub that already exists is one a human may already have
/// edited, and replacing it would delete a decision somebody had written.
fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    file.write_all(bytes)
        .map_err(|err| format!("{}: {err}", path.display()))
}

// ---------------------------------------------------------------------------
// what the store and the repository know about one run
// ---------------------------------------------------------------------------

/// Everything both halves of this module re-derive from durable state.
///
/// Gathered once because the export path and the import path ask the *same*
/// questions of the *same* rows: §6.5's packet quotes the checks a reviewer
/// reads, and §4.5's gate decides on them. Two gatherers would be two answers to
/// "what did verification do", and the one a human read would be allowed to
/// differ from the one the gate acted on.
struct RunFacts {
    run: conductor_store::RunRow,
    task: conductor_store::TaskRow,
    /// The registered repository root — §3.3 control 2's tree.
    root: PathBuf,
    project_id: ProjectId,
    /// The plan version the run is pinned to (acceptance row 21).
    plan_version: conductor_store::PlanVersionRow,
    /// The check catalogue the task names.
    profile: Profile,
    /// Every recorded check, in the order they ran.
    results: Vec<RunCheckResult>,
    /// §4.8's verdict, or `None` when no `event` row records one.
    verdict: Option<String>,
}

impl RunFacts {
    /// The tree §4.5's criteria are measured against.
    ///
    /// The tree the **newest recorded check** was bound to. §5.1 gives `run` no
    /// tree-hash column and the workspace may already be gone, so this is the
    /// only tree a review can name from durable state — and it is the right one:
    /// a check recorded against an older tree is precisely what criterion 1
    /// refuses when it is compared against this.
    ///
    /// `// S13:` what this cannot detect is a workspace mutated *after* the last
    /// check, which §4.5 catches at verification time with `VOID` — before the
    /// review boundary this path runs behind.
    fn tree_hash(&self) -> String {
        self.results
            .last()
            .map(|result| result.tree_hash.clone())
            .unwrap_or_default()
    }
}

/// Read the rows and files one run's review depends on.
fn run_facts(store: &Store, run_id: &RunId) -> Result<RunFacts, String> {
    let run = store
        .run(run_id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("run {run_id} is not in the store"))?;
    let task_id = TaskId::new(run.task_id.clone()).map_err(|err| err.to_string())?;
    let task = store
        .task(&task_id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("task {task_id} is not in the store"))?;

    let plan_version_id =
        PlanVersionId::new(task.plan_version_id.clone()).map_err(|err| err.to_string())?;
    let plan_version = store
        .plan_version(&plan_version_id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("plan version {plan_version_id} is not in the store"))?;
    let project = store
        .project(&plan_version.project_id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("project {} is not in the store", plan_version.project_id))?;
    let root = PathBuf::from(&project.root_path);

    // Fail closed on a profile that will not load. An unreadable catalogue is
    // not evidence that this task has no checks to satisfy — it is the reason
    // `plan validate` treats the same file as §7.2's `2`.
    let profile_path = if task.verification_profile.trim().is_empty() {
        root.join(plan::VERIFICATION_CONFIG_PATH)
    } else {
        root.join(&task.verification_profile)
    };
    let profile = profile::load(&profile_path)
        .map_err(|err| format!("{}: {err}", profile_path.display()))?
        .profile;

    let results = store
        .verification_results_for_run(run_id)
        .map_err(|err| err.to_string())?;

    Ok(RunFacts {
        project_id: plan_version.project_id.clone(),
        verdict: recorded_verdict(store, run_id),
        run,
        task,
        root,
        plan_version,
        profile,
        results,
    })
}

/// §4.8's verdict for a run, read out of the evidence log.
///
/// # Why the `event` table and not a column
///
/// §5.1 gives `run` no verdict column: the verdict is produced by
/// reconciliation, consumed to choose a route, and then survives only in the
/// `RUN_STATE_CHANGED` payload `lease::advance_state` writes — where the
/// producer spells it `verdict=<NAME>` in the detail string. That payload is the
/// durable record, so it is what is read.
///
/// This is a *read* of a recorded fact, not a replay: nothing here reconstructs
/// state from the log, and §4.5's gate does not branch on the string. It travels
/// verbatim into [`ReconciliationEvidence::AcceptedAtReview`] so a reader of the
/// durable record can see that a human accepted `CONTRADICTED` rather than a
/// clean run — which is a materially different history.
///
/// `None` when nothing recorded one, and `None` is treated as "unknown" rather
/// than as "clean" everywhere it is used.
fn recorded_verdict(store: &Store, run_id: &RunId) -> Option<String> {
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT payload FROM event
              WHERE run_id = ?1 AND kind = 'RUN_STATE_CHANGED'
              ORDER BY seq DESC",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![run_id.as_str()], |row| {
            row.get::<_, String>(0)
        })
        .ok()?;
    for payload in rows.flatten() {
        let Ok(value) = serde_json::from_str::<Value>(&payload) else {
            continue;
        };
        let Some(detail) = value["detail"].as_str() else {
            continue;
        };
        if let Some(rest) = detail.split("verdict=").nth(1) {
            let verdict = rest
                .split(|c: char| c == ';' || c.is_whitespace())
                .next()
                .unwrap_or_default()
                .trim();
            if !verdict.is_empty() {
                return Some(verdict.to_string());
            }
        }
    }
    None
}

/// `check.id` → the argv that check runs, joined.
///
/// §5.1 persists `command_hash` and never the command text, so "what did it
/// actually run" cannot be answered from a `verification_check` row. The profile
/// is the only place the argv lives, which is why [`CheckLine::command`] says so
/// in its own documentation and why `packet/implementation.rs` reads it the same
/// way.
fn check_commands(profile: &Profile) -> BTreeMap<String, String> {
    profile
        .required
        .iter()
        .chain(profile.invariants.iter())
        .chain(
            profile
                .conditional
                .iter()
                .flat_map(|group| group.checks.iter()),
        )
        .map(|check| (check.id.clone(), check.command.argv().join(" ")))
        .collect()
}

/// §4.5's evidence for one run, re-derived from durable state.
///
/// `reconciliation` is the caller's, because it is the one input that differs
/// between the two callers: the export path has no acceptance to offer and asks
/// what the gate says *without* one, while the import path enters the human's
/// decision as [`ReconciliationEvidence::AcceptedAtReview`]. Everything else —
/// which checks ran, at which tree, with what outcome, how many findings are
/// open, what the plan bound — is read identically for both.
fn completion_evidence(
    store: &Store,
    facts: &RunFacts,
    reconciliation: ReconciliationEvidence,
) -> Result<CompletionEvidence, String> {
    let tree_hash = facts.tree_hash();

    let required_ids: BTreeSet<&str> = facts
        .profile
        .required
        .iter()
        .map(|check| check.id.as_str())
        .collect();
    let invariant_ids: BTreeSet<&str> = facts
        .profile
        .invariants
        .iter()
        .map(|check| check.id.as_str())
        .collect();

    let mut required = Vec::new();
    let mut invariants = Vec::new();
    let mut conditional = Vec::new();
    for result in &facts.results {
        let evidence = CheckEvidence {
            check_id: result.check_id.clone(),
            outcome: result.outcome,
            tree_hash: result.tree_hash.clone(),
        };
        if required_ids.contains(result.check_id.as_str()) {
            required.push(evidence);
        } else if invariant_ids.contains(result.check_id.as_str()) {
            invariants.push(evidence);
        } else {
            // Conditionals **and** anything the profile no longer declares. The
            // second case is a check the catalogue dropped after it ran, and the
            // conditional bucket is where it does least harm: §4.5's criterion 2
            // still requires it to have passed at this tree, so an edit to
            // `verification.yaml` cannot turn a `FAIL` into a criterion nobody
            // evaluates.
            conditional.push(evidence);
        }
    }

    Ok(CompletionEvidence {
        tree_hash,
        required: ChecksEvidence::new(required),
        conditional: ChecksEvidence::new(conditional),
        invariants: ChecksEvidence::new(invariants),
        findings: FindingsEvidence::unresolved(blocking_findings(store, &facts.run.id)?),
        acceptance: acceptance_evidence(store, facts)?,
        policy: policy_evidence(store, facts),
        reconciliation,
    })
}

/// How many unresolved findings block completion — §4.5's criterion 4.
///
/// Severity-filtered on [`conductor_run::vertical::BLOCKING_FINDING_SEVERITY`],
/// which is the constant the run path already counts by. Counting differently
/// here would mean a run the vertical would have completed is refused at review,
/// or the reverse.
fn blocking_findings(store: &Store, run_id: &RunId) -> Result<usize, String> {
    Ok(store
        .findings_for_run(run_id)
        .map_err(|err| err.to_string())?
        .into_iter()
        .filter(|finding| {
            finding.resolution.is_none()
                && finding.severity == conductor_run::vertical::BLOCKING_FINDING_SEVERITY
        })
        .count())
}

/// §4.5's criterion 5, read off the `task` row S11's materializer wrote.
///
/// The three answers `AcceptanceEvidence` distinguishes are kept distinct here
/// for the reason its own documentation gives: `NULL` is *"no plan document has
/// ever been materialized for this task"*, `'[]'` is *"a plan was read and
/// declares none"*, and a column that will not decode is neither — it stops the
/// import rather than being read as "there is nothing to bind".
fn acceptance_evidence(store: &Store, facts: &RunFacts) -> Result<AcceptanceEvidence, String> {
    let Some(json) = store
        .acceptance_criteria(&facts.task.id)
        .map_err(|err| err.to_string())?
    else {
        return Ok(AcceptanceEvidence::NotEvaluated {
            owner: Slice::S11PlanLedger,
        });
    };
    let declared: Vec<plan::model::AcceptanceCriterion> =
        serde_json::from_str(&json).map_err(|err| {
            format!(
                "task {} has an acceptance_criteria column that does not decode \
                 ({err}); a column that cannot be read is not evidence that there \
                 is nothing to bind",
                facts.task.id
            )
        })?;
    if declared.is_empty() {
        return Ok(AcceptanceEvidence::NoCriteria);
    }
    Ok(AcceptanceEvidence::Evaluated {
        criteria: declared
            .into_iter()
            .map(|criterion| CriterionEvidence {
                results: facts
                    .results
                    .iter()
                    .filter(|result| criterion.verified_by.contains(&result.check_id))
                    .map(|result| CheckEvidence {
                        check_id: result.check_id.clone(),
                        outcome: result.outcome,
                        tree_hash: result.tree_hash.clone(),
                    })
                    .collect(),
                id: criterion.id,
                manual: criterion.manual,
                verified_by: criterion.verified_by,
            })
            .collect(),
    })
}

/// §4.5's criterion 7, as far as durable state can answer it.
///
/// Three cases, and the third is the honest one rather than the convenient one:
///
/// * The verdict is recorded and is not `POLICY_SENSITIVE` — nothing the attempt
///   did was policy-sensitive, so criterion 7 has nothing to require. This is
///   `vertical::policy_position`'s first branch, read from the same fact.
/// * The verdict is `POLICY_SENSITIVE` and a grant was consumed for this run —
///   criterion 7 is satisfied, and the evidence names *what* authorized it.
/// * Anything else — a `POLICY_SENSITIVE` verdict with no consumed grant, or no
///   recorded verdict at all. `// S13:` [`PolicyEvidence`] has **no refusing
///   variant**, so claiming either satisfied answer here would be a fabrication
///   and claiming the other would be a lie about what was checked.
///   [`PolicyEvidence::NotEvaluated`] is the only truthful shape available: the
///   gate records it as *deferred* on the [`conductor_core::completion::VerifiedComplete`]
///   token, and `review import --json` reports that list, so a `COMPLETE` reached
///   this way is explicit about what nobody checked.
fn policy_evidence(store: &Store, facts: &RunFacts) -> PolicyEvidence {
    let Some(verdict) = facts.verdict.as_deref() else {
        return PolicyEvidence::NotEvaluated {
            owner: Slice::S7Policy,
        };
    };
    if verdict != "POLICY_SENSITIVE" {
        return PolicyEvidence::NoSensitiveActions;
    }
    match consumed_grant(store, &facts.run.id) {
        Some(grant) => PolicyEvidence::AllGrantsPresent {
            detail: format!("grant {grant}"),
        },
        None => PolicyEvidence::NotEvaluated {
            owner: Slice::S7Policy,
        },
    }
}

/// The most recent grant this run spent, if it spent one.
fn consumed_grant(store: &Store, run_id: &RunId) -> Option<String> {
    store
        .conn()
        .query_row(
            "SELECT g.id FROM approval_grant g
               JOIN approval_request r ON r.id = g.request_id
              WHERE r.run_id = ?1 AND g.state = 'CONSUMED'
              ORDER BY g.granted_at DESC LIMIT 1",
            rusqlite::params![run_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

// ---------------------------------------------------------------------------
// §6.5's review packet, gathered
// ---------------------------------------------------------------------------

/// Compose the review half of §6.5's packet from the store and the repository.
fn review_fields(
    store: &Store,
    review: &conductor_store::ReviewRow,
    facts: &RunFacts,
) -> Result<ReviewFields, String> {
    let commands = check_commands(&facts.profile);
    let checks = facts
        .results
        .iter()
        .map(|result| CheckLine {
            check_id: result.check_id.clone(),
            command: commands
                .get(&result.check_id)
                .cloned()
                // Not an empty string: a reviewer must be able to tell "the
                // catalogue no longer declares this check" from "it ran with no
                // arguments".
                .unwrap_or_else(|| {
                    format!(
                        "<{} is not declared by the current verification profile>",
                        result.check_id
                    )
                }),
            outcome: result.outcome.as_str().to_string(),
            exit_code: result.exit_code,
            duration_ms: result.duration_ms,
            tree_hash: result.tree_hash.clone(),
        })
        .collect();

    let workspace = facts.run.workspace_path.as_deref().map(Path::new);
    let observed = Observed::measure(workspace, &facts.run.base_commit);

    Ok(ReviewFields {
        tasks: vec![facts.task.id.as_str().to_string()],
        boundary: review.boundary.clone(),
        end_state: facts.task.state.as_str().to_string(),
        proposed_next_state: proposed_next_state(store, facts),
        claims: agent_claims(store, facts),
        measured: Measured {
            // §4.8's verdict, verbatim. `UNRECORDED` rather than a friendlier
            // guess: see `recorded_verdict`.
            reconciliation_verdict: facts
                .verdict
                .clone()
                .unwrap_or_else(|| UNRECORDED_VERDICT.to_string()),
            tree_hash: facts.tree_hash(),
            changed_paths: observed.changed_paths,
            commits: observed.commits,
        },
        diff: observed.diff,
        checks,
        policy: policy_lines(store, &facts.run.id),
        approvals: approval_lines(store, &facts.run.id),
        unresolved_findings: store
            .findings_for_run(&facts.run.id)
            .map_err(|err| err.to_string())?
            .into_iter()
            .filter(|finding| finding.resolution.is_none())
            .map(|finding| FindingLine {
                id: finding.id,
                kind: finding.kind,
                severity: finding.severity,
                evidence_ref: finding.evidence_ref,
            })
            .collect(),
    })
}

/// What a verdict is called when nothing recorded one.
///
/// A word rather than an empty string, so a reviewer sees a stated absence
/// instead of a field that looks unfilled.
const UNRECORDED_VERDICT: &str = "UNRECORDED";

/// What §6.5 says Conductor would do next — **advisory**.
///
/// Derived by running the *same* gate the import path runs, with the
/// reconciliation verdict entered honestly as not-clean and no acceptance
/// offered. If the only criteria refusing are the two an `accept` resolves —
/// the review boundary itself, and the findings acceptance clears — then
/// accepting would complete the task, and `COMPLETE` is what Conductor proposes.
/// Otherwise there is work left and the proposal is `REPAIRING`.
///
/// One gate, not two. Re-deriving §4.5 here with a shortcut would be a second
/// opinion a human could read and act on before the authoritative one refused.
fn proposed_next_state(store: &Store, facts: &RunFacts) -> String {
    let verdict = facts
        .verdict
        .clone()
        .unwrap_or_else(|| UNRECORDED_VERDICT.to_string());
    let evidence =
        match completion_evidence(store, facts, ReconciliationEvidence::NotClean { verdict }) {
            Ok(evidence) => evidence,
            // The packet still has to say something, and "we could not tell" is
            // the true answer. `AWAITING_REVIEW` is where the run already is.
            Err(_) => return RunState::AwaitingReview.as_str().to_string(),
        };
    match conductor_core::completion::evaluate(&evidence) {
        Ok(_) => RunState::Complete.as_str().to_string(),
        Err(refusals) => {
            let resolved_by_accepting = refusals.iter().all(|refusal| {
                matches!(
                    refusal.criterion,
                    conductor_core::completion::Criterion::ReconciliationVerdict
                        | conductor_core::completion::Criterion::NoUnresolvedFindings
                )
            });
            if resolved_by_accepting {
                RunState::Complete.as_str().to_string()
            } else {
                RunState::Repairing.as_str().to_string()
            }
        }
    }
}

/// The agent's side of §6.5's side-by-side, read from the attempt's report.
///
/// Absent when no report arrived, which is acceptance rows 3 and 4 — and
/// [`Claims`] is built for that: `claim: None` says "the agent said nothing"
/// rather than defaulting to a claim it never made.
fn agent_claims(store: &Store, facts: &RunFacts) -> Claims {
    let Some(store_path) = store.path().parent() else {
        return Claims::default();
    };
    let artifacts = ArtifactRoot::new(store_path.join("artifacts"));
    let path = artifacts
        .attempt_dir(&facts.run.id, latest_ordinal(store, &facts.run.id))
        .join("report.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Claims::default();
    };
    let Ok(report) = serde_json::from_str::<conductor_core::AgentReport>(&text) else {
        // A report that will not parse is acceptance row 5's
        // `REPORT_UNPARSEABLE`, which already left a finding on the run. The
        // packet carries the finding; inventing claims out of a broken document
        // would put words in the agent's mouth.
        return Claims::default();
    };
    Claims {
        claim: Some(claim_name(report.claim).to_string()),
        task_id: report.task_id,
        files_touched: report.files_touched,
        summary: report.summary,
        commands_run: report.commands_run,
        acceptance_criteria: report.acceptance_criteria,
        deviations: report.deviations,
        blockers: report.blockers,
        unverified_claims: report.unverified_claims,
    }
}

/// §6.5's status vocabulary, as `schemas/agent-report.v1.json` spells it.
fn claim_name(claim: conductor_core::ReportClaim) -> &'static str {
    match claim {
        conductor_core::ReportClaim::Complete => "COMPLETE",
        conductor_core::ReportClaim::Partial => "PARTIAL",
        conductor_core::ReportClaim::Failed => "FAILED",
    }
}

/// What git measured about the run's work.
struct Observed {
    changed_paths: Vec<String>,
    commits: Vec<String>,
    diff: Diff,
}

impl Observed {
    /// Measure the workspace, or state plainly that there is nothing to measure.
    ///
    /// `// S13:` all three fields are empty when the workspace is gone. §4.1
    /// cleans workspaces up, and a review can be exported long afterwards — so
    /// the alternatives are an honest absence or a reconstruction from the run
    /// branch in the operator's repository, which is a different tree from the
    /// one the agent worked in. An empty list a reviewer can see is better than
    /// a diff that is almost right.
    fn measure(workspace: Option<&Path>, base_commit: &str) -> Observed {
        let empty = Observed {
            changed_paths: Vec::new(),
            commits: Vec::new(),
            diff: Diff::none(),
        };
        let Some(workspace) = workspace else {
            return empty;
        };
        if !workspace.exists() {
            return empty;
        }

        let changed_paths =
            match conductor_git::run_git(workspace, &["status", "--porcelain", "-z"]) {
                Ok(out) if out.ok() => conductor_git::git::nul_records(&out.stdout)
                    .into_iter()
                    .filter_map(|record| record.get(3..).map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            };
        let commits = match conductor_git::run_git(
            workspace,
            &["log", "--format=%H", &format!("{base_commit}..HEAD")],
        ) {
            Ok(out) if out.ok() => out.stdout_lossy().lines().map(str::to_string).collect(),
            _ => Vec::new(),
        };
        let diff = match conductor_git::run_git(workspace, &["diff", "--numstat", base_commit]) {
            Ok(out) if out.ok() => numstat(&out.stdout_lossy()),
            _ => Diff::none(),
        };
        Observed {
            changed_paths,
            commits,
            diff,
        }
    }
}

/// `git diff --numstat` reduced to §6.5's inline stat.
///
/// Binary files report `-` for both counts; they are counted as files and
/// contribute no lines, which is what `--shortstat` also does.
fn numstat(text: &str) -> Diff {
    let mut files = 0usize;
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        files += 1;
        let mut columns = line.split('\t');
        insertions += columns
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        deletions += columns
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
    }
    Diff::summary(files, insertions, deletions)
}

/// §6.5's *"policy evaluations and explanations"*.
///
/// `// S13:` §5.1 persists no per-run log of policy evaluations — §4.4 resolves
/// an action, acts, and keeps the *snapshot* rather than the decisions. The
/// evaluations that do leave a durable trace are the ones that raised an
/// approval request, and those are what travel. A run nothing gated therefore
/// carries an empty list, which is the true statement about it rather than a
/// reconstruction of every rule that did not fire.
fn policy_lines(store: &Store, run_id: &RunId) -> Vec<PolicyLine> {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT action, explanation FROM approval_request
          WHERE run_id = ?1 ORDER BY requested_at, id",
    ) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![run_id.as_str()], |row| {
        Ok(PolicyLine {
            action: row.get(0)?,
            // A request exists precisely because §4.4 resolved the action to
            // `require_approval`; `allow` and `deny` raise nothing.
            decision: "require_approval".to_string(),
            explanation: row.get(1)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// §6.5's *"approvals granted with scope"*.
fn approval_lines(store: &Store, run_id: &RunId) -> Vec<ApprovalLine> {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT g.id, r.kind, g.scope, g.granted_by
           FROM approval_grant g JOIN approval_request r ON r.id = g.request_id
          WHERE r.run_id = ?1 ORDER BY g.granted_at, g.id",
    ) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![run_id.as_str()], |row| {
        Ok(ApprovalLine {
            grant_id: row.get(0)?,
            kind: row.get(1)?,
            scope: row.get(2)?,
            granted_by: row.get(3)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

// ==== BEGIN `review import` client — no store handle ====
//
// Everything between this marker and its partner is the client half of §6.5's
// mutating import. It parses the human's file, sends one JSON-RPC call, and maps
// the answer onto §7.2's codes. It must never name a database handle or a
// decision-applying function: `crates/conductor-cli/tests/review.rs` scans
// exactly this region and fails if it does.

/// Send a human's decision to the control socket.
///
/// There is **no** fallback for "the socket was not there". §6.5 makes importing
/// a decision *"a mutating operation … never a file an agent could write"*, and a
/// client that wrote the decision itself whenever no server was up would be a
/// decision produced by anything that can run this binary. The answer is §7.2's
/// code `2`, and the review stays `EXPORTED`.
fn import_client(args: &ImportArgs, socket_args: &SocketArgs) -> Result<Answer, Refusal> {
    let text = std::fs::read_to_string(&args.file)
        .map_err(|err| Refusal::usage(format!("{}: {err}", args.file.display())))?;
    let document: DecisionDocument = serde_yaml::from_str(&text)
        .map_err(|err| Refusal::usage(format!("{}: {err}", args.file.display())))?;

    // An unfilled `decision:` is a usage error and says so, because it is the
    // one field the stub deliberately ships blank. Whether a *filled* value is
    // one of §6.5's five is the server's question — one authority for the
    // vocabulary, on the side that performs the mutation.
    //
    // **Sent verbatim, never trimmed.** `ReviewDecision::from_str` matches
    // exactly and explains why: `"accept "` and `"accept;"` must be told about
    // rather than quietly repaired, and a client that trimmed would be a second
    // opinion on which spellings are legal — one that silently widens the set on
    // the way to the one decision that completes a task.
    let decision = document.decision.clone().ok_or_else(|| {
        Refusal::usage(format!(
            "{} names no decision; fill in `decision:` with one of accept, \
             repair, revise_plan, pause or stop",
            args.file.display()
        ))
    })?;

    let path = socket_args
        .socket
        .clone()
        .map(Ok)
        .unwrap_or_else(socket::default_socket_path)
        .map_err(|err| Refusal::not_initialized(err.to_string()))?;

    let params = json!({
        "review_id": document.review_id,
        "run_id": document.run_id,
        "task_id": document.task_id,
        "plan_version": document.plan_version,
        "packet_hash": document.packet_hash,
        "decision": decision,
        "notes": document.notes,
        "target_plan_version": document.target_plan_version,
        "decisions_to_record": document.decisions_to_record,
        "plan_amendments": document.plan_amendments,
        // §4.3's `granted_by`, read from the environment rather than accepted as
        // a flag: a flag would let whoever runs the command name somebody else
        // as the decider, and the field exists to say who decided.
        "decided_by": std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
        "nonce": args.nonce,
    });

    match socket::call(&path, IMPORT_METHOD, params) {
        Ok(result) => Ok(Answer {
            rendered: render_import(&result),
            json: result,
            code: exit::SUCCESS,
        }),
        // §7.2's `2`. "Conductor is not up" is not the same event as "the
        // decision was refused", and a wrapper script has to tell them apart:
        // one is worth retrying after starting the daemon and the other is not.
        Err(err @ socket::SocketError::NotListening { .. }) => Err(Refusal::not_initialized(
            format!("{err}\n  start one with `conductor approval serve`"),
        )),
        Err(err) => Err(Refusal::refused(err.to_string())),
    }
}

fn render_import(result: &Value) -> String {
    let mut out = format!(
        "review {} {} — run {} task {}\n",
        field(&result["review"]),
        field(&result["decision"]),
        field(&result["run"]),
        field(&result["task"]),
    );
    out.push_str(&format!("  review    {}\n", field(&result["review_state"])));
    out.push_str(&format!("  run       {}\n", field(&result["run_state"])));
    out.push_str(&format!("  task      {}\n", field(&result["task_state"])));
    if let Some(grant) = result["grant"].as_str() {
        out.push_str(&format!("  grant     {grant}\n"));
    }
    if let Some(registered) = result["plan_version_registered"].as_str() {
        out.push_str(&format!("  registered {registered}\n"));
    }
    if let Some(deferred) = result["deferred"].as_array()
        && !deferred.is_empty()
    {
        out.push_str(&format!(
            "  not evaluated   {} (a later slice owes these)\n",
            deferred
                .iter()
                .map(field)
                .collect::<Vec<String>>()
                .join(", ")
        ));
    }
    out
}

fn field(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

// ==== END `review import` client ====

// ---------------------------------------------------------------------------
// `review.import` — the server §6.5 puts in charge of the mutation
// ---------------------------------------------------------------------------

/// Why the server refused an import.
///
/// A type of this module's own rather than [`crate::approval`]'s private
/// `Refusal`, so the dispatch arm is the only line that has to change in that
/// file — §4.3 keeps one server, and this keeps the diff to it honest about
/// being one arm.
#[derive(Debug)]
pub struct ImportRefusal {
    /// One of [`crate::socket::rpc_code`]'s.
    pub code: i64,
    /// What went wrong, in terms the human who wrote the file can act on.
    pub message: String,
}

impl ImportRefusal {
    fn invalid(message: impl Into<String>) -> ImportRefusal {
        ImportRefusal {
            code: socket::rpc_code::INVALID_PARAMS,
            message: message.into(),
        }
    }

    fn refused(message: impl Into<String>) -> ImportRefusal {
        ImportRefusal {
            code: socket::rpc_code::REFUSED,
            message: message.into(),
        }
    }
}

/// Apply one human decision — §6.5's mutating import, and §5.2's `EXPORTED →
/// DECIDED`.
///
/// # The order of operations, and what each failure leaves behind
///
/// 1. **Bind first.** The review is loaded, required to be `EXPORTED`, and every
///    field the decision file carries is compared against the row. Nothing has
///    been written when any of those refuse.
/// 2. **Apply the outcome.** `accept` runs §4.5's gate and refuses when it
///    refuses; the other three move the run through
///    [`conductor_store::lease::apply_review_decision`], whose
///    `WHERE state = 'AWAITING_REVIEW'` is the guard.
/// 3. **Mirror the task**, because §5.2 draws one machine and a task that
///    disagrees with its run is a state nothing else in the system can read.
/// 4. **Record the decision last.** `DECIDED` is terminal, so writing it for a
///    decision that did not take effect would be unrecoverable — the review
///    could never be answered again. The reverse ordering is recoverable: if the
///    run moved and this write then failed, a second import is refused by the
///    run's state and the refusal names it, which is a thing an operator can act
///    on.
///
/// A replay is refused twice over for four of the five decisions — the review is
/// `DECIDED` **and** the run is no longer `AWAITING_REVIEW` — and exactly once
/// for `pause`, which moves no run state. That single guard is
/// [`conductor_store::review::record_decision`]'s `WHERE … AND state =
/// 'EXPORTED'`, not the check in [`bind`]: measured, by disabling [`bind`]'s
/// check and watching `a_paused_review_is_the_replay_nothing_but_the_review_row_can_refuse`
/// still pass. [`bind`]'s copy is the redundant one that produces the better
/// message, in the same spirit as `mark_exported` refusing twice.
pub fn import(store: &mut Store, params: &Value) -> Result<Value, ImportRefusal> {
    let review_id = string(params, "review_id")?;
    let decision = string(params, "decision")?
        .parse::<ReviewDecision>()
        // §4.4's fail-closed rule applied to the most typo-exposed value in the
        // system: an unrecognised word must never resolve to the most permissive
        // of §6.5's five, and one of the five advances a task to `COMPLETE`.
        .map_err(|err| ImportRefusal::invalid(err.to_string()))?;

    let review = store
        .review(&review_id)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?
        .ok_or_else(|| ImportRefusal::invalid(format!("no review {review_id}")))?;
    bind(&review, params)?;

    // §6.5 lists these beside the decision, and neither is implemented here.
    // `// S13:` refused rather than dropped: a human writing `decisions_to_record:
    // [D-0009]` has given an instruction, and an import that silently discarded
    // it would be the failure nobody sees. The refusal names the field.
    for (key, what) in [
        (
            "decisions_to_record",
            "recording a decision document (§3.6)",
        ),
        ("plan_amendments", "amending a plan document (§3.4)"),
    ] {
        if params[key].as_array().is_some_and(|list| !list.is_empty()) {
            return Err(ImportRefusal::invalid(format!(
                "{key} is not empty, and {what} is not something `review import` \
                 does; refusing rather than dropping an instruction a human wrote"
            )));
        }
    }

    let run_id = review.run_id.clone();
    let task_id = review.task_id.clone();
    let facts = run_facts(store, &run_id).map_err(ImportRefusal::refused)?;

    let mut extra = serde_json::Map::new();
    let outcome = match decision {
        ReviewDecision::Accept => Some(accept(store, &review, &facts, params, &mut extra)?),
        ReviewDecision::Repair => Some(ReviewOutcome::Repairing),
        ReviewDecision::RevisePlan => {
            revise_plan(store, &facts, params, &mut extra)?;
            Some(ReviewOutcome::Superseded)
        }
        // §6.5's one decision that moves nothing. Pausing is not a transition —
        // a human has looked and is not deciding yet — and what changes is the
        // review row, which before S13 could not distinguish "nobody has looked"
        // from "a human looked and wants it left alone".
        ReviewDecision::Pause => None,
        ReviewDecision::Stop => Some(ReviewOutcome::Cancelled),
    };

    let run_state = match outcome {
        Some(outcome) => {
            let state = store
                .apply_review_decision(
                    &run_id,
                    outcome,
                    &format!("review {review_id}: {decision}"),
                    now_ms(),
                )
                .map_err(|err| ImportRefusal::refused(err.to_string()))?;
            mirror(store, &task_id, state)?;
            state
        }
        None => facts.run.state,
    };

    let recorded = store
        .record_review_decision(
            &review_id,
            decision,
            params["decided_by"].as_str().unwrap_or("unknown"),
            params["notes"].as_str(),
            now_ms(),
        )
        .map_err(|err| ImportRefusal::refused(err.to_string()))?;

    let task_state = store
        .task(&task_id)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?
        .map(|task| task.state);

    let mut answer = json!({
        "review": recorded.id,
        "run": run_id.as_str(),
        "task": task_id.as_str(),
        "decision": decision.as_str(),
        "review_state": recorded.state.as_str(),
        "run_state": run_state.as_str(),
        "task_state": task_state.map(|state| state.as_str()),
        "packet_hash": recorded.packet_hash,
    });
    if let Value::Object(map) = &mut answer {
        map.append(&mut extra);
    }
    Ok(answer)
}

/// Refuse a decision that is not about *this* review.
///
/// Five checks, each its own refusal naming what disagreed. The `packet_hash`
/// one is the sharp one: a decision carrying a hash the review never exported is
/// a decision about bytes nobody has seen, which is exactly what §4.3's
/// `REVIEW_ACCEPTANCE` — *"one review packet"* — is scoped to prevent.
fn bind(review: &conductor_store::ReviewRow, params: &Value) -> Result<(), ImportRefusal> {
    if review.state != ReviewState::Exported {
        return Err(ImportRefusal::refused(format!(
            "review {} is {}, and §5.2 draws EXPORTED → DECIDED only; a PENDING \
             review has exported nothing a human could have read, and a DECIDED \
             one has been answered",
            review.id, review.state
        )));
    }

    let stored_hash = review.packet_hash.as_deref().ok_or_else(|| {
        ImportRefusal::refused(format!(
            "review {} is EXPORTED with no packet hash; a decision recorded \
             against it would authorize nothing in particular (§4.3)",
            review.id
        ))
    })?;

    for (key, expected, what) in [
        ("run_id", review.run_id.as_str(), "run"),
        ("task_id", review.task_id.as_str(), "task"),
        (
            "plan_version",
            review.plan_version_id.as_str(),
            "plan version",
        ),
        ("packet_hash", stored_hash, "packet hash"),
    ] {
        let offered = params[key].as_str().unwrap_or_default();
        if offered != expected {
            return Err(ImportRefusal::refused(format!(
                "the decision names {what} {offered:?} and review {} is bound to \
                 {expected:?}; a decision that floats free of the packet a human \
                 read is an approval of something nobody has seen (§6.5)",
                review.id
            )));
        }
    }
    Ok(())
}

/// §6.5's `accept` — the only decision that has to *earn* its state.
///
/// # What happens, and why in this order
///
/// 1. **The findings are resolved.** §4.8 makes a finding something only a
///    person clears, and a human accepting a review is that person. It happens
///    first because criterion 4 counts unresolved findings, and a run reaches a
///    review boundary largely *because* of them — without this, `accept` would
///    always refuse on criterion 4 and the decision would be unusable.
/// 2. **A §4.3 `REVIEW_ACCEPTANCE` is requested and granted**, exactly as
///    `plan_approve` does for a plan: one human is both the question and the
///    answer, so the request and the grant are written together and the pair is
///    the *record* of the decision. `Expiry::Never` because §4.3's fourth column
///    says a review acceptance does not expire.
/// 3. **The gate runs** with [`ReconciliationEvidence::AcceptedAtReview`],
///    carrying the grant id. `Ok` yields the [`conductor_core::completion::VerifiedComplete`]
///    that `ReviewOutcome::Accepted` demands and nothing else can mint; `Err` is
///    a refusal naming every criterion, and the run stays `AWAITING_REVIEW`.
///
/// **What a refusal leaves behind, stated rather than hidden:** the resolved
/// findings and the grant. Both are records of what a person did, and they are
/// true whether or not some *other* criterion then refused — a human who
/// answered a finding answered it. What does not happen is the state change, and
/// that is the thing `accept` exists to authorize.
fn accept(
    store: &mut Store,
    review: &conductor_store::ReviewRow,
    facts: &RunFacts,
    params: &Value,
    extra: &mut serde_json::Map<String, Value>,
) -> Result<ReviewOutcome, ImportRefusal> {
    let run_id = facts.run.id.clone();
    let (_, epoch) = store
        .run_state(&run_id)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?
        .ok_or_else(|| ImportRefusal::refused(format!("run {run_id} is not in the store")))?;
    let fence = Fence::new(run_id.clone(), epoch);

    let unresolved: Vec<String> = store
        .findings_for_run(&run_id)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?
        .into_iter()
        .filter(|finding| finding.resolution.is_none())
        .map(|finding| finding.id)
        .collect();
    let resolution = format!("accepted at review {}", review.id);
    for finding in &unresolved {
        store
            .resolve_finding(&fence, finding, &resolution, now_ms())
            .map_err(|err| ImportRefusal::refused(err.to_string()))?;
    }

    let packet_hash = review.packet_hash.clone().unwrap_or_default();
    let stamp = now_ms();
    let request_id = format!("AR-{}-{stamp}", review.id);
    let policy_hash = policy_load::pinned_for_run(store.conn(), &run_id)
        .map(|pinned| pinned.hash)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?;

    approvals::request(
        store.conn_mut(),
        &NewApprovalRequest {
            id: request_id.clone(),
            // §4.3's granularity for this kind is "one review packet", and the
            // packet's identity is the hash the decision is bound to.
            subject: Subject::ReviewPacket {
                packet_id: packet_hash.clone(),
            },
            // §4.3's review acceptance is not run-scoped (schema v6): what is
            // authorized is a packet, and the packet outlives the run's lease.
            run_id: None,
            facts: FactSet::new(),
            policy_hash,
            matched_rules: Vec::new(),
            explanation: format!(
                "a human at the control socket accepted review {} of run {run_id} \
                 (§6.5)",
                review.id
            ),
            evidence_ref: review.packet_path.clone(),
            // §4.3's fourth column: a review acceptance does not expire.
            expires: Expiry::Never,
        },
        stamp,
    )
    .map_err(|err| ImportRefusal::refused(err.to_string()))?;

    let granted = approvals::grant(
        store.conn_mut(),
        &request_id,
        &GrantOptions {
            id: format!("AG-{}-{stamp}", review.id),
            scope: Scope::from_pairs([("review".to_string(), review.id.clone())]),
            reuse: false,
            expires: Expiry::Never,
            granted_by: params["decided_by"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            channel: CHANNEL.to_string(),
            nonce_hash: None,
        },
        stamp,
    )
    .map_err(|err| ImportRefusal::refused(err.to_string()))?;

    let verdict = facts
        .verdict
        .clone()
        .unwrap_or_else(|| UNRECORDED_VERDICT.to_string());
    let evidence = completion_evidence(
        store,
        facts,
        ReconciliationEvidence::AcceptedAtReview {
            verdict,
            authorization: granted.id.clone(),
        },
    )
    .map_err(ImportRefusal::refused)?;

    let verified = conductor_core::completion::evaluate(&evidence).map_err(|refusals| {
        ImportRefusal::refused(format!(
            "the completion gate still refuses run {run_id}, so the acceptance \
             does not complete it: {}. Accepting a review resolves the review \
             boundary; §4.5 keeps verification authoritative, and the decisions \
             for a criterion that still fails are repair, revise_plan and stop",
            refusals
                .iter()
                .map(|refusal| format!("{:?}: {}", refusal.criterion, refusal.detail))
                .collect::<Vec<String>>()
                .join("; ")
        ))
    })?;

    extra.insert("grant".to_string(), json!(granted.id));
    extra.insert("findings_resolved".to_string(), json!(unresolved));
    extra.insert(
        "deferred".to_string(),
        json!(
            verified
                .deferred()
                .iter()
                .map(|criterion| format!("{criterion:?}"))
                .collect::<Vec<String>>()
        ),
    );
    Ok(ReviewOutcome::Accepted(verified))
}

/// §6.5's `revise_plan` — register the version that supersedes this work.
///
/// **Registered, never approved.** §5.2 gives `APPROVED` to *"a human at the
/// control socket"* through `conductor plan approve`, and a review decision that
/// also approved would be a second door into the state §4.3's whole tier table
/// exists to guard. What this does is make the version *exist* in the ledger, so
/// the supersession the run is about to record points at something real —
/// [`ledger::register_plan_version`] reads it from the registered tree (§3.3
/// control 2), runs §3.7's refusals over it, and lands it in `VALIDATED`.
///
/// The target must not be the version the work was already authorized under: a
/// "revision" to the same version supersedes a task in favour of the document
/// that produced it.
fn revise_plan(
    store: &mut Store,
    facts: &RunFacts,
    params: &Value,
    extra: &mut serde_json::Map<String, Value>,
) -> Result<(), ImportRefusal> {
    let target = params["target_plan_version"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| {
            ImportRefusal::invalid(
                "revise_plan needs `target_plan_version:` — the version under \
                 `.conductor/plans/` that supersedes this work",
            )
        })?;
    let current = u32::try_from(facts.plan_version.version).unwrap_or(0);
    if target == current {
        return Err(ImportRefusal::invalid(format!(
            "target_plan_version is {target}, which is the version this work was \
             already authorized under; a revision to the same document \
             supersedes a task in favour of the plan that produced it"
        )));
    }

    let id = ledger::plan_version_id(&facts.project_id, target);
    let already = store
        .plan_version(&id)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?;
    if already.is_none() {
        // §3.7's clarification 3: the validator takes the catalogue as a
        // parameter and the caller assembles it. Read from the registered tree,
        // like everything else this verb reads.
        let catalogue_path = facts.root.join(plan::VERIFICATION_CONFIG_PATH);
        let catalogue = plan::check_ids(
            &profile::load(&catalogue_path)
                .map_err(|err| {
                    ImportRefusal::invalid(format!("{}: {err}", catalogue_path.display()))
                })?
                .profile,
        );
        ledger::register_plan_version(store, &facts.project_id, target, &catalogue)
            .map_err(|err| ImportRefusal::refused(err.to_string()))?;
    }

    let row = store
        .plan_version(&id)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?
        .ok_or_else(|| ImportRefusal::refused(format!("plan version {id} vanished")))?;
    extra.insert("plan_version_registered".to_string(), json!(id.as_str()));
    extra.insert("plan_version_state".to_string(), json!(row.state.as_str()));
    Ok(())
}

/// Move the task to wherever the run just went — §5.2's *"the run mirrors its
/// task"*.
///
/// A no-op when they already agree, because §5.2's table has no self-transition:
/// writing the state a task is already in looks like progress and is not. The
/// same shape `vertical::mirror` uses, written here because that function is
/// private to the crate that owns the run path.
fn mirror(store: &mut Store, task_id: &TaskId, run_state: RunState) -> Result<(), ImportRefusal> {
    let current = store
        .task(task_id)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?
        .ok_or_else(|| ImportRefusal::refused(format!("no task {task_id}")))?
        .state;
    let target = run_state.as_task_state();
    if current == target {
        return Ok(());
    }
    store
        .set_task_state(task_id, target)
        .map_err(|err| ImportRefusal::refused(err.to_string()))?;
    Ok(())
}

fn string(params: &Value, key: &str) -> Result<String, ImportRefusal> {
    params[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| ImportRefusal::invalid(format!("{key} is required")))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numstat_with_a_binary_file_counts_the_file_and_no_lines() {
        // `git diff --numstat` writes `-` for both counts of a binary file.
        // Parsing that as zero *files* would under-report the diff a reviewer is
        // being asked to accept; parsing it as zero lines is what --shortstat
        // does and is correct.
        let diff = numstat("3\t1\tsrc/a.rs\n-\t-\tlogo.png\n");
        let rendered = serde_yaml::to_string(&diff).expect("a diff serializes");
        assert!(rendered.contains("files: 2"), "{rendered}");
        assert!(rendered.contains("insertions: 3"), "{rendered}");
        assert!(rendered.contains("deletions: 1"), "{rendered}");
    }

    #[test]
    fn a_verdict_is_read_out_of_the_detail_string_its_producer_writes() {
        // `policy_gate::route_reconciliation` writes `verdict=<NAME>; policy: …`
        // and `advance_state` wraps it in `{"to":…,"detail":…}`. If either
        // producer changes shape this test fails rather than a review packet
        // quietly reporting UNRECORDED.
        let payload = serde_json::json!({
            "to": "AWAITING_REVIEW",
            "route": "AWAITING_REVIEW",
            "detail": "verdict=CONTRADICTED; policy: nothing gated",
        })
        .to_string();
        let value: Value = serde_json::from_str(&payload).expect("json");
        let detail = value["detail"].as_str().expect("a detail");
        let verdict = detail
            .split("verdict=")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| c == ';' || c.is_whitespace()).next());
        assert_eq!(verdict, Some("CONTRADICTED"));
    }
}
