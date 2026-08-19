//! `conductor task run | show | list` — master plan §7.1, with §7.2's exit
//! codes.
//!
//! Exit code **3** is the one that earns its own test: §7.2 gives it a dedicated
//! slot because "Conductor stopped and needs a human" is the most common
//! non-success outcome and "must be distinguishable from failure by a wrapper
//! script". A test that only checked "non-zero" would not notice the day it
//! became 1.
//!
//! # Why the fixture is a plan and not a spec (S12)
//!
//! Until S12 every test here wrote `.conductor/task.yaml`, the S5-era task spec,
//! and `task run` read it. That file was never the authority §3.2 describes — *"a
//! plan is a file you write"* — and the command that read it fabricated a
//! `plan_version` row at `version 0`, `state DRAFT` to satisfy §5.1's foreign
//! key. So the fixture below is a real `.conductor/` layout and v1 is **approved
//! over the control socket** before anything runs, because that is now the only
//! way a task row exists at all.
//!
//! The subject of this file did not change: it is still §7.2's exit codes and
//! what `task show` / `task list` report. `tests/task_run_plan.rs` owns the
//! question of *which* plan authorizes a run; the assertions here deliberately
//! do not restate it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use conductor_store::Store;

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

/// §3.1's project file. `adapter: fake` is declared even though every run below
/// passes `--adapter fake`, because `runnable::resolve` reads this file to learn
/// which project it is looking at — a repository without it is not a project.
const PROJECT_YAML: &str = r#"
project:
  id: p-task
  default_branch: main
  adapter: fake
"#;

/// The verification profile the fixture plan's task names.
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

const POLICY_YAML: &str = r#"
policy:
  rules: []
"#;

/// The plan every test here runs against: one task, one bound criterion.
///
/// `verification_profile` is a path relative to the repository root, which is
/// what §4.5's clarification 3 settles to.
const PLAN_V1: &str = r#"
plan:
  id: p-task
  version: 1
  objective: "Prove §7.2's exit codes on the core verb."
  milestones:
    - id: M-01
      title: "The core verb"
      slices:
        - id: S-01
          title: "task run"
          tasks:
            - id: T-0012
              objective: "Add a greeting helper to the library."
              rationale: "The exit-code tests need a task that changes a file."
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

/// The same plan naming a verification profile that is not in the repository.
///
/// §3.7 does not refuse this — the validator is given the check catalogue and
/// deliberately does not resolve per-task profile *paths* — so a plan can be
/// approved and still name a document `task run` cannot read. That makes this
/// the refusal `task run` itself owns, and the one the deleted spec test used to
/// stand for.
const PLAN_WITH_MISSING_PROFILE: &str = r#"
plan:
  id: p-task
  version: 1
  objective: "Prove a task with no readable profile never starts."
  milestones:
    - id: M-01
      title: "The core verb"
      slices:
        - id: S-01
          title: "task run"
          tasks:
            - id: T-0012
              objective: "Work nothing could ever prove."
              rationale: "A task whose criteria bind to a document that is absent."
              depends_on: []
              scope:
                allowed_globs: ["src/**"]
              verification_profile: .conductor/there-is-no-such-profile.yaml
              attempt_budget: 3
              acceptance_criteria:
                - id: AC-1
                  statement: "The library gains a function."
                  verified_by: [unit-tests]
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
        Fixture::with_plan(PLAN_V1)
    }

    fn with_plan(plan: &str) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mkdir");
        std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> u32 { 0 }\n").expect("write");
        write(&repo, ".conductor/project.yaml", PROJECT_YAML);
        write(&repo, ".conductor/verification.yaml", PROFILE);
        write(&repo, ".conductor/policy.yaml", POLICY_YAML);
        write(&repo, ".conductor/plans/v1/plan.yaml", plan);

        git(&repo, &["init", "-q", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["config", "user.email", "fixture@localhost"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "initial"]);

        std::fs::write(root.join("scenario.json"), SCENARIO).expect("write");
        // `approval serve` refuses to create a store — §7.2's `2` is "store
        // unhealthy", and a control socket that conjured a database would be a
        // second way for project truth to appear. The fixture creates it, as an
        // operator's `conductor init` would have left it.
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
    /// prefix eight characters longer, and the bind goes through a
    /// `.<name>.<pid>.staging` sibling that adds sixteen more. A descriptive
    /// name here fails as "path must be shorter than SUN_LEN", which reads like
    /// a Conductor defect and is not one.
    fn socket(&self) -> PathBuf {
        self.dir.path().join("cs").join("s")
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

    /// Make v1 authoritative the way §5.2 requires: a human at the control
    /// socket. The server lives only for the call, so nothing else here shares
    /// the store with it.
    fn approve_ok(&self, version: u32) {
        let out = {
            let _server = Server::start(self);
            run(&[
                "plan",
                "approve",
                &version.to_string(),
                "--repo",
                &self.repo.display().to_string(),
                "--socket",
                &self.socket().display().to_string(),
                "--json",
            ])
        };
        assert_eq!(
            out.status.code(),
            Some(0),
            "approving v{version}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
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

fn run(args: &[&str]) -> Output {
    Command::new(CONDUCTOR)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
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
    f.approve_ok(1);
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
    //
    // The profile is swapped *after* approval on purpose: §3.6 hashes the plan
    // document, and swapping a check's command is a change to what proves the
    // task rather than to what the task is. A human does not re-approve a plan
    // because a test started failing.
    let f = Fixture::new();
    f.approve_ok(1);
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
    f.approve_ok(1);
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
    //
    // The non-3 half is a plan naming a verification profile that is not there —
    // a configuration mistake, which is what the deleted `task.yaml` half of
    // this test used to be. It must not read as "a human is needed", because no
    // decision would fix it.
    let f = Fixture::with_plan(PLAN_WITH_MISSING_PROFILE);
    f.approve_ok(1);
    let broken = f.task_run(&[]);
    assert_ne!(
        broken.status.code(),
        Some(3),
        "an unreadable verification profile is not 'a human is needed': {}",
        String::from_utf8_lossy(&broken.stderr)
    );

    let g = Fixture::new();
    g.approve_ok(1);
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
    f.approve_ok(1);
    // The sidecars go too. A `-wal` left beside a replaced main file describes a
    // database that is no longer there, and the test would then be measuring
    // SQLite's recovery path rather than Conductor's refusal.
    for sidecar in ["conductor.db-wal", "conductor.db-shm"] {
        let _ = std::fs::remove_file(f.root().join(sidecar));
    }
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
fn a_task_naming_a_verification_profile_that_is_not_there_is_refused_before_anything_runs() {
    // §3.7's smallest version, on the authority that replaced the task spec: an
    // acceptance criterion bound to a document Conductor cannot read is "the
    // mechanism by which a task reaches `COMPLETE` on an agent's word". §3.7
    // itself cannot catch it — the validator is handed the check catalogue and
    // does not resolve per-task profile paths — so `task run` must, and it must
    // do it before an agent has edited anything.
    let f = Fixture::with_plan(PLAN_WITH_MISSING_PROFILE);
    f.approve_ok(1);

    let out = f.task_run(&[]);
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("verification_profile"),
        "the refusal must say which field is wrong: {stderr}"
    );
    // Nothing was created on the strength of a task nothing could prove.
    assert!(!f.root().join("workspaces").exists());
}

// `a_task_id_that_does_not_match_the_spec_is_refused` lived here until S12.
//
// It asserted that `task run T-9999` was refused when `.conductor/task.yaml`
// declared `T-0012`, which was a statement about a file that no longer exists.
// The surviving invariant — a task the approved plan does not declare is refused
// by name — is asserted in `tests/task_run_plan.rs::
// a_task_the_approved_plan_does_not_declare_is_refused_by_name`, against the
// authority that now decides it. A second copy here would be a second thing to
// keep in agreement, so the coverage moved rather than went away.

// ---------------------------------------------------------------------------
// task show / task list
// ---------------------------------------------------------------------------

#[test]
fn show_reports_state_attempts_verification_and_findings() {
    // §7.1: "`conductor task show <task-id>` — state, attempts, verification,
    // findings, diff".
    let f = Fixture::new();
    f.approve_ok(1);
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
    f.approve_ok(1);
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
    f.approve_ok(1);
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
    f.approve_ok(1);
    assert_eq!(f.task_run(&[]).status.code(), Some(0));
    let out = f.task(&["list", "--state", "NOT_A_STATE", "--json"]);
    assert_eq!(out.status.code(), Some(64));
}

#[test]
fn every_task_command_renders_without_json_too() {
    // §7.1: "`--json` on every command" — which implies a human rendering as
    // well, and a `--json` flag that changes only the rendering.
    let f = Fixture::new();
    f.approve_ok(1);
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
