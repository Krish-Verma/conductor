//! `conductor task run --adapter codex` hands the agent a **per-run** credential
//! home — master plan §4.9, as amended by S10.
//!
//! # What these tests are for
//!
//! S10 built `enforce::env::materialize_credential_home` and proved it in
//! isolation, but the real launch path never called it: the S10 completion
//! report lists *"`CODEX_HOME` is materialised but not yet wired into the CLI's
//! launch path"* under known limitations. A credential boundary that only the
//! tests construct is not a product boundary, so every assertion here goes
//! through the `conductor` binary rather than through library calls.
//!
//! # Why the boundary exists at all
//!
//! Codex authenticates from files under `CODEX_HOME`, not from an API-key
//! variable. Pointing that variable at the operator's real `~/.codex` is wrong
//! twice over, and the second is the serious one:
//!
//! 1. It hands the agent `config.toml`, every profile and the whole session
//!    history — none of which is a credential.
//! 2. **Codex writes into `CODEX_HOME`.** A contained run would leave session
//!    rollouts in the operator's home: outside the workspace, outside §4.8's
//!    reconciled surface and outside the per-run `TMPDIR` audit. The containment
//!    story would have a hole shaped exactly like its own foundation.
//!
//! # Secret hygiene
//!
//! **No test here reads, prints or compares the contents of a credential.** The
//! operator's home is a synthetic one built inside the fixture's tempdir, every
//! file in it is a canary this file wrote itself, and the real `~/.codex` is
//! only ever used as a *path* in an inequality — never opened. Assertions are on
//! paths, permission bits and byte counts.
//!
//! The stand-in agent records its environment by **name only** for the allowlist
//! assertion, and records the one value that is a path rather than a secret.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CONDUCTOR: &str = env!("CARGO_BIN_EXE_conductor");

/// The task Codex is asked to do, and the profile that decides it.
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

/// A file shaped like a credential and containing nothing of the kind.
///
/// Every byte the fixture writes into the synthetic operator home is one of
/// these. A test that copied a live credential to prove copying works would
/// have proved it by doing the thing this boundary exists to prevent.
const CANARY: &str = "SYNTHETIC-CANARY-NOT-A-CREDENTIAL\n";

/// A stand-in for `codex exec`, in the two respects that matter here: it reads
/// `CODEX_HOME`, and it **writes** into it.
///
/// `__OBS__` is replaced with an absolute path outside the workspace, so the
/// recording survives even when `CODEX_HOME` is unset — which is exactly the
/// state this file's first test has to be able to observe.
const STUB: &str = r#"#!/bin/sh
# Conductor test double for `codex exec` (S11 task 10).
# Records metadata only: the value of CODEX_HOME (a directory path, never a
# secret) and the *names* of the variables it was given.
set -u
obs="__OBS__"
mkdir -p "$obs"
printf '%s\n' "${CODEX_HOME-<unset>}" > "$obs/codex-home"
env | sed 's/=.*//' | sort > "$obs/env-names"
: > "$obs/ran"

# Codex writes session rollouts into CODEX_HOME. This is the containment probe:
# wherever this lands is where a real run would leave the operator's history.
if [ -n "${CODEX_HOME-}" ]; then
  mkdir -p "$CODEX_HOME/sessions"
  printf '%s\n' '{"canary":"session-rollout"}' > "$CODEX_HOME/sessions/rollout-canary.jsonl"
fi

# One line of Codex-shaped JSONL, so the adapter parses something real.
printf '%s\n' '{"type":"thread.started","thread_id":"stub-thread"}'

# The work itself, in scope.
printf '%s\n' 'pub fn added() -> u32 { 1 }' > src/added.rs

# The report goes where --output-last-message says, as the adapter expects.
report=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--output-last-message" ]; then
    report="$arg"
  fi
  prev="$arg"
done
if [ -n "$report" ]; then
  printf '%s\n' '{"claim":"COMPLETE","files_touched":["src/added.rs"],"summary":"stub"}' > "$report"
fi
exit 0
"#;

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

        let fixture = Fixture { dir, repo };
        fixture.write_operator_home();
        fixture.write_stub();
        fixture
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().expect("canonicalize")
    }

    /// A synthetic stand-in for the operator's home directory.
    ///
    /// Its `.codex` holds three canaries: the one file the adapter names, a
    /// configuration file it does not, and a session history it certainly does
    /// not. Two of the three must never reach the agent.
    fn write_operator_home(&self) {
        let codex = self.operator_codex();
        std::fs::create_dir_all(codex.join("sessions")).expect("mkdir");
        std::fs::write(codex.join("auth.json"), CANARY).expect("write");
        std::fs::write(codex.join("config.toml"), CANARY).expect("write");
        std::fs::write(codex.join("sessions/history-canary.jsonl"), CANARY).expect("write");
    }

    fn operator_home(&self) -> PathBuf {
        self.root().join("operator-home")
    }

    fn operator_codex(&self) -> PathBuf {
        self.operator_home().join(".codex")
    }

    /// Where the stand-in agent records what it was given.
    fn observations(&self) -> PathBuf {
        self.root().join("observed")
    }

    fn write_stub(&self) {
        let path = self.root().join("codex-stub");
        std::fs::write(
            &path,
            STUB.replace("__OBS__", &self.observations().display().to_string()),
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        }
    }

    fn store(&self) -> String {
        self.root().join("conductor.db").display().to_string()
    }

    /// `conductor task run T-0012 --adapter codex`, with the operator's home
    /// redirected at the fixture's synthetic one.
    ///
    /// `CODEX_HOME` is **removed** from the CLI's own environment unless a test
    /// sets it: a developer who happens to have it exported must not cause this
    /// suite to read their real credential directory.
    fn task_run(&self, codex_home: Option<&Path>) -> Output {
        let mut command = Command::new(CONDUCTOR);
        command
            .args([
                "task",
                "run",
                "T-0012",
                "--json",
                "--repo",
                &self.repo.display().to_string(),
                "--store",
                &self.store(),
                "--adapter",
                "codex",
                "--agent-binary",
                &self.root().join("codex-stub").display().to_string(),
            ])
            .env("HOME", self.operator_home())
            .env_remove("CODEX_HOME");
        if let Some(home) = codex_home {
            command.env("CODEX_HOME", home);
        }
        command
            .output()
            .unwrap_or_else(|e| panic!("spawn {CONDUCTOR}: {e}"))
    }

    /// What the agent saw in `CODEX_HOME`, or `<unset>`.
    fn observed_codex_home(&self) -> String {
        std::fs::read_to_string(self.observations().join("codex-home"))
            .unwrap_or_else(|e| {
                panic!(
                    "the agent never recorded its environment at {}: {e}",
                    self.observations().display()
                )
            })
            .trim()
            .to_string()
    }

    fn workspace(&self, run: &str) -> PathBuf {
        self.root().join("workspaces").join(run)
    }
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

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not json ({e}):\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

// ---------------------------------------------------------------------------
// 1. Positive — the launch path materialises, and it materialises *inside* the
//    run's workspace.
// ---------------------------------------------------------------------------

#[test]
fn a_codex_launch_receives_a_credential_home_inside_the_run_workspace() {
    let f = Fixture::new();
    let out = f.task_run(None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the run did not complete:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let run = json(&out)["run"]
        .as_str()
        .expect("the report names the run")
        .to_string();
    let expected = f.workspace(&run).join(".conductor-agent-home");
    assert_eq!(
        f.observed_codex_home(),
        expected.display().to_string(),
        "CODEX_HOME must name the per-run directory inside this run's workspace"
    );

    // Only the file the adapter names — not the configuration, not the history.
    let home = PathBuf::from(f.observed_codex_home());
    assert!(
        home.join("auth.json").is_file(),
        "the credential was copied"
    );
    assert!(
        !home.join("config.toml").exists(),
        "config.toml is not a credential and must not travel"
    );
    assert!(
        !home.join("sessions/history-canary.jsonl").exists(),
        "the operator's session history must not travel"
    );
    assert_eq!(
        std::fs::metadata(home.join("auth.json"))
            .expect("stat the copy")
            .len(),
        std::fs::metadata(f.operator_codex().join("auth.json"))
            .expect("stat the source")
            .len(),
        "the copy must be the file the adapter named (compared by size; \
         contents are never read)"
    );

    #[cfg(unix)]
    {
        assert_eq!(mode(&home), 0o700, "the credential home must be owner-only");
        assert_eq!(
            mode(&home.join("auth.json")),
            0o600,
            "the credential must be owner-only"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Negative — asserted on the value, not on the absence of a crash.
// ---------------------------------------------------------------------------

#[test]
fn the_operators_own_codex_directory_is_never_handed_to_the_agent() {
    let f = Fixture::new();
    let out = f.task_run(None);
    assert_eq!(out.status.code(), Some(0), "{:?}", out.status);
    let run = json(&out)["run"].as_str().expect("a run id").to_string();

    let observed = f.observed_codex_home();
    assert_ne!(
        observed,
        f.operator_codex().display().to_string(),
        "the run must not be pointed at the directory it copied from"
    );
    assert_ne!(
        observed,
        f.operator_home().display().to_string(),
        "nor at the operator's home itself"
    );

    // The real operator's directory on this machine, as a *path* — never
    // opened, never read, never printed beyond this inequality.
    let real = PathBuf::from(std::env::var("HOME").expect("HOME")).join(".codex");
    assert_ne!(
        observed,
        real.display().to_string(),
        "a real `~/.codex` must never be handed to an agent"
    );

    assert!(
        Path::new(&observed).starts_with(f.workspace(&run)),
        "{observed} is outside the run's workspace, so it dies with nothing \
         and is outside §4.8's reconciled surface"
    );
}

// ---------------------------------------------------------------------------
// 3. Containment — where the agent's own writes land.
// ---------------------------------------------------------------------------

#[test]
fn codex_written_state_lands_in_the_workspace_and_not_in_the_operators_home() {
    let f = Fixture::new();
    let out = f.task_run(None);
    assert_eq!(out.status.code(), Some(0), "{:?}", out.status);
    let run = json(&out)["run"].as_str().expect("a run id").to_string();

    let rollout = f
        .workspace(&run)
        .join(".conductor-agent-home")
        .join("sessions/rollout-canary.jsonl");
    assert!(
        rollout.is_file(),
        "the agent's own write must land inside the workspace at {}",
        rollout.display()
    );

    assert!(
        !f.operator_codex()
            .join("sessions/rollout-canary.jsonl")
            .exists(),
        "the agent left a session rollout in the operator's home"
    );
    let left_behind: BTreeSet<String> = std::fs::read_dir(f.operator_codex().join("sessions"))
        .expect("read the operator's sessions")
        .map(|e| e.expect("entry").file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        left_behind,
        BTreeSet::from(["history-canary.jsonl".to_string()]),
        "the operator's session directory gained something"
    );
}

// ---------------------------------------------------------------------------
// 4. Fail closed — a missing credential refuses before anything launches.
// ---------------------------------------------------------------------------

#[test]
fn a_missing_credential_file_refuses_rather_than_launching_with_an_empty_home() {
    let f = Fixture::new();
    std::fs::remove_file(f.operator_codex().join("auth.json")).expect("remove the credential");

    let out = f.task_run(None);
    assert_ne!(
        out.status.code(),
        Some(0),
        "a run with no credential must not report success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("auth.json"),
        "the refusal must name the file that is missing, or the operator debugs \
         a 401 several layers away from the cause: {stderr}"
    );
    assert!(
        !f.observations().join("ran").exists(),
        "the agent was launched anyway, with an empty credential home"
    );
}

// ---------------------------------------------------------------------------
// 5. The allowlist is not weakened by any of the above.
// ---------------------------------------------------------------------------

#[test]
fn the_agents_environment_is_the_allowlist_plus_codex_home_and_nothing_else() {
    // §4.9 is an allowlist, and `with_extra` is the only way in. Wiring a
    // credential home must add exactly one name — a containment assertion would
    // not notice the day it added two.
    let f = Fixture::new();
    let out = f.task_run(None);
    assert_eq!(out.status.code(), Some(0), "{:?}", out.status);

    let observed: BTreeSet<String> = std::fs::read_to_string(f.observations().join("env-names"))
        .expect("the agent recorded the names it was given")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();

    let mut expected: BTreeSet<String> = conductor_run::enforce::env::AGENT_ENV_KEYS
        .iter()
        .map(|k| (*k).to_string())
        .collect();
    expected.insert("CODEX_HOME".to_string());
    // `sh` sets these itself in the child; they were not handed to it.
    let shell_owned = ["PWD", "SHLVL", "_", "IFS", "OPTIND", "PS1", "PS2", "PS4"];
    let observed: BTreeSet<String> = observed
        .into_iter()
        .filter(|name| !shell_owned.contains(&name.as_str()))
        .collect();

    assert_eq!(
        observed, expected,
        "the agent's environment is neither more nor less than §4.9's allowlist \
         plus the one variable the adapter named"
    );
}
