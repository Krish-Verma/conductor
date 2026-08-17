//! Shared setup for the supervision, scenario and recovery tests.
#![allow(dead_code)] // one copy per test binary; each uses a subset

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use conductor_agent::StartInput;
use conductor_core::{PolicyHash, RunId, TaskId};

/// The fake agent binary from **this build**, not from `PATH`.
///
/// `CARGO_BIN_EXE_*` only exists for binaries in the same package, and the fake
/// agent lives in `conductor-agent`, so it is located relative to the test
/// executable instead. Asserting it exists here means a missing binary is a
/// clear failure rather than a mysterious `NotFound` inside a timeout.
pub fn fake_agent_binary() -> PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    // .../target/debug/deps/<test-binary> → .../target/debug/
    let target_dir = test_exe
        .parent()
        .and_then(Path::parent)
        .expect("target directory");
    let path = target_dir.join("conductor-fake-agent");
    assert!(
        path.exists(),
        "the fake agent binary is missing at {}; `cargo test --all` builds it",
        path.display()
    );
    path
}

/// Pay macOS's first-execution scan **once per test binary**, outside any
/// deadline.
///
/// M29: a freshly built binary takes 21.7 s to run the first time on this host
/// against 3.3 s warm, and at S2.5 that cost blew a probe's deadline under
/// parallel load. The supervisor is designed so a cold start cannot be misread
/// (its startup budget is separate and generous, and the agent's own budgets
/// start at its first line), but a test that asserts a sub-second timeout should
/// not be the place that discovers whether that design holds under a 21 s scan.
/// So the scan is paid here, deliberately, before anything is measured — the
/// same move S2.5's `payload_self_check` makes and for the same reason.
pub fn warm_the_binary() {
    static WARM: OnceLock<()> = OnceLock::new();
    WARM.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        let scenario = write_scenario(
            dir.path(),
            r#"{"id":"warm","steps":[{"step":"exit","code":0}]}"#,
        );
        let started = Instant::now();
        let status = Command::new(fake_agent_binary())
            .arg("--scenario")
            .arg(&scenario)
            .arg("--max-lifetime-ms")
            .arg("120000")
            .current_dir(dir.path())
            .status()
            .expect("the fake agent must be runnable on this host");
        assert!(
            status.success(),
            "the fake agent could not complete a trivial scenario; nothing \
             measured through it would mean anything"
        );
        eprintln!(
            "warmed the fake agent binary in {:?} (M29: 21.7s cold / 3.3s warm)",
            started.elapsed()
        );
    });
}

/// Write a catalogued scenario into `dir` and return its path.
pub fn scenario_file(dir: &Path, id: &str) -> PathBuf {
    let scenario = conductor_agent::scenario::scenario_by_id(id)
        .unwrap_or_else(|| panic!("no catalogued scenario {id}"));
    let path = dir.join(format!("scenario-{id}.json"));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&scenario).expect("serialize"),
    )
    .expect("write scenario");
    path
}

/// Write a hand-written scenario into `dir` and return its path.
pub fn write_scenario(dir: &Path, json: &str) -> PathBuf {
    let path = dir.join(format!(
        "scenario-{}.json",
        json.len() as u64 ^ std::process::id() as u64
    ));
    std::fs::write(&path, json).expect("write scenario");
    path
}

/// A `StartInput` whose workspace is `dir`.
pub fn start_input(dir: &Path) -> StartInput {
    let mut env = BTreeMap::new();
    // §4.9's allowlist. `PATH` is here because the scenarios that run `git`
    // need it; nothing else from the parent environment is inherited.
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }
    env.insert("HOME".to_string(), dir.join("home").display().to_string());

    StartInput {
        run_id: RunId::new("r-0041").expect("run id"),
        task_id: TaskId::new("T-0012").expect("task id"),
        attempt_ordinal: 1,
        workspace: dir.to_path_buf(),
        report_path: dir.join("artifacts").join("report.json"),
        session_id: None,
        // A stand-in packet: these fixtures test the supervisor and the adapters,
        // and neither reads the instruction's *content*. The real one is built by
        // `worker::store_packet` from durable state, and the tests that assert
        // that are the ones that drive a whole run.
        instructions: "packet: implementation\nobjective: \"Do the fixture's work.\"\n".to_string(),
        instructions_path: dir.join("artifacts").join("packet.yaml"),
        env,
    }
}

/// The policy hash every fixture run references.
pub const POLICY_HASH: &str =
    "blake3:0000000000000000000000000000000000000000000000000000000000000000";

/// A disposable source repository with one commit.
pub fn source_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("source");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    std::fs::write(repo.join("src/lib.rs"), "pub fn base() -> u32 { 0 }\n").expect("write");
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("write");

    git(&repo, &["init", "--initial-branch=main"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@localhost"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-m", "initial"]);
    repo
}

/// `git rev-parse HEAD` in a repository.
pub fn head(repo: &Path) -> String {
    String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("git rev-parse")
            .stdout,
    )
    .trim()
    .to_string()
}

/// Run `git`, insisting it succeeded.
pub fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A workspace cloned from `source`, isolated per §4.1 / ADR-0001.
pub fn workspace(source: &Path, workspaces_root: &Path, run: &str) -> conductor_git::Workspace {
    let request = conductor_git::WorkspaceRequest {
        source: source.to_path_buf(),
        workspace: workspaces_root.join(run),
        run_id: RunId::new(run).expect("run id"),
        task_id: TaskId::new("T-0012").expect("task id"),
        base_commit: head(source),
        policy_hash: PolicyHash::new(POLICY_HASH).expect("policy hash"),
    };
    conductor_git::create_workspace(&request).expect("create workspace")
}

/// Block until `predicate` holds, or fail.
///
/// A bounded wait for something the test cannot synchronise on directly — the
/// kernel finishing a teardown, for instance. Never used to wait for the agent,
/// which announces itself with checkpoints.
pub fn wait_until(what: &str, timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out after {timeout:?} waiting for {what}");
}
