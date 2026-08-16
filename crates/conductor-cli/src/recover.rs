//! `conductor recover` — master plan §7.1, §7.2, §3.2, §3.5.
//!
//! > ```text
//! > conductor recover
//! > ```
//!
//! # What this command is for
//!
//! §3.2 makes one promise about the split in §3.1 and says an acceptance test
//! enforces it: *"deleting `conductor.db` loses no plan, no decision, no policy,
//! and no verification definition."* A promise that a file can be deleted is
//! only as good as the command that puts it back, and §7.1 gives that command a
//! slot in the 13-command surface. This is it.
//!
//! It runs §3.5's recovery path over `.conductor/`: re-register the project,
//! read every plan version, restore the approvals that left an `APPROVED`
//! receipt, sync the decisions, and rebuild the task list from the newest
//! approved plan. [`conductor_run::plan::reconstruct`] is where the order and
//! the reasoning live; this module is the process boundary.
//!
//! # `--scan` is not here yet, and is not a flag that does nothing
//!
//! §7.1 spells the command `conductor recover [--scan]`, and §3.5's path has two
//! more clauses this command does not implement: *"scan `workspaces/` for
//! `.conductor-run.json` descriptors and reconcile each against git → read commit
//! trailers"*. Those are judgements about live execution state, they need the
//! reconciler, and S14 owns startup reconciliation across projects. The flag is
//! therefore **absent** rather than accepted-and-ignored: a flag that parses and
//! does nothing is the failure mode CLAUDE.md's "no knobs that do nothing" rule
//! exists to prevent, and an unknown-argument error is an honest answer.
//!
//! # Exit codes (§7.2)
//!
//! ```text
//! 0   project truth was rebuilt
//! 2   the `.conductor/` layout could not be read, or the store is unusable
//! 1   the rebuild ran and refused — a receipt that does not match its document
//! 70  internal error
//! ```
//!
//! A receipt that disagrees with its plan is `1`, not `2`: the command ran, read
//! both halves and reached a verdict. That is §3.3's *"execution halts — it is
//! never resynced"*, surfacing as a refusal a human must resolve rather than as
//! a substrate problem.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args;
use conductor_core::PlanVersionState;
use conductor_run::plan::reconstruct::{self, ReconstructError};
use conductor_store::Store;
use serde_json::json;

use crate::exit;

/// `conductor recover`
#[derive(Debug, Args)]
pub struct RecoverArgs {
    /// The repository. Defaults to the working directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Path to `conductor.db`. Defaults to §3.1's location.
    #[arg(long)]
    pub store: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    /// Wall-clock milliseconds to stamp on rows this rebuild creates. Testing
    /// seam; defaults to now.
    #[arg(long, hide = true)]
    pub now_ms: Option<i64>,
}

/// Run `conductor recover`.
pub fn run(args: &RecoverArgs) -> ExitCode {
    let root = match &args.repo {
        Some(path) => path.clone(),
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(err) => {
                eprintln!("no working directory: {err}");
                return ExitCode::from(exit::NOT_INITIALIZED);
            }
        },
    };

    let store_path = match &args.store {
        Some(path) => path.clone(),
        None => match Store::default_path() {
            Ok(path) => path,
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::from(exit::NOT_INITIALIZED);
            }
        },
    };
    if let Some(parent) = store_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("cannot create {}: {err}", parent.display());
        return ExitCode::from(exit::NOT_INITIALIZED);
    }
    // `open_or_create`, because the case this command exists for is a store
    // that is not there.
    let mut store = match Store::open_or_create(&store_path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("store at {}: {err}", store_path.display());
            return ExitCode::from(exit::NOT_INITIALIZED);
        }
    };

    let now_ms = args.now_ms.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
    });

    let rebuilt = match reconstruct::reconstruct(&mut store, &root, now_ms) {
        Ok(rebuilt) => rebuilt,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(exit_code_for(&err));
        }
    };

    let versions: Vec<_> = rebuilt
        .versions
        .iter()
        .map(|v| {
            json!({
                "version": v.version,
                "plan_version_id": v.id.as_str(),
                "state": v.state.as_str(),
                "approval_restored": v.approval.is_some(),
            })
        })
        .collect();
    let tasks: Vec<&str> = rebuilt
        .tasks
        .as_ref()
        .map(|m| m.created.iter().map(|t| t.id.as_str()).collect())
        .unwrap_or_default();
    let decisions: Vec<&str> = rebuilt.decisions.iter().map(|d| d.id.as_str()).collect();

    let report = json!({
        "project": rebuilt.project.id.as_str(),
        "root": rebuilt.project.root_path,
        "store": store_path.display().to_string(),
        "versions": versions,
        "approved_version": rebuilt.approved_version(),
        "tasks_rebuilt": tasks,
        "decisions": decisions,
    });

    if args.json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("internal error: {err}");
                return ExitCode::from(exit::INTERNAL);
            }
        }
    } else {
        print!("{}", render(&rebuilt, &store_path));
    }
    ExitCode::from(exit::SUCCESS)
}

/// §7.2's line between `2` and `1`, drawn the way [`crate::plan`] draws it.
///
/// `2` is *"no project / not initialized / store unhealthy"* — its third clause
/// shows the slot is about Conductor's substrate being unusable, so a
/// `.conductor/` that is missing **and** one that is present and unparseable
/// both land there. `1` is kept for "the command ran, read both halves and the
/// answer was no" — a plan §3.7 refuses, and a receipt that does not match its
/// document.
///
/// The distinction is not cosmetic: §7.2 exists so a wrapper script can tell
/// "this machine has nothing to recover" from "this repository's approval and
/// its plan disagree", and only the second is a thing a human must adjudicate.
fn exit_code_for(err: &ReconstructError) -> u8 {
    use conductor_run::plan::LedgerError;
    match err {
        // The layout could not be read as a project.
        ReconstructError::Io { .. } | ReconstructError::Verification { .. } => {
            exit::NOT_INITIALIZED
        }
        ReconstructError::Decision(_) => exit::NOT_INITIALIZED,
        ReconstructError::Ledger(ledger) => match ledger {
            LedgerError::Project(_)
            | LedgerError::Plan(_)
            | LedgerError::Io { .. }
            | LedgerError::Git(_)
            | LedgerError::Store(_)
            | LedgerError::UnknownProject { .. } => exit::NOT_INITIALIZED,
            // The rebuild ran and refused: §3.7 said no, or the receipt and the
            // document disagree (§3.3's "execution halts — it is never
            // resynced").
            _ => exit::FAILURE,
        },
        ReconstructError::Materialize(_) => exit::FAILURE,
    }
}

fn render(rebuilt: &reconstruct::Reconstruction, store_path: &std::path::Path) -> String {
    let mut out = String::new();
    out.push_str("project truth rebuilt from .conductor/ (§3.5)\n\n");
    out.push_str(&format!("  project             {}\n", rebuilt.project.id));
    out.push_str(&format!(
        "  repository          {}\n",
        rebuilt.project.root_path
    ));
    out.push_str(&format!("  store               {}\n", store_path.display()));
    out.push_str("\nplan versions\n");
    if rebuilt.versions.is_empty() {
        out.push_str("  (none under .conductor/plans/)\n");
    }
    for version in &rebuilt.versions {
        let receipt = if version.approval.is_some() {
            "approval restored from APPROVED receipt"
        } else if version.state == PlanVersionState::Approved {
            "already approved"
        } else {
            "no receipt"
        };
        out.push_str(&format!(
            "  v{:<3}              {:<18} {receipt}\n",
            version.version,
            version.state.as_str()
        ));
    }
    out.push_str("\ndecisions\n");
    if rebuilt.decisions.is_empty() {
        out.push_str("  (none)\n");
    }
    for decision in &rebuilt.decisions {
        out.push_str(&format!("  {:<18} {}\n", decision.id, decision.status));
    }
    out.push_str("\ntask list\n");
    match &rebuilt.tasks {
        Some(materialization) if !materialization.created.is_empty() => {
            for task in &materialization.created {
                out.push_str(&format!("  {} created\n", task.id));
            }
        }
        Some(_) => out.push_str("  (every task the approved plan declares already had a row)\n"),
        None => out.push_str("  (no approved plan, so no task list to rebuild)\n"),
    }
    out.push_str(
        "\nnot rebuilt, by design (§3.5): run and attempt history, timings, the\n\
         event journal, the verification cache, pending approval requests, lease\n\
         state.\n",
    );
    out
}
