//! `conductor plan validate | approve` — master plan §7.1, §7.2, §3.1 and §3.7.
//!
//! > ```text
//! > conductor plan validate [--version N]
//! > conductor plan approve <version>     # human-only, socket-only
//! > ```
//!
//! # This module cannot approve anything, and that is the design
//!
//! §5.2 gives `APPROVED` to *"a human at the control socket"*, and §4.3 spends a
//! whole table explaining why that is a property of the **execution mode**
//! rather than of a check somebody remembered to write: *"A `0600` unix socket
//! does not distinguish a human from a same-user subprocess, and removing an
//! environment variable is obscurity."* The consequence for this file is blunt.
//! `plan approve` here builds a JSON-RPC call and sends it down
//! [`crate::socket`]; the verb that writes rows lives behind that socket, in
//! [`crate::approval`], which is the one server §4.3 allows. There is no
//! fallback for "the socket was not there" — that answer is §7.2's code `2`, and
//! the plan stays unapproved.
//!
//! Nothing here opens a database. That is deliberate and it is asserted:
//! `crates/conductor-cli/tests/plan_approve.rs` fails if this module names a
//! database handle at all. §4.3 asks for *"read-only verbs only"*, and S8's
//! answer — recorded in `crates/conductor-run/tests/layering.rs` — was that a
//! surface with no verbs beats a surface with a read-only subset, because a
//! subset is an argument somebody can win one call at a time.
//!
//! # `validate` is the other half, and it is pure
//!
//! §3.7's refusals are a function of three files in `.conductor/` and nothing
//! else, so `plan validate` reads the layout, hands the check catalogue to the
//! validator as §3.7's clarification 3 requires — *"the validator takes the
//! catalogue as a parameter and the caller assembles it"* — and prints what
//! comes back. It touches no store and needs no daemon: a human writing a plan
//! must be able to check it before Conductor is running.
//!
//! # Exit codes (§7.2)
//!
//! ```text
//! 0   the plan validates, or the approval was granted
//! 1   §3.7 refused the plan, or the server refused the approval
//! 2   the `.conductor/` layout could not be read, or no control socket is up
//! 64  usage error
//! 70  internal error
//! ```
//!
//! The line between `1` and `2` is worth stating once, because a wrapper script
//! depends on it: `2` is *"no project / not initialized / store unhealthy"* —
//! its third clause shows the slot is about Conductor's substrate being
//! unusable, not merely about an absent file — so a `.conductor/` that is
//! missing and one that cannot be parsed both land there. `1` is kept for "the
//! command ran and the answer was no".

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use conductor_run::plan::{self, PlanDefect, ValidatedPlan, ValidationReport};
use conductor_run::verify::profile;
use serde_json::{Value, json};

use crate::approval::SocketArgs;
use crate::exit;
use crate::socket;

/// The RPC method the server answers. Named once, here and in
/// [`crate::approval`]'s dispatch, so a rename cannot leave the client calling
/// a verb nobody serves.
const APPROVE_METHOD: &str = "plan.approve";

/// `conductor plan …`
#[derive(Debug, Subcommand)]
pub enum PlanCommand {
    /// Run §3.7's refusals over a plan version.
    Validate(ValidateArgs),
    /// Make a plan version authoritative. Human-only, socket-only (§7.1).
    Approve(ApproveArgs),
}

/// Options shared by every `plan` subcommand.
#[derive(Debug, Args)]
pub struct PlanArgs {
    /// The repository. Defaults to the working directory.
    #[arg(long, global = true)]
    pub repo: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long, global = true)]
    pub json: bool,
}

/// `conductor plan validate [--version N]`
#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Which version under `.conductor/plans/`. The newest one by default.
    #[arg(long)]
    pub version: Option<u32>,
}

/// `conductor plan approve <version>`
#[derive(Debug, Args)]
pub struct ApproveArgs {
    /// `N` from `.conductor/plans/vN/`.
    pub version: u32,
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
    /// §7.2's `2` — the `.conductor/` layout is not usable.
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
}

/// Run one `plan` subcommand.
pub fn run(command: &PlanCommand, shared: &PlanArgs, socket_args: &SocketArgs) -> ExitCode {
    let outcome = match command {
        PlanCommand::Validate(args) => validate(args, shared),
        PlanCommand::Approve(args) => approve(args, shared, socket_args),
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

/// A command's answer: what a script reads, what a human reads, and the code.
struct Answer {
    json: Value,
    rendered: String,
    code: u8,
}

// ---------------------------------------------------------------------------
// `plan validate`
// ---------------------------------------------------------------------------

/// The three files §3.7 needs, loaded from one repository root.
struct Layout {
    root: PathBuf,
    /// The check ids `verification.yaml` defines — §3.7's catalogue.
    catalogue: BTreeSet<String>,
}

/// Read the `.conductor/` layout, or refuse.
///
/// Every failure here is §7.2's `2`, including a file that exists and does not
/// parse. **Fail closed**: an unreadable `verification.yaml` must never be read
/// as "this project defines no checks", because that reading turns every bound
/// criterion in the plan into a dangling reference and reports a catalogue
/// problem as a plan problem — the author would go and edit the wrong file.
fn layout(shared: &PlanArgs) -> Result<Layout, Refusal> {
    let root = match &shared.repo {
        Some(path) => path.clone(),
        None => std::env::current_dir()
            .map_err(|err| Refusal::not_initialized(format!("no working directory: {err}")))?,
    };

    // Loaded and thrown away: `plan validate` does not need the project's
    // adapter or its scope defaults, but a `.conductor/` without a readable
    // `project.yaml` is not a project (§3.1), and saying so here is what makes
    // "there is no plan" distinguishable from "your plan is wrong".
    plan::project::load(&root).map_err(|err| Refusal::not_initialized(err.to_string()))?;

    let catalogue_path = root.join(VERIFICATION_CONFIG_PATH);
    // The path is prepended rather than left to the error, because
    // `ProfileError` names it only when the *file* could not be read: a parse
    // failure says what is wrong and not where, and "did not find expected key"
    // with no filename sends an author to whichever of §3.1's four files they
    // guess at first.
    let loaded = profile::load(&catalogue_path)
        .map_err(|err| Refusal::not_initialized(format!("{}: {err}", catalogue_path.display())))?;
    Ok(Layout {
        root,
        catalogue: plan::check_ids(&loaded.profile),
    })
}

/// Where the check catalogue lives, relative to the repository root — §3.1.
const VERIFICATION_CONFIG_PATH: &str = ".conductor/verification.yaml";

fn validate(args: &ValidateArgs, shared: &PlanArgs) -> Result<Answer, Refusal> {
    let layout = layout(shared)?;
    let version = match args.version {
        Some(version) => version,
        None => newest_version(&layout.root)?,
    };
    let relative = plan::plan_path(version);
    let path = layout.root.join(&relative);
    let text = std::fs::read_to_string(&path)
        .map_err(|err| Refusal::not_initialized(format!("plan file {}: {err}", path.display())))?;
    let document = plan::parse(&text).map_err(|err| Refusal::not_initialized(err.to_string()))?;
    let hash = plan::content_hash(&text)
        .map_err(|err| Refusal::not_initialized(err.to_string()))?
        .as_str()
        .to_string();

    // Not one of §3.7's six refusals, and checked anyway. §3.4's
    // `Conductor-Plan: v3@blake3:…` trailer, §5.1's `UNIQUE(project_id, version)`
    // and supersession each need one answer to "which version is this?", and the
    // ledger already refuses a document that disagrees with its own directory.
    // Leaving it out here would mean `plan validate` says yes to a file that
    // `plan approve` then refuses, which is the worst possible place to discover
    // it.
    if document.version != version {
        let message = format!(
            "{relative} declares version {} but sits in the directory for version \
             {version}; §3.4's trailer, §5.1's UNIQUE(project_id, version) and \
             supersession each need one answer to \"which version is this?\"",
            document.version
        );
        return Ok(Answer {
            json: json!({
                "version": version,
                "plan": relative,
                "content_hash": hash,
                "valid": false,
                "defects": [{
                    "kind": "version_mismatch",
                    "subject": document.version.to_string(),
                    "message": message,
                }],
                "manual_criteria": [],
                "requires_human_review": false,
            }),
            rendered: format!(
                "plan validate: refused, 1 defect(s)\n  - [version_mismatch] {message}\n"
            ),
            code: exit::FAILURE,
        });
    }

    match plan::validate(&document, &layout.catalogue) {
        Ok(validated) => Ok(accepted(version, &relative, &hash, &validated)),
        Err(report) => Ok(rejected(version, &relative, &hash, &report)),
    }
}

fn accepted(version: u32, relative: &str, hash: &str, validated: &ValidatedPlan) -> Answer {
    let manual: Vec<Value> = validated
        .manual_criteria()
        .iter()
        .map(|criterion| {
            json!({
                "task": criterion.task,
                "criterion": criterion.criterion,
                "statement": criterion.statement,
            })
        })
        .collect();
    let tasks = validated.tasks().count();
    let mut rendered = format!(
        "plan validate v{version}: ok — {relative}\n  content hash {hash}\n  \
         {tasks} task(s)\n"
    );
    for criterion in validated.manual_criteria() {
        // Reported rather than counted: §3.7's escape hatch "forces a review
        // boundary", and a human who cannot see which criteria carry it cannot
        // know how much of the plan a machine will never finish.
        rendered.push_str(&format!(
            "  manual {} {} — {}\n",
            criterion.task, criterion.criterion, criterion.statement
        ));
    }
    Answer {
        json: json!({
            "version": version,
            "plan": relative,
            "content_hash": hash,
            "valid": true,
            "defects": [],
            "manual_criteria": manual,
            "requires_human_review": validated.requires_human_review(),
        }),
        rendered,
        code: exit::SUCCESS,
    }
}

fn rejected(version: u32, relative: &str, hash: &str, report: &ValidationReport) -> Answer {
    let defects: Vec<Value> = report.defects().iter().map(defect_json).collect();
    Answer {
        json: json!({
            "version": version,
            "plan": relative,
            "content_hash": hash,
            "valid": false,
            "defects": defects,
            "manual_criteria": [],
            "requires_human_review": false,
        }),
        // The report's own `Display` already names the rule and the id for every
        // defect. Re-rendering it here would be a second wording of §3.7 that
        // could drift from the first.
        rendered: format!("{report}"),
        code: exit::FAILURE,
    }
}

/// One defect, as a script reads it.
///
/// `kind` and `subject` come from [`PlanDefect`]'s own accessors, which exist
/// so a caller can assert *which* rule fired rather than match on prose. The
/// prose travels too, because the prose is what tells a human what to do.
fn defect_json(defect: &PlanDefect) -> Value {
    json!({
        "kind": defect.kind(),
        "subject": defect.subject(),
        "message": defect.to_string(),
    })
}

/// The newest `vN` under `.conductor/plans/` that holds a plan document.
///
/// Newest rather than "the one the project points at", because §3.1's layout is
/// the only index there is: a project does not name a current version anywhere,
/// and §5.2 makes "which one is authoritative" a question about the store's
/// `APPROVED` row rather than about the directory. What an author validating a
/// plan means by "the plan" is the one they are writing, which is the last one.
fn newest_version(root: &Path) -> Result<u32, Refusal> {
    let plans = root.join(plan::PLANS_DIR);
    let entries = std::fs::read_dir(&plans).map_err(|err| {
        Refusal::not_initialized(format!("plan directory {}: {err}", plans.display()))
    })?;
    let mut newest = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(number) = name.strip_prefix('v').and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if entry.path().join("plan.yaml").is_file() {
            newest = Some(newest.map_or(number, |current: u32| current.max(number)));
        }
    }
    newest.ok_or_else(|| {
        Refusal::not_initialized(format!(
            "no plan version under {}; §3.1's layout is `plans/vN/plan.yaml`",
            plans.display()
        ))
    })
}

// ---------------------------------------------------------------------------
// `plan approve`
// ---------------------------------------------------------------------------

/// Ask the control socket to make a plan version authoritative.
///
/// The project file is read here **only** to refuse early and to name the
/// project in the call. §3.3's control 2 — *"Conductor reads plan approval only
/// from the registered repository's working tree"* — is enforced on the server,
/// which resolves the tree out of the store and compares it against the root
/// offered here rather than trusting it. A client-side check would be a
/// convention; the server's is a refusal.
fn approve(
    args: &ApproveArgs,
    shared: &PlanArgs,
    socket_args: &SocketArgs,
) -> Result<Answer, Refusal> {
    let root = match &shared.repo {
        Some(path) => path.clone(),
        None => std::env::current_dir()
            .map_err(|err| Refusal::not_initialized(format!("no working directory: {err}")))?,
    };
    let root = root
        .canonicalize()
        .map_err(|err| Refusal::not_initialized(format!("{}: {err}", root.display())))?;
    let project =
        plan::project::load(&root).map_err(|err| Refusal::not_initialized(err.to_string()))?;

    let path = socket_args
        .socket
        .clone()
        .map(Ok)
        .unwrap_or_else(socket::default_socket_path)
        .map_err(|err| Refusal::not_initialized(err.to_string()))?;

    let params = json!({
        "repo_root": root.display().to_string(),
        "project_id": project.id,
        "version": args.version,
        // §4.3's `granted_by`. Read from the environment rather than accepted as
        // a flag: a flag would let whoever runs the command name somebody else
        // as the approver, and the field's whole purpose is to say who decided.
        "granted_by": std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
        "nonce": args.nonce,
    });

    match socket::call(&path, APPROVE_METHOD, params) {
        Ok(result) => Ok(Answer {
            rendered: format!(
                "approved plan v{} — {}\n  content hash {}\n  approver {}\n  \
                 sidecar {}\n",
                text(&result["version"]),
                text(&result["plan_version"]),
                text(&result["content_hash"]),
                text(&result["approver"]),
                text(&result["sidecar"]),
            ),
            json: result,
            code: exit::SUCCESS,
        }),
        // §7.2's `2`. A missing control socket is "Conductor is not up", which a
        // wrapper script must be able to tell from "the approval was refused" —
        // one is worth retrying after starting the daemon and the other is not.
        Err(err @ socket::SocketError::NotListening { .. }) => Err(Refusal::not_initialized(
            format!("{err}\n  start one with `conductor approval serve`"),
        )),
        Err(err) => Err(Refusal::refused(err.to_string())),
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_version_is_the_one_with_a_plan_document_in_it() {
        // A `vN` directory with no `plan.yaml` is a directory somebody created
        // and has not written yet. Treating it as the newest version would make
        // `plan validate` answer "there is no plan file" about a version the
        // author was not asking about.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for (version, write_plan) in [(1, true), (2, true), (3, false)] {
            let path = root.join(format!("{}/v{version}", plan::PLANS_DIR));
            std::fs::create_dir_all(&path).expect("mkdir");
            if write_plan {
                std::fs::write(path.join("plan.yaml"), "plan:\n").expect("write");
            }
        }
        assert_eq!(newest_version(root).expect("a version"), 2);
    }

    #[test]
    fn a_repository_with_no_plans_directory_is_not_initialized_rather_than_invalid() {
        // §7.2's `2` against §7.2's `1`. "There is no plan" and "your plan is
        // wrong" are different events for the script that has to react.
        let dir = tempfile::tempdir().expect("tempdir");
        let refusal = newest_version(dir.path()).expect_err("refused");
        assert_eq!(refusal.code, exit::NOT_INITIALIZED);
        assert!(refusal.message.contains("plans"), "{}", refusal.message);
    }
}
