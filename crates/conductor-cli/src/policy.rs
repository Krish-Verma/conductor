//! `conductor policy explain <action>` — master plan §7.1.
//!
//! > ```text
//! > conductor policy explain <action>    # why was this denied — the 2 a.m. command
//! > ```
//!
//! §4.4 line 633 sets the contract: *"prints: action · resolved effect · the
//! ceiling that applied · every rule that matched **and every rule considered
//! that did not, with the reason** · facts and their sources · policy hash · any
//! exception with scope and expiry. Negative results are what people debug."*
//!
//! # Two sources of policy, and which one the command uses
//!
//! Without `--run`, it explains the policy **on disk right now** — the question
//! "what would happen if I did this?".
//!
//! With `--run`, it explains the snapshot the run is **pinned** to (§4.4: "a run
//! evaluates against its snapshot for its entire life"). That is the one the 2
//! a.m. question actually needs: a run that was blocked was blocked by its
//! snapshot, not by whatever the file says after someone edited it.
//!
//! # Exit code
//!
//! Always `0` on success, including for a `deny`. §7.2 reserves `4` for "policy
//! denied", but `explain` is an *informational* command: it succeeded in
//! explaining. Returning `4` would make `conductor policy explain` unusable in a
//! `set -e` script for the exact purpose it exists for. The slice that
//! *enforces* a denial (S9) is the one that should return `4`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use conductor_core::RunId;
use conductor_run::policy::evaluate::{Decision, Request, evaluate};
use conductor_run::policy::model::{Action, Fact, FactSet, Origin, ResolvedPolicy};
use conductor_run::policy::{explain, load};
use conductor_store::Store;

use crate::exit;
use crate::task::StoreArgs;

/// `conductor policy …`
#[derive(Debug, Subcommand)]
pub enum PolicyCommand {
    /// Why an action resolves the way it does.
    Explain(ExplainArgs),
}

/// `conductor policy explain <action>`
#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// The action, e.g. `dependency.add.runtime`. An unrecognised name is not
    /// an error — §4.4 denies it, and the explanation says so.
    pub action: String,

    /// The repository. Defaults to the working directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,

    /// Override the global policy path.
    #[arg(long)]
    pub global: Option<PathBuf>,

    /// Ignore the global policy entirely.
    #[arg(long)]
    pub no_global: bool,

    /// An additional per-task policy document.
    #[arg(long)]
    pub task_policy: Option<PathBuf>,

    /// Explain the policy this run is **pinned** to, not the one on disk.
    #[arg(long)]
    pub run: Option<String>,

    /// Scope entries, `key=value`. Repeatable.
    #[arg(long = "scope", value_name = "KEY=VALUE")]
    pub scope: Vec<String>,

    /// A deterministic fact, `key=value`. Repeatable.
    #[arg(long = "fact", value_name = "KEY=VALUE")]
    pub facts: Vec<String>,

    /// A model-assisted fact, `key=value`. Repeatable. §4.4 caps any `deny`
    /// resting on one at `require_approval`.
    #[arg(long = "model-fact", value_name = "KEY=VALUE")]
    pub model_facts: Vec<String>,

    /// A human-asserted fact, `key=value`. Repeatable.
    #[arg(long = "human-fact", value_name = "KEY=VALUE")]
    pub human_facts: Vec<String>,
}

/// Run one `policy` subcommand.
pub fn run(command: &PolicyCommand, shared: &StoreArgs) -> ExitCode {
    match command {
        PolicyCommand::Explain(args) => match explain_action(args, shared) {
            Ok(decision) => {
                if shared.json {
                    match serde_json::to_string_pretty(&decision) {
                        Ok(json) => println!("{json}"),
                        Err(err) => {
                            eprintln!("internal error: {err}");
                            return ExitCode::from(exit::INTERNAL);
                        }
                    }
                } else {
                    print!("{}", explain::render(&decision));
                }
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("{message}");
                ExitCode::from(exit::FAILURE)
            }
        },
    }
}

fn explain_action(args: &ExplainArgs, shared: &StoreArgs) -> Result<Decision, String> {
    let mut context = BTreeMap::new();
    for entry in &args.scope {
        let (key, value) = split_pair(entry, "--scope")?;
        context.insert(key, value);
    }

    let policy = match &args.run {
        Some(run) => {
            let run_id = RunId::new(run.clone()).map_err(|e| e.to_string())?;
            let path = match &shared.store {
                Some(path) => path.clone(),
                None => Store::default_path().map_err(|e| e.to_string())?,
            };
            let store = Store::open_existing(&path).map_err(|e| e.to_string())?;
            let pinned = load::pinned_for_run(store.conn(), &run_id).map_err(|e| e.to_string())?;
            // A run's scope is part of its identity: without this, an exception
            // scoped to the run would silently never apply and the explanation
            // would be about a different question than the one asked.
            context
                .entry("run".to_string())
                .or_insert_with(|| run_id.as_str().to_string());
            pinned.policy
        }
        None => resolve_from_disk(args)?,
    };

    let mut facts = FactSet::new();
    for (entries, build) in [
        (
            &args.facts,
            Fact::deterministic as fn(String, String) -> Fact,
        ),
        (&args.model_facts, Fact::model_assisted),
        (&args.human_facts, Fact::human),
    ] {
        for entry in entries {
            let (key, value) = split_pair(entry, "--fact")?;
            facts.push(build(key, value));
        }
    }

    let request = Request {
        action: Action::parse(&args.action),
        facts,
        context,
        now_ms: now_ms(),
    };
    Ok(evaluate(&policy, &request))
}

/// Load the global and project documents that actually exist.
///
/// A file that is simply not there is **not** an error — an operator with no
/// global policy has no global policy. A file that is there but unreadable or
/// malformed is a hard error, because that is the case where carrying on would
/// mean explaining a policy that is not the one in force.
fn resolve_from_disk(args: &ExplainArgs) -> Result<ResolvedPolicy, String> {
    let repo = args
        .repo
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let global = if args.no_global {
        None
    } else {
        args.global.clone().or_else(load::global_policy_path)
    };
    let project = repo.join(load::PROJECT_POLICY_PATH);

    load::resolve_documents(
        load_if_present(global.as_deref(), Origin::Global)?,
        load_if_present(Some(&project), Origin::Project)?,
        load_if_present(args.task_policy.as_deref(), Origin::Task)?,
    )
    .map_err(|e| e.to_string())
}

/// Load a policy document if the file is there.
///
/// Shared with [`crate::approval`], which needs the same rule when it records
/// the policy hash §3.1's `APPROVED` sidecar carries: absent is not an error,
/// present-and-unreadable is. Two copies of that rule would be two places for
/// "carry on without it" to creep into one of them.
pub(crate) fn load_if_present(
    path: Option<&Path>,
    origin: Origin,
) -> Result<Option<conductor_run::policy::model::PolicyDocument>, String> {
    let Some(path) = path else { return Ok(None) };
    if !path.exists() {
        return Ok(None);
    }
    load::load_document(path, origin)
        .map(Some)
        .map_err(|e| e.to_string())
}

fn split_pair(entry: &str, flag: &str) -> Result<(String, String), String> {
    entry
        .split_once('=')
        .map(|(key, value)| (key.trim().to_string(), value.to_string()))
        .filter(|(key, _)| !key.is_empty())
        .ok_or_else(|| format!("{flag} expects KEY=VALUE, got {entry:?}"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}
