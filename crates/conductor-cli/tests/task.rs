//! `conductor task run | show | list` — master plan §7.1, with §7.2's exit
//! codes.
//!
//! Exit code **3** is the one that earns its own test: §7.2 gives it a dedicated
//! slot because "Conductor stopped and needs a human" is the most common
//! non-success outcome and "must be distinguishable from failure by a wrapper
//! script". A test that only checked "non-zero" would not notice the day it
//! became 1.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

/// The verification profile the fixture task names.
const PROFILE: &str = r#"
verification:
  toolchain_fingerprint:
    - ["/bin/echo", "toolchain-v1"]
  required:
    - id: unit-tests
      command: ["/bin/sh", "-c", "echo ok"]
      timeout_seconds: 60
"#;

/// A profile whose only required check fails — §7.2's exit code 5.
const FAILING_PROFILE: &str = r#"
verification:
  toolchain_fingerprint:
    - ["/bin/echo", "toolchain-v1"]
  required:
    - id: unit-tests
      command: ["/bin/sh", "-c", "echo boom; exit 1"]
      timeout_seconds: 60
"#;

/// S5's minimal task spec — the file Part 8 asks for, and the whole of it.
const SPEC: &str = r#"
id: T-0012
objective: Add a greeting helper to the library.
scope:
  - "src/**"
verification_profile: .conductor/verification.yaml
attempt_budget: 3
"#;

/// An agent that does the work and reports it.
const SCENARIO: &str = r#"{"id":"cli-success","steps":[
  {"step":"emit","kind":"agent.started","detail":"cli"},
  {"step":"write_file","path":"src/added.rs","contents":"pub fn added() -> u32 { 1 }\n"},
  {"step":"checkpoint","name":"after-edits"},
  {"step":"report_on_stdout","claim":"COMPLETE","files_touched":["src/added.rs"],
   "summary":"added a function"},
  {"step":"exit","code":0}]}"#;

struct Fixture {
    dir: tempfile::TempDir,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mkdir");
        std::fs::create_dir_all(repo.join(".conductor")).expect("mkdir");
        std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> u32 { 0 }\n").expect("write");
        std::fs::write(repo.join(".conductor/task.yaml"), SPEC).expect("write");
        std::fs::write(repo.join(".conductor/verification.yaml"), PROFILE).expect("write");

        git(&repo, &["init", "-q", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["config", "user.email", "fixture@localhost"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "initial"]);

        std::fs::write(root.join("scenario.json"), SCENARIO).expect("write");
        Fixture { dir, repo }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canonicalize")
    }

    fn store(&self) -> String {
        self.root().join("conductor.db").display().to_string()
    }

    fn fake_agent(&self) -> String {
        // The fake agent from this build, located relative to the test binary.
        let test_exe = std::env::current_exe().expect("current_exe");
        let target = test_exe
            .parent()
            .and_then(Path::parent)
            .expect("target dir");
        let path = target.join("conductor-fake-agent");
        assert!(
            path.exists(),
            "the fake agent is missing at {}; `cargo test --all` builds it",
            path.display()
        );
        path.display().to_string()
    }

    /// `conductor task run T-0012 --json`, with the fixture's plumbing.
    fn task_run(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "task".to_string(),
            "run".to_string(),
            "T-0012".to_string(),
            "--json".to_string(),
            "--repo".to_string(),
            self.repo.display().to_string(),
            "--store".to_string(),
            self.store(),
            "--adapter".to_string(),
            "fake".to_string(),
            "--agent-binary".to_string(),
            self.fake_agent(),
            "--scenario".to_string(),
            self.root().join("scenario.json").display().to_string(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_string()));
        run(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn task(&self, args: &[&str]) -> Output {
        let mut all: Vec<String> = vec!["task".to_string()];
        all.extend(args.iter().map(|s| (*s).to_string()));
        all.push("--store".to_string());
        all.push(self.store());
        run(&all.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(CONDUCTOR)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
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

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

// ---------------------------------------------------------------------------
// task run
// ---------------------------------------------------------------------------

#[test]
fn a_successful_task_exits_zero_and_reports_its_commit() {
    let f = Fixture::new();
    let out = f.task_run(&[]);

    assert_eq!(
        out.status.code(),
        Some(0),
        "success is exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json(&out);
    assert_eq!(report["task"], "T-0012");
    assert_eq!(report["state"], "COMPLETE");
    assert_eq!(report["outcome"], "COMPLETE");
    let sha = report["commit"]["sha"].as_str().expect("a commit sha");
    assert_eq!(sha.len(), 40, "a real commit id, not a placeholder");

    // …and the repository agrees, which is the only authority that counts.
    let reference = report["integrated"]["reference"]
        .as_str()
        .expect("a ref name");
    assert_eq!(git_out(&f.repo, &["rev-parse", reference]), sha);
    // The user's own branch is untouched (§4.1: never auto-merged).
    assert_eq!(
        git_out(&f.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
}

#[test]
fn a_verification_failure_exits_five() {
    // §7.2: "5 verification failed". Distinct from 3, because a failing check is
    // not a human decision — §4.5 sends it to repair.
    let f = Fixture::new();
    std::fs::write(f.repo.join(".conductor/verification.yaml"), FAILING_PROFILE)
        .expect("write profile");

    let out = f.task_run(&[]);
    assert_eq!(
        out.status.code(),
        Some(5),
        "a failing required check is exit 5: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let report = json(&out);
    assert_eq!(report["outcome"], "STOPPED");
    assert!(
        !report["refusals"].as_array().expect("refusals").is_empty(),
        "the gate must say what it refused on"
    );
}

#[test]
fn a_run_that_needs_a_human_exits_three() {
    // §7.2: "3 action required — approval or review pending ← scriptable 'human
    // needed'". Acceptance row 6: a false-success report is `CONTRADICTED` and
    // halts.
    let f = Fixture::new();
    std::fs::write(
        f.root().join("scenario.json"),
        r#"{"id":"cli-false-success","steps":[
             {"step":"report_on_stdout","claim":"COMPLETE","files_touched":["src/added.rs"],
              "summary":"all done (it is not)"},
             {"step":"exit","code":0}]}"#,
    )
    .expect("write scenario");

    let out = f.task_run(&[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "a run awaiting a human is exit 3, not a generic failure: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let report = json(&out);
    assert_eq!(report["state"], "AWAITING_REVIEW");
}

#[test]
fn exit_three_is_distinguishable_from_exit_one() {
    // The property §7.2 actually asks for, asserted as a property rather than as
    // two separate numbers: "must be distinguishable from failure".
    let f = Fixture::new();
    std::fs::write(f.repo.join(".conductor/task.yaml"), "id: T-0012\n").expect("write");
    let broken = f.task_run(&[]);
    assert_ne!(
        broken.status.code(),
        Some(3),
        "an invalid spec is not 'a human is needed'"
    );

    let g = Fixture::new();
    std::fs::write(
        g.root().join("scenario.json"),
        r#"{"id":"x","steps":[{"step":"report_on_stdout","claim":"COMPLETE",
             "files_touched":["src/added.rs"],"summary":"lie"},{"step":"exit","code":0}]}"#,
    )
    .expect("write");
    assert_eq!(g.task_run(&[]).status.code(), Some(3));
}

#[test]
fn an_unhealthy_store_exits_two() {
    // §7.2: "2 no project / not initialized / store unhealthy".
    let f = Fixture::new();
    std::fs::write(f.root().join("conductor.db"), b"this is not a database").expect("write");
    let out = f.task_run(&[]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_invalid_task_spec_is_refused_before_anything_runs() {
    // §3.7's smallest version: a task with no verification profile is "the
    // mechanism by which a task reaches `COMPLETE` on an agent's word".
    let f = Fixture::new();
    std::fs::write(
        f.repo.join(".conductor/task.yaml"),
        "id: T-0012\nobjective: do a thing\nscope: [\"src/**\"]\nverification_profile: \"\"\n",
    )
    .expect("write");

    let out = f.task_run(&[]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("verification"),
        "the refusal must say which field is wrong: {stderr}"
    );
    // Nothing was created on the strength of an unusable spec.
    assert!(!f.root().join("workspaces").exists());
}

#[test]
fn a_task_id_that_does_not_match_the_spec_is_refused() {
    let f = Fixture::new();
    let out = f.task_run(&[
        "--spec",
        &f.repo.join(".conductor/task.yaml").display().to_string(),
    ]);
    assert_eq!(out.status.code(), Some(0), "control: the ids do match");

    let g = Fixture::new();
    let mut args = vec!["task", "run", "T-9999", "--json", "--repo"];
    let repo = g.repo.display().to_string();
    args.push(&repo);
    let store = g.store();
    args.extend(["--store", &store]);
    let agent = g.fake_agent();
    args.extend(["--adapter", "fake", "--agent-binary", &agent]);
    let scenario = g.root().join("scenario.json").display().to_string();
    args.extend(["--scenario", &scenario]);
    let out = run(&args);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("T-9999"),
        "the refusal must name the id that was asked for: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// task show / task list
// ---------------------------------------------------------------------------

#[test]
fn show_reports_state_attempts_verification_and_findings() {
    // §7.1: "`conductor task show <task-id>` — state, attempts, verification,
    // findings, diff".
    let f = Fixture::new();
    assert_eq!(f.task_run(&[]).status.code(), Some(0));

    let out = f.task(&["show", "T-0012", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let report = json(&out);

    assert_eq!(report["task"]["id"], "T-0012");
    assert_eq!(report["task"]["state"], "COMPLETE");
    assert_eq!(report["run"]["state"], "COMPLETE");
    assert_eq!(
        report["attempts"].as_array().expect("attempts").len(),
        1,
        "one agent attempt"
    );
    assert_eq!(report["attempts"][0]["state"], "RECONCILED");
    assert!(
        !report["verification"]
            .as_array()
            .expect("verification")
            .is_empty(),
        "the checks that decided COMPLETE must be visible"
    );
    assert!(report["findings"].is_array());
    // The Conductor-owned effects, so a human can see what was done in their
    // repository and under which operation id. Three, not two: S3's baseline
    // artifact goes through the same ledger, and `task show` reports what the
    // ledger holds rather than a curated subset of it.
    let effects = report["effects"].as_array().expect("effects");
    let kinds: Vec<&str> = effects
        .iter()
        .map(|e| e["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(
        kinds,
        vec!["artifact.write", "git.commit.local", "git.fetch_into_main"],
        "{effects:?}"
    );
    assert!(
        effects.iter().all(|e| e["state"] == "CONFIRMED"),
        "every effect must be decided: {effects:?}"
    );
    assert!(report["diff"].is_array(), "the changed paths");
}

#[test]
fn show_refuses_a_task_that_does_not_exist() {
    let f = Fixture::new();
    assert_eq!(f.task_run(&[]).status.code(), Some(0));
    let out = f.task(&["show", "T-9999", "--json"]);
    assert_ne!(out.status.code(), Some(0));
    assert_ne!(
        out.status.code(),
        Some(3),
        "a missing task is a mistake, not a human decision"
    );
}

#[test]
fn list_reports_every_task_and_filters_by_state() {
    // §7.1: "`conductor task list [--state …]`".
    let f = Fixture::new();
    assert_eq!(f.task_run(&[]).status.code(), Some(0));

    let out = f.task(&["list", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let all = json(&out);
    let tasks = all["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "T-0012");
    assert_eq!(tasks[0]["state"], "COMPLETE");

    let complete = json(&f.task(&["list", "--state", "COMPLETE", "--json"]));
    assert_eq!(complete["tasks"].as_array().expect("tasks").len(), 1);

    let pending = json(&f.task(&["list", "--state", "PENDING", "--json"]));
    assert_eq!(pending["tasks"].as_array().expect("tasks").len(), 0);
}

#[test]
fn an_unknown_state_filter_is_a_usage_error() {
    // §7.2: "64 usage error (EX_USAGE)". A typo'd state must not silently match
    // nothing — that reads as "no such tasks" and is a different answer.
    let f = Fixture::new();
    assert_eq!(f.task_run(&[]).status.code(), Some(0));
    let out = f.task(&["list", "--state", "NOT_A_STATE", "--json"]);
    assert_eq!(out.status.code(), Some(64));
}

#[test]
fn every_task_command_renders_without_json_too() {
    // §7.1: "`--json` on every command" — which implies a human rendering as
    // well, and a `--json` flag that changes only the rendering.
    let f = Fixture::new();
    assert_eq!(f.task_run(&[]).status.code(), Some(0));

    for args in [vec!["show", "T-0012"], vec!["list"]] {
        let out = f.task(&args);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("T-0012"), "{args:?}: {text}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_err(),
            "{args:?}: without --json the output should be for a person: {text}"
        );
    }
}
