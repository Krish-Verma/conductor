//! `conductor init` and `conductor plan validate` — master plan §7.1, §7.2,
//! §3.1 and §3.7.
//!
//! # What these tests are for
//!
//! §7.1's thirteen commands open with `conductor init` — *"scaffold
//! `.conductor/` in the current repo"* — and §7.1 folds `project add/list/inspect`
//! into it. §3.1 draws the layout it must produce. The one property that ties
//! the two commands together is the one asserted first here: **a scaffold that
//! writes a plan its own validator refuses is worse than no scaffold**, because
//! the first thing it teaches a new operator is that Conductor disagrees with
//! itself.
//!
//! # Exit codes are §7.2's, not invented here
//!
//! ```text
//! 0   the plan validates, or the scaffold was written
//! 1   §3.7 refused the plan, or `.conductor/` already exists
//! 2   the `.conductor/` layout could not be read at all
//! 64  usage error
//! ```
//!
//! `2` covers *"no project / not initialized"*, and its third clause — "store
//! unhealthy" — is what settles the ambiguous case: the slot is about
//! Conductor's substrate being unusable, not merely about a file being absent.
//! So a `.conductor/` that is missing **and** one that cannot be parsed both
//! land on `2`, and `1` is kept for "the command ran and the answer was no".

use std::path::Path;
use std::process::{Command, Output};

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

// ---------------------------------------------------------------------------
// driving the binary
// ---------------------------------------------------------------------------

fn run(args: &[&str]) -> Output {
    Command::new(CONDUCTOR)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap_or_else(|| {
        panic!(
            "no exit code; stderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// stdout and stderr together.
///
/// A refusal must *name* the thing it refused, and asserting on the union means
/// the test is about the message rather than about which stream this slice
/// happened to choose for it.
fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not json ({e}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn arg(path: &Path) -> String {
    path.to_str().expect("utf8 path").to_string()
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

// ---------------------------------------------------------------------------
// §3.1 + §3.7 — the scaffold and the validator must agree
// ---------------------------------------------------------------------------

#[test]
fn init_writes_a_layout_that_plan_validate_then_accepts() {
    // POSITIVE CONTROL for everything below, and the acceptance criterion the
    // brief states: a validator that refused everything would pass every
    // refusal test in this file and fail this one, and a scaffold that emitted
    // an invalid plan would fail it too. End to end, two processes, no shared
    // in-process state.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let init = run(&["init", "--repo", &arg(root), "--json"]);
    assert_eq!(code(&init), 0, "init must succeed: {}", said(&init));

    // §3.1's tree, exactly.
    for relative in [
        ".conductor/project.yaml",
        ".conductor/policy.yaml",
        ".conductor/verification.yaml",
        ".conductor/plans/v1/plan.yaml",
    ] {
        assert!(
            root.join(relative).is_file(),
            "§3.1's layout is missing {relative}; init wrote {:?}",
            said(&init)
        );
    }

    let validate = run(&["plan", "validate", "--repo", &arg(root), "--json"]);
    assert_eq!(
        code(&validate),
        0,
        "the scaffold must validate: {}",
        said(&validate)
    );
    let report = json(&validate);
    assert_eq!(report["valid"], serde_json::json!(true));
    assert_eq!(report["version"], serde_json::json!(1));
    assert_eq!(
        report["defects"],
        serde_json::json!([]),
        "a clean plan reports no defects"
    );
    assert!(
        report["content_hash"]
            .as_str()
            .is_some_and(|hash| hash.starts_with("blake3:")),
        "§3.6's content hash belongs in the machine-readable answer: {report}"
    );
}

#[test]
fn init_refuses_to_overwrite_an_existing_conductor_directory() {
    // §3.1's `.conductor/` is authoritative for "what we agreed to do, and what
    // we are allowed to do". A scaffold that overwrote it would delete an
    // approved plan and a policy with one mistyped command, and §3.2's whole
    // argument for keeping plans in git is that they must survive things.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write(root, ".conductor/project.yaml", "project: {id: p-mine}\n");

    let out = run(&["init", "--repo", &arg(root)]);
    assert_ne!(code(&out), 0, "clobbering must not be a success");
    assert!(
        said(&out).contains(".conductor"),
        "the refusal must name what is in the way: {}",
        said(&out)
    );

    // The refusal is only worth anything if it left the file alone.
    assert_eq!(
        std::fs::read_to_string(root.join(".conductor/project.yaml")).expect("still there"),
        "project: {id: p-mine}\n",
        "init overwrote the file it said it refused to overwrite"
    );
}

// ---------------------------------------------------------------------------
// §3.7 — the refusal has to name the id
// ---------------------------------------------------------------------------

#[test]
fn plan_validate_names_the_defect_and_the_id_it_is_about() {
    // §3.7's most important refusal, through the CLI. `PlanDefect`'s own doc
    // gives the reason the *name* matters: "A refusal that says only 'the plan
    // is invalid' makes a human diff their plan against a specification; a
    // refusal that names `T-0002` and `AC-3` makes them open one file at one
    // line."
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    assert_eq!(
        code(&run(&["init", "--repo", &arg(root)])),
        0,
        "the fixture starts from a scaffold that validates"
    );
    // Exactly one defect introduced: a criterion bound to nothing and not
    // declared `manual`. §3.7: "an unbound criterion is the mechanism by which
    // a task reaches `COMPLETE` on an agent's word".
    write(root, ".conductor/plans/v1/plan.yaml", UNBOUND_CRITERION);

    let out = run(&["plan", "validate", "--repo", &arg(root)]);
    assert_eq!(
        code(&out),
        1,
        "§3.7 refusing a plan is §7.2's `1`, not a crash and not a success"
    );
    let message = said(&out);
    for needle in ["unbound_criterion", "T-0001", "AC-2"] {
        assert!(
            message.contains(needle),
            "the refusal must name {needle:?}; it said:\n{message}"
        );
    }

    // The same refusal, machine-readable, naming the same subject — §7.1's
    // "`--json` on every command" is not a second opinion.
    let out = run(&["plan", "validate", "--repo", &arg(root), "--json"]);
    assert_eq!(code(&out), 1);
    let report = json(&out);
    assert_eq!(report["valid"], serde_json::json!(false));
    let defects = report["defects"].as_array().expect("a defect list");
    assert!(
        defects
            .iter()
            .any(|defect| { defect["kind"] == "unbound_criterion" && defect["subject"] == "AC-2" }),
        "the json must carry the rule and the id it fired on: {report}"
    );
}

/// The scaffold's plan with a second criterion that binds to nothing.
///
/// Written out rather than patched so the fixture says what it is: one defect,
/// and the id the refusal has to quote is `AC-2` on `T-0001`.
const UNBOUND_CRITERION: &str = r#"
plan:
  id: p-fixture
  version: 1
  objective: "One criterion nobody can check."
  milestones:
    - id: M-01
      title: "First milestone"
      slices:
        - id: S-01
          title: "First slice"
          tasks:
            - id: T-0001
              objective: "Describe the first task."
              acceptance_criteria:
                - id: AC-1
                  statement: "A criterion a person judges."
                  manual: true
                - id: AC-2
                  statement: "The thing works correctly."
"#;

// ---------------------------------------------------------------------------
// §7.2 — the codes these commands can actually produce
// ---------------------------------------------------------------------------

#[test]
fn plan_validate_in_a_directory_with_no_conductor_layout_is_code_2() {
    // §7.2's `2` is "no project / not initialized". A directory nobody ran
    // `init` in is exactly that, and it must be distinguishable from `1` — a
    // wrapper script has to tell "your plan is wrong" from "there is no plan".
    let dir = tempfile::tempdir().expect("tempdir");
    let out = run(&["plan", "validate", "--repo", &arg(dir.path())]);
    assert_eq!(code(&out), 2, "said: {}", said(&out));
    assert!(
        said(&out).contains("project.yaml"),
        "the refusal must name the file that is missing: {}",
        said(&out)
    );
}

#[test]
fn a_conductor_layout_that_cannot_be_parsed_is_code_2_and_not_a_silent_default() {
    // Fail closed. A `verification.yaml` that does not parse must never be read
    // as "this project defines no checks" — that reading turns §3.7's "every
    // acceptance criterion binds to at least one check" into a rule about an
    // empty catalogue, and every bound criterion in the plan would suddenly
    // look like a dangling reference.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    assert_eq!(code(&run(&["init", "--repo", &arg(root)])), 0);
    write(root, ".conductor/verification.yaml", ":\n  not: [valid\n");

    let out = run(&["plan", "validate", "--repo", &arg(root)]);
    assert_eq!(
        code(&out),
        2,
        "an unreadable catalogue is not a plan defect: {}",
        said(&out)
    );
    assert!(
        said(&out).contains("verification.yaml"),
        "the refusal must name the file it could not read: {}",
        said(&out)
    );
}

#[test]
fn plan_validate_can_be_asked_for_a_version_that_is_not_the_latest() {
    // §7.1 spells the flag: `conductor plan validate [--version N]`. Without it
    // the command answers about whichever version the directory happens to hold
    // most of, which is not a question anyone asked.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    assert_eq!(code(&run(&["init", "--repo", &arg(root)])), 0);
    // v2 exists and is broken; v1 is the scaffold and is not.
    write(root, ".conductor/plans/v2/plan.yaml", UNBOUND_CRITERION_V2);

    let latest = run(&["plan", "validate", "--repo", &arg(root)]);
    assert_eq!(
        code(&latest),
        1,
        "with no --version the newest version is the subject: {}",
        said(&latest)
    );
    let pinned = run(&["plan", "validate", "--repo", &arg(root), "--version", "1"]);
    assert_eq!(
        code(&pinned),
        0,
        "--version 1 must answer about v1: {}",
        said(&pinned)
    );

    // And a version that is not there is "not initialized", not "invalid".
    let absent = run(&["plan", "validate", "--repo", &arg(root), "--version", "9"]);
    assert_eq!(code(&absent), 2, "said: {}", said(&absent));
}

const UNBOUND_CRITERION_V2: &str = r#"
plan:
  id: p-fixture
  version: 2
  objective: "One criterion nobody can check."
  milestones:
    - id: M-01
      title: "First milestone"
      slices:
        - id: S-01
          title: "First slice"
          tasks:
            - id: T-0001
              objective: "Describe the first task."
              acceptance_criteria:
                - id: AC-2
                  statement: "The thing works correctly."
"#;

#[test]
fn a_plan_verb_that_does_not_exist_is_a_usage_error_and_not_a_failure() {
    // §7.2's `64` is `EX_USAGE`. It must not collapse into `1`: a script that
    // mistyped a verb and a script whose plan is invalid need different
    // reactions, and clap already knows which is which.
    let out = run(&["plan", "bless"]);
    assert_eq!(code(&out), 64, "said: {}", said(&out));

    let out = run(&["plan", "approve"]);
    assert_eq!(
        code(&out),
        64,
        "§7.1 spells `plan approve <version>`; the version is not optional: {}",
        said(&out)
    );
}
