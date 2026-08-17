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
//!
//! # Where the adapter question sits relative to the plan (S12)
//!
//! S12 moved `task run` onto the plan ledger, so a task only exists once a human
//! has approved a plan version. The adapter question is asked *before* that: a
//! name nobody implements, or no name at all, is refused without the store or the
//! plan being consulted. That ordering is what lets the two refusals below run
//! against a repository with no approved plan and still be about the adapter —
//! and the two successful runs approve v1 first, because otherwise there would be
//! no task to run whichever adapter won.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use conductor_store::Store;

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

const POLICY_YAML: &str = r#"
policy:
  rules: []
"#;

/// The plan that declares T-0012. Deliberately says nothing about adapters:
/// §3.1 puts that decision in `project.yaml`, and a plan that could name one
/// would be the third source this file's rule denies exists.
const PLAN_V1: &str = r#"
plan:
  id: p-adapter
  version: 1
  objective: "Prove the project file decides which agent runs."
  milestones:
    - id: M-01
      title: "Adapter resolution"
      slices:
        - id: S-01
          title: "task run"
          tasks:
            - id: T-0012
              objective: "Add a greeting helper to the library."
              rationale: "Some agent has to run it; which one is the question."
              depends_on: []
              scope:
                allowed_globs: ["src/**"]
                forbidden_globs: [".conductor/**"]
              verification_profile: .conductor/verification.yaml
              attempt_budget: 3
              acceptance_criteria:
                - id: AC-1
                  statement: "The library gains a function."
                  verified_by: [unit-tests]
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
    ///
    /// A fixture with no project file cannot have its plan approved either —
    /// `plan approve` reads `project.yaml` to learn which project it is speaking
    /// for. That is not a gap in the fixture: the refusal it exists to measure
    /// happens at adapter resolution, before any of that is reached.
    fn new(project_adapter: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mkdir");
        std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> u32 { 0 }\n").expect("write");
        write(&repo, ".conductor/verification.yaml", PROFILE);
        write(&repo, ".conductor/policy.yaml", POLICY_YAML);
        write(&repo, ".conductor/plans/v1/plan.yaml", PLAN_V1);
        if let Some(adapter) = project_adapter {
            write(&repo, ".conductor/project.yaml", &project_yaml(adapter));
        }

        git(&repo, &["init", "-q", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["config", "user.email", "fixture@localhost"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "initial"]);

        std::fs::write(root.join("scenario.json"), SCENARIO).expect("write");
        // `approval serve` refuses to create a store, so the fixture creates it —
        // the state `conductor init` leaves behind.
        Store::open_or_create(root.join("conductor.db")).expect("create the store");
        Fixture { dir, repo }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canonicalize")
    }

    fn store(&self) -> String {
        self.root().join("conductor.db").display().to_string()
    }

    /// Deliberately short, and **not** canonicalized: a `sockaddr_un` path is
    /// capped at `SUN_LEN`, macOS resolves `/var/folders/…` to a `/private`
    /// prefix eight characters longer, and the bind adds a
    /// `.<name>.<pid>.staging` sibling on top of that.
    fn socket(&self) -> PathBuf {
        self.dir.path().join("cs").join("s")
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

    /// Make a plan version authoritative, as §5.2 requires: a human at the
    /// control socket.
    fn approve_ok(&self, version: u32) {
        let out = {
            let _server = Server::start(self);
            Command::new(CONDUCTOR)
                .args([
                    "plan",
                    "approve",
                    &version.to_string(),
                    "--repo",
                    &self.repo.display().to_string(),
                    "--socket",
                    &self.socket().display().to_string(),
                    "--json",
                ])
                .output()
                .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
        };
        assert_eq!(
            out.status.code(),
            Some(0),
            "approving v{version}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
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
            self.store(),
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

/// A `conductor approval serve` process and the socket it published.
struct Server {
    child: Child,
}

impl Server {
    fn start(fixture: &Fixture) -> Server {
        let socket = fixture.socket();
        std::fs::create_dir_all(socket.parent().expect("a parent")).expect("mkdir socket");
        let child = Command::new(CONDUCTOR)
            .args([
                "approval",
                "serve",
                "--store",
                &fixture.store(),
                "--socket",
                &socket.display().to_string(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"));
        let mut server = Server { child };
        // Generous, because M29 measured macOS taking 21.7 s to scan a freshly
        // built binary before its first instruction runs.
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return server;
            }
            if let Ok(Some(status)) = server.child.try_wait() {
                use std::io::Read;
                let mut said = String::new();
                if let Some(mut err) = server.child.stderr.take() {
                    let _ = err.read_to_string(&mut said);
                }
                if let Some(mut out) = server.child.stdout.take() {
                    let _ = out.read_to_string(&mut said);
                }
                panic!(
                    "the control socket server exited {status} before listening at {}: {said}",
                    socket.display()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("nothing was listening at {}", socket.display());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
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
    f.approve_ok(1);
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
    f.approve_ok(1);
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
        "a missing project.yaml with no override is §7.2's `2`: {}",
        String::from_utf8_lossy(&out.stderr)
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
    // name must be refused, and refused identically whichever of §3.1's two
    // doors it came through. The command-line half is the control — without it
    // "the project file is refused" would also be satisfied by a run that
    // refuses every project file it reads.
    let from_file = Fixture::new(Some("hal9000"));
    let file_out = from_file.task_run(&[]);
    assert_ne!(file_out.status.code(), Some(0));
    let file_stderr = String::from_utf8_lossy(&file_out.stderr);
    assert!(
        file_stderr.contains("hal9000"),
        "the refusal must name the adapter it did not recognise: {file_stderr}"
    );

    let from_flag = Fixture::new(Some("fake"));
    let flag_out = from_flag.task_run(&["--adapter", "hal9000"]);
    let flag_stderr = String::from_utf8_lossy(&flag_out.stderr);
    assert!(
        flag_stderr.contains("hal9000"),
        "and so must the command-line one: {flag_stderr}"
    );
    assert_eq!(
        file_out.status.code(),
        flag_out.status.code(),
        "one unknown name, one answer, whichever door it came through"
    );

    // Neither fixture has an approved plan, and neither refusal needed one: an
    // adapter nobody implements is refused before the store or the plan ledger
    // is consulted at all, which is why no agent could have been launched under
    // a name Conductor cannot resolve.
    assert!(
        !from_file.root().join("workspaces").exists(),
        "a workspace was cloned for an adapter that does not exist"
    );
}
