//! `conductor init` — master plan §7.1 and §3.1.
//!
//! > ```text
//! > conductor init                       # scaffold .conductor/ in the current repo
//! > ```
//!
//! §7.1 also records what this command absorbed: *"`project add/list/inspect`
//! (folded into `init`/`status` — the project is the repo you are in)"*. So
//! `init` is the whole of "tell Conductor about this repository", and §3.1 draws
//! exactly what it must leave behind:
//!
//! ```text
//! .conductor/
//! ├── project.yaml            identity, adapter, scope defaults, review cadence,
//! │                           execution_requirements
//! ├── policy.yaml             project policy rules
//! ├── verification.yaml       check profiles, toolchain fingerprint commands
//! └── plans/
//!     └── v1/
//!         └── plan.yaml       milestones · slices · tasks · acceptance criteria
//! ```
//!
//! # It writes files and touches no store
//!
//! §3.1 makes the git half authoritative and the SQLite half *disposable* —
//! §3.5's recovery path is "re-register the project → read `.conductor/`". A
//! scaffold that required a healthy database would make the disposable half a
//! precondition for creating the authoritative one, and it would fail in a
//! repository with no commits, because §5.1's `repo_identity` is
//! `blake3(first_commit ‖ …)` and there is no first commit yet. Registration
//! therefore happens the first time something actually needs a row —
//! `conductor plan approve` — and `init` stays a file operation.
//!
//! # `decisions/` is not created
//!
//! §3.1's tree shows it, and nothing reads it yet: `crate::plan`'s core has no
//! reader for `.conductor/decisions/*.md`. CLAUDE.md's rule applies — "an empty
//! skeleton creates false architectural commitments" — so the directory arrives
//! with the slice that reads it.
//!
//! # Why the one acceptance criterion it writes is `manual: true`
//!
//! §3.7 refuses "any acceptance criterion not bound to at least one check", and
//! its escape hatch is `manual: true`, "which forces a review boundary". A
//! scaffold cannot know what proves anything in a repository it has just met:
//! writing `verified_by: [unit-tests]` and a `cargo test` check into a
//! repository that is not Rust produces a plan that validates and a check that
//! fails for the wrong reason. So the starter criterion is the one §3.7 says to
//! write when no machine can judge — honest, refused by nothing, and the first
//! thing an author replaces.
//!
//! # Exit codes (§7.2)
//!
//! `0` the scaffold was written · `1` `.conductor/` already exists, or a write
//! failed · `70` internal error.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Args;
use serde_json::json;

use crate::exit;

/// Where the layout goes, relative to the repository root — §3.1.
pub const CONDUCTOR_DIR: &str = ".conductor";

/// The version `init` scaffolds. §3.1's tree shows `v3` because the plan it
/// draws has been revised twice; a new repository starts at one.
const FIRST_VERSION: u32 = 1;

/// `conductor init`
#[derive(Debug, Args)]
pub struct InitArgs {
    /// The repository. Defaults to the working directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Scaffold §3.1's layout, or refuse.
pub fn run(args: &InitArgs) -> ExitCode {
    let root = match &args.repo {
        Some(path) => path.clone(),
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(err) => {
                eprintln!("the working directory cannot be read: {err}");
                return ExitCode::from(exit::FAILURE);
            }
        },
    };

    match scaffold(&root) {
        Ok(written) => {
            if args.json {
                let report = json!({
                    "root": root.display().to_string(),
                    "created": written,
                    "plan_version": FIRST_VERSION,
                });
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("internal error: {err}");
                        return ExitCode::from(exit::INTERNAL);
                    }
                }
            } else {
                println!(
                    "conductor: scaffolded {} in {}",
                    CONDUCTOR_DIR,
                    root.display()
                );
                for relative in &written {
                    println!("  {relative}");
                }
                println!(
                    "  next: edit the plan, then `conductor plan validate` and \
                     `conductor plan approve {FIRST_VERSION}`"
                );
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(exit::FAILURE)
        }
    }
}

/// Write the layout, all of it or none of it.
///
/// Built in a private sibling directory and moved into place with `rename(2)`,
/// the way [`crate::socket`] publishes the control socket and for a related
/// reason: a half-written `.conductor/` is a project whose `plan validate`
/// refuses for a reason that has nothing to do with the plan. `rename` also
/// closes the gap between the "does it already exist?" check and the write —
/// the check refuses any existing entry, and the rename can only land on a name
/// that is free or an empty directory.
fn scaffold(root: &Path) -> Result<Vec<String>, String> {
    let target = root.join(CONDUCTOR_DIR);
    // `symlink_metadata`, not `exists`: a dangling symlink at `.conductor` is
    // something a human put there, and following it to "nothing is here" would
    // scaffold through it.
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!(
            "{} already exists; refusing to overwrite it. §3.1 makes \
             `.conductor/` authoritative for \"what we agreed to do, and what we \
             are allowed to do\" — an approved plan, the project policy and the \
             check catalogue all live there, and a scaffold that replaced them \
             would delete a human decision with one mistyped command.",
            target.display()
        ));
    }
    if !root.is_dir() {
        return Err(format!(
            "{} is not a directory, so there is no repository to scaffold",
            root.display()
        ));
    }

    let identity = project_id(root);
    let branch = default_branch(root);
    let files: Vec<(String, String)> = vec![
        ("project.yaml".to_string(), project_yaml(&identity, &branch)),
        ("policy.yaml".to_string(), POLICY_YAML.to_string()),
        (
            "verification.yaml".to_string(),
            VERIFICATION_YAML.to_string(),
        ),
        (
            format!("plans/v{FIRST_VERSION}/plan.yaml"),
            plan_yaml(&identity),
        ),
    ];

    let staging = root.join(format!(".conductor.{}.staging", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let build = || -> std::io::Result<()> {
        for (relative, text) in &files {
            let path = staging.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)?;
        }
        Ok(())
    };
    if let Err(err) = build() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("could not write the scaffold: {err}"));
    }
    if let Err(err) = std::fs::rename(&staging, &target) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "could not move the scaffold into {}: {err}",
            target.display()
        ));
    }

    Ok(files
        .iter()
        .map(|(relative, _)| format!("{CONDUCTOR_DIR}/{relative}"))
        .collect())
}

/// §5.1's `p-<short>`, derived from the directory the repository is in.
///
/// Derived rather than prompted for, because §7.1 gives `init` no arguments and
/// §5.1's id is *"constant for the life of the repository"* — a value a human
/// edits once in a file they can diff is better than one they type once into a
/// prompt they cannot re-read. Anything that is not `[a-z0-9]` becomes `-`, so
/// the id is safe in a branch name (§4.1's `conductor/<run-id>`), a file path
/// and a commit trailer (§3.4).
fn project_id(root: &Path) -> String {
    let name = root
        .canonicalize()
        .ok()
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut slug = String::new();
    for ch in name.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "p-project".to_string()
    } else {
        format!("p-{slug}")
    }
}

/// The branch a completed run integrates into.
///
/// Read from the repository rather than assumed, because
/// `project.default_branch` decides where work lands: writing `main` into a
/// repository whose branch is `master` is an invented fact with consequences,
/// and §3.1 makes this file authoritative. `main` is the fallback for a
/// directory that is not a git repository yet — a value the author can see and
/// change, not a silent guess about a repository that does not exist.
fn default_branch(root: &Path) -> String {
    conductor_git::run_git(root, &["symbolic-ref", "--short", "HEAD"])
        .ok()
        .filter(|out| out.ok())
        .map(|out| out.stdout_trimmed())
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "main".to_string())
}

fn project_yaml(id: &str, default_branch: &str) -> String {
    format!(
        "\
# `.conductor/project.yaml` — master plan §3.1: identity, adapter, scope
# defaults, review cadence, execution_requirements.
project:
  id: {id}
  default_branch: {default_branch}
  adapter: codex
  scope_defaults:
    # What a task may write when it declares no scope of its own.
    allowed_globs: []
    # §3.3 rejects any change to `.conductor/` at reconciliation whatever this
    # file says. The entry is here so a reader sees the rule, not because this
    # is what enforces it.
    forbidden_globs: [\".conductor/**\"]
"
    )
}

/// §4.4's project document, with no rules.
///
/// Empty rather than opinionated. §4.4 makes the locked global rules a ceiling
/// and project rules something that tightens beneath it, so an empty project
/// policy is not "anything goes" — it is "this repository adds nothing to the
/// ceiling", which is the only thing `init` can truthfully say about a
/// repository it has just met.
const POLICY_YAML: &str = "\
# `.conductor/policy.yaml` — master plan §4.4. Project rules are first-class and
# tighten the locked global ceiling; they cannot loosen it, and an action no
# rule names fails closed.
#
#   rules:
#     - id: project.no-force-push
#       action: git.push.force
#       effect: deny
policy:
  rules: []
";

/// §4.5's check catalogue, with no checks.
///
/// Empty for the same reason the starter criterion is `manual: true`: the
/// commands that prove things in this repository are not knowable from outside
/// it, and a catalogue containing `cargo test` in a repository that is not Rust
/// is worse than an empty one — it produces a check that fails for a reason
/// that has nothing to do with the work.
const VERIFICATION_YAML: &str = "\
# `.conductor/verification.yaml` — master plan §4.5. Every check id a plan's
# `verified_by` names must be declared here; §3.7 refuses an id that resolves to
# nothing, because a criterion that looks bound and is bound to nothing is worse
# than one that is openly unbound.
#
#   required:
#     - id: unit-tests
#       command: cargo test
#       timeout_seconds: 1200
verification:
  toolchain_fingerprint: []
  required: []
  invariants: []
";

fn plan_yaml(id: &str) -> String {
    format!(
        "\
# `.conductor/plans/v{FIRST_VERSION}/plan.yaml` — master plan §3.1 and §3.6. A plan is a data
# structure: milestones containing slices containing tasks. Prose belongs in
# `objective:` and `rationale:` and nowhere else.
#
# §3.6's ids — M-01, S-05, T-0012 — are assigned once and never reused.
plan:
  id: {id}
  version: {FIRST_VERSION}
  objective: \"Say what this plan is for.\"
  milestones:
    - id: M-01
      title: \"First milestone\"
      slices:
        - id: S-01
          title: \"First slice\"
          tasks:
            - id: T-0001
              objective: \"Say what this task must produce.\"
              rationale: \"Say why it is worth doing.\"
              depends_on: []
              scope:
                allowed_globs: []
                forbidden_globs: [\".conductor/**\"]
              verification_profile: default
              attempt_budget: 3
              acceptance_criteria:
                - id: AC-1
                  statement: \"Say what must be true, then bind it to a check id from verification.yaml.\"
                  # §3.7's escape hatch, and the only honest thing a scaffold can
                  # write: it cannot know what proves anything here. Replace this
                  # with `verified_by: [<check-id>]` as soon as there is a check —
                  # an unbound criterion is how a task reaches COMPLETE on an
                  # agent's word, and `manual: true` forces a review boundary
                  # instead.
                  manual: true
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_id_is_derived_from_the_directory_and_is_safe_everywhere_it_appears() {
        // §3.4 puts the id in a commit trailer, §4.1 puts it near a branch name,
        // and §5.1 makes it a primary key. A space or a slash in any of those is
        // a different kind of bug in each place.
        let dir = tempfile::tempdir().expect("tempdir");
        let awkward = dir.path().join("My Repo (v2)!");
        std::fs::create_dir_all(&awkward).expect("mkdir");
        assert_eq!(project_id(&awkward), "p-my-repo-v2");
    }

    #[test]
    fn a_directory_whose_name_survives_nothing_still_yields_a_usable_id() {
        // §5.1's id is a primary key and `ProjectId` refuses a blank value, so
        // "there was nothing left after sanitising" must not produce `p-`.
        let dir = tempfile::tempdir().expect("tempdir");
        let unnameable = dir.path().join("!!!");
        std::fs::create_dir_all(&unnameable).expect("mkdir");
        assert_eq!(project_id(&unnameable), "p-project");
    }

    #[test]
    fn the_scaffold_leaves_nothing_behind_when_it_refuses() {
        // The staging directory is an implementation detail; a leftover would be
        // a second `.conductor`-shaped tree in the repository that nothing ever
        // cleans up, and that git would offer to commit.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(CONDUCTOR_DIR)).expect("mkdir");
        scaffold(dir.path()).expect_err("an existing .conductor/ is refused");
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != CONDUCTOR_DIR)
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }
}
