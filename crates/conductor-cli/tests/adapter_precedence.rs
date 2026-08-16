//! Which adapter runs a task — master plan §3.1, §6.1, §7.1.
//!
//! # The defect this file exists for
//!
//! §3.1 lists `.conductor/project.yaml` as authoritative for *"identity,
//! **adapter**, scope defaults, review cadence, execution_requirements"*, and
//! S11 built the loader for it: `plan::project::Project` has an `adapter` field,
//! it is refused when blank, and it is inside `config_hash`. Nothing read it.
//! `conductor task run` took the adapter from `--adapter`, which defaulted to
//! `fake` — so a project that declared `adapter: codex` and a project that
//! declared nothing at all behaved identically, and the declaration was a knob
//! that did nothing.
//!
//! A configuration field that is parsed, validated, hashed and then ignored is
//! worse than an absent one: it reads as a decision the operator made, and it
//! silently is not.
//!
//! # The rule
//!
//! ```text
//! .conductor/project.yaml  adapter:      the project's configured adapter
//! --adapter <name>                       an explicit override, for this run
//! neither                                a refusal, never a default
//! ```
//!
//! There is no third source and no fallback. §3.1 says the adapter is one of the
//! decisions *"a human makes once, in a file they can diff"*, so inventing one
//! when the file is missing would be exactly the default project
//! `plan::project::load` already refuses to invent.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

const PROFILE: &str = r#"
verification:
  toolchain_fingerprint:
    - ["/bin/echo", "toolchain-v1"]
  required:
    - id: unit-tests
      command: ["/bin/sh", "-c", "echo ok"]
      timeout_seconds: 60
"#;

const SPEC: &str = r#"
id: T-0012
objective: Add a greeting helper to the library.
scope:
  - "src/**"
verification_profile: .conductor/verification.yaml
attempt_budget: 3
"#;

const SCENARIO: &str = r#"{"id":"cli-success","steps":[
  {"step":"emit","kind":"agent.started","detail":"cli"},
  {"step":"write_file","path":"src/added.rs","contents":"pub fn added() -> u32 { 1 }\n"},
  {"step":"checkpoint","name":"after-edits"},
  {"step":"report_on_stdout","claim":"COMPLETE","files_touched":["src/added.rs"],
   "summary":"added a function"},
  {"step":"exit","code":0}]}"#;

fn project_yaml(adapter: &str) -> String {
    format!("project:\n  id: p-adapter\n  default_branch: main\n  adapter: {adapter}\n")
}

struct Fixture {
    dir: tempfile::TempDir,
    repo: PathBuf,
}

impl Fixture {
    /// A runnable repository. `project_adapter` is written into
    /// `.conductor/project.yaml`; `None` writes no project file at all.
    fn new(project_adapter: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mkdir");
        std::fs::create_dir_all(repo.join(".conductor")).expect("mkdir");
        std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> u32 { 0 }\n").expect("write");
        std::fs::write(repo.join(".conductor/task.yaml"), SPEC).expect("write");
        std::fs::write(repo.join(".conductor/verification.yaml"), PROFILE).expect("write");
        if let Some(adapter) = project_adapter {
            std::fs::write(repo.join(".conductor/project.yaml"), project_yaml(adapter))
                .expect("write");
        }

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

    fn fake_agent(&self) -> String {
        let test_exe = std::env::current_exe().expect("current_exe");
        let target = test_exe
            .parent()
            .and_then(Path::parent)
            .expect("target dir");
        let path = target.join("conductor-fake-agent");
        assert!(
            path.exists(),
            "the fake agent is missing at {}",
            path.display()
        );
        path.display().to_string()
    }

    /// `conductor task run T-0012 --json`, **without** `--adapter` unless
    /// `extra` supplies it. The fake agent's binary and scenario are always
    /// passed: they say *how* to run the fake adapter, not *which* adapter to
    /// run, and this file is about the second question.
    fn task_run(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "task".to_string(),
            "run".to_string(),
            "T-0012".to_string(),
            "--json".to_string(),
            "--repo".to_string(),
            self.repo.display().to_string(),
            "--store".to_string(),
            self.root().join("conductor.db").display().to_string(),
            "--agent-binary".to_string(),
            self.fake_agent(),
            "--scenario".to_string(),
            self.root().join("scenario.json").display().to_string(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_string()));
        Command::new(CONDUCTOR)
            .args(&args)
            .output()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
    }
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
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

#[test]
fn the_project_file_names_the_adapter_when_the_command_line_does_not() {
    // POSITIVE CONTROL, and the whole point of the field. §3.1 makes
    // project.yaml authoritative for the adapter; a run that does not override
    // it must use what the file says.
    let f = Fixture::new(Some("fake"));
    let out = f.task_run(&[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json(&out);
    assert_eq!(report["adapter"]["id"], "fake");
    assert_eq!(
        report["adapter"]["source"], ".conductor/project.yaml",
        "the report must say where the adapter came from, or an operator \
         debugging a wrong adapter has nowhere to look"
    );
}

#[test]
fn the_command_line_overrides_the_project_file_for_one_run() {
    // The override is real, not merely reported: the project declares `codex`,
    // which on this fixture has no binary and no credential, so a run that
    // honoured the file would fail. It succeeds, therefore `fake` actually ran.
    let f = Fixture::new(Some("codex"));
    let out = f.task_run(&["--adapter", "fake"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json(&out);
    assert_eq!(report["adapter"]["id"], "fake");
    assert_eq!(report["adapter"]["source"], "--adapter");
}

#[test]
fn an_adapter_named_nowhere_is_refused_rather_than_defaulted() {
    // The old behaviour: `--adapter` defaulted to `fake`, so this ran the fake
    // agent against a repository that never asked for one. §3.1's "no default
    // project" reasoning applies to the adapter specifically — it "decides
    // *which agent runs*".
    let f = Fixture::new(None);
    let out = f.task_run(&[]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a missing project.yaml with no override is §7.2's `2`"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(".conductor/project.yaml"),
        "the refusal must name the file that should have declared it: {stderr}"
    );
    assert!(
        stderr.contains("--adapter"),
        "and the flag that can override it: {stderr}"
    );
}

#[test]
fn an_adapter_the_project_file_names_and_nobody_implements_is_refused_by_name() {
    // The project file is not a trusted source of adapter *names*: an unknown
    // name must be refused before a task row is written, exactly as it is when
    // it arrives on the command line.
    let f = Fixture::new(Some("hal9000"));
    let out = f.task_run(&[]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("hal9000"),
        "the refusal must name the adapter it did not recognise: {stderr}"
    );
}
