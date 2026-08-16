//! S9 — the environment is the boundary, and the boundary is measured.
//!
//! Master plan §4.9: *"Layer 6 is the primary control, not the fourth. An agent
//! with no push credential cannot push, regardless of what it types, what it is
//! told, or whether any hook fires."*
//!
//! Every test here obeys the rule S8 paid for: **a security test must prove both
//! that the intended thing works and that the unintended thing does not.** A
//! test that only asserts "the canary is absent from the child" passes happily
//! against a build that spawns no child at all, against a canary that was never
//! planted, and against an `env` binary that prints nothing. So each isolation
//! assertion is paired with a positive control that makes the same measurement
//! through the same code path with the isolation removed, and *that* control
//! must observe the leak. Without it, absence of evidence is being read as
//! evidence of absence.
//!
//! # Canaries, never real secrets
//!
//! The parent-side values planted here are disposable strings this file
//! invented. Nothing reads the operator's actual `AWS_SECRET_ACCESS_KEY` or
//! `GITHUB_TOKEN`, and nothing needs to: proving a *named variable* does not
//! cross the boundary does not require the real value behind that name.
//!
//! # Two things S9 found that the environment list alone did not cover
//!
//! 1. `GIT_ASKPASS=/bin/false` — shipped since S5 — names a path that **does not
//!    exist on macOS**, where `false` lives at `/usr/bin/false`. It failed
//!    closed only because `GIT_TERMINAL_PROMPT=0` caught the fallback. A named
//!    mechanism that is not present is not a mechanism, so the askpass is now
//!    Conductor's own file, written per run.
//! 2. A **system** gitconfig is read by absolute path, so it survives both
//!    `env_clear` and a redirected `HOME`. On this host Xcode ships one
//!    declaring `credential.helper=osxkeychain`. `GIT_CONFIG_NOSYSTEM=1` is
//!    therefore part of the environment, and [`the_system_gitconfig_cannot_be_read`]
//!    is the test that fails if it is ever dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use conductor_run::enforce::env::{
    AGENT_ENV_KEYS, RunEnvironment, materialize_credential_home, prepare,
};

/// Variable names an operator plausibly has, with values that are obviously not
/// real. §4.9 names exactly these classes: SSH agent, GitHub, cloud, database.
const CANARIES: &[(&str, &str)] = &[
    ("AWS_SECRET_ACCESS_KEY", "conductor-canary-not-a-real-key"),
    ("AWS_SESSION_TOKEN", "conductor-canary-session"),
    ("DATABASE_URL", "postgres://canary@127.0.0.1/canary"),
    ("GH_TOKEN", "conductor-canary-gh"),
    ("GITHUB_TOKEN", "conductor-canary-github"),
    ("SSH_AUTH_SOCK", "/tmp/conductor-canary-agent.sock"),
    ("ANTHROPIC_API_KEY", "conductor-canary-anthropic"),
    ("NETRC", "/tmp/conductor-canary-netrc"),
];

/// Serialises every test that mutates the process environment.
///
/// libtest runs tests in threads of one process, and `set_var`/`remove_var` are
/// process-global. Two tests planting and restoring the same names concurrently
/// will interleave — one restores `None` while the other is still asserting —
/// and the result is an intermittent failure that reproduces roughly never.
/// That is exactly the shape of the unattributable flake this repository is
/// already carrying one of, so it is prevented rather than tolerated.
static CANARY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Plant every canary in *this* process, so the child has something to inherit
/// if the boundary leaks. Restored on drop so one test cannot contaminate
/// another.
struct Canaries {
    previous: Vec<(&'static str, Option<String>)>,
    // Held for the lifetime of the planting, released when the values are back.
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Canaries {
    fn plant() -> Canaries {
        let guard = CANARY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut previous = Vec::new();
        for (name, value) in CANARIES {
            previous.push((*name, std::env::var(name).ok()));
            // SAFETY: the lock above makes this the only thread touching the
            // environment, and no child has been spawned yet.
            unsafe { std::env::set_var(name, value) };
        }
        Canaries {
            previous,
            _guard: guard,
        }
    }
}

impl Drop for Canaries {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

/// Run `env` the way `supervise::spawn` runs an agent: `env_clear`, then exactly
/// the map it was given.
fn child_env_isolated(env: &RunEnvironment) -> BTreeMap<String, String> {
    let output = Command::new("/usr/bin/env")
        .env_clear()
        .envs(env.vars())
        .output()
        .expect("/usr/bin/env is present on every supported host");
    parse_env_output(&output.stdout)
}

/// The **positive control**: the same measurement, same binary, same parsing —
/// with `env_clear` omitted. This is what a leak looks like. If this does not
/// observe the canaries then the isolated measurement proves nothing.
fn child_env_inherited(env: &RunEnvironment) -> BTreeMap<String, String> {
    let output = Command::new("/usr/bin/env")
        .envs(env.vars())
        .output()
        .expect("/usr/bin/env is present on every supported host");
    parse_env_output(&output.stdout)
}

fn parse_env_output(bytes: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut map = BTreeMap::new();
    for line in text.lines() {
        // Only `NAME=` lines start a variable; a value containing a newline
        // continues onto the next line and must not be read as a new name.
        if let Some((name, value)) = line.split_once('=')
            && !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            map.insert(name.to_string(), value.to_string());
        }
    }
    map
}

fn workspace(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("conductor-s9-{label}-"))
        .tempdir()
        .expect("tempdir")
}

#[test]
fn the_child_environment_is_exactly_the_allowlist() {
    let ws = workspace("allowlist");
    let env = prepare(ws.path()).expect("prepare");

    let observed = child_env_isolated(&env);
    let observed_names: BTreeSet<&str> = observed.keys().map(String::as_str).collect();
    let allowed: BTreeSet<&str> = AGENT_ENV_KEYS.iter().copied().collect();

    // Set equality, not containment. "Everything expected is present" would pass
    // for a child that also received forty variables nobody listed — which is
    // exactly the failure an allowlist exists to prevent.
    let unexpected: Vec<&&str> = observed_names.difference(&allowed).collect();
    assert!(
        unexpected.is_empty(),
        "the child saw variables no allowlist names: {unexpected:?}"
    );
    let missing: Vec<&&str> = allowed.difference(&observed_names).collect();
    assert!(
        missing.is_empty(),
        "the allowlist promises variables the child never saw: {missing:?}"
    );
}

#[test]
fn a_planted_credential_does_not_reach_the_child_and_the_control_proves_it_could() {
    let _canaries = Canaries::plant();
    let ws = workspace("canary");
    let env = prepare(ws.path()).expect("prepare");

    // The claim.
    let isolated = child_env_isolated(&env);
    for (name, _) in CANARIES {
        assert!(
            !isolated.contains_key(*name),
            "{name} crossed into the agent's environment"
        );
    }

    // The control. Same binary, same parser, same canaries — `env_clear`
    // removed. If this does not see them, the assertion above was measuring
    // nothing: the canaries were never planted, or `env` never ran, or the
    // parser silently returned empty.
    let inherited = child_env_inherited(&env);
    for (name, value) in CANARIES {
        assert_eq!(
            inherited.get(*name).map(String::as_str),
            Some(*value),
            "the positive control failed to observe {name}; this test cannot \
             detect a leak and its passing means nothing"
        );
    }
}

#[test]
fn ssh_auth_sock_is_absent_rather_than_emptied() {
    // An empty `SSH_AUTH_SOCK` is not the same as no `SSH_AUTH_SOCK`: some
    // clients read the empty value as "use the default socket path". §4.9 says
    // unset, and the map is built rather than filtered, so it must simply not
    // be a key.
    let _canaries = Canaries::plant();
    let ws = workspace("ssh");
    let env = prepare(ws.path()).expect("prepare");
    assert!(!env.vars().contains_key("SSH_AUTH_SOCK"));
    assert!(!child_env_isolated(&env).contains_key("SSH_AUTH_SOCK"));
}

#[test]
fn the_per_run_home_and_tmpdir_exist_on_disk_and_the_child_can_write_to_tmpdir() {
    let ws = workspace("home");
    let env = prepare(ws.path()).expect("prepare");

    let home = Path::new(env.vars().get("HOME").expect("HOME is allowlisted"));
    let tmpdir = Path::new(env.vars().get("TMPDIR").expect("TMPDIR is allowlisted"));

    // §4.9 promises a per-run HOME and a per-run TMPDIR "inside the workspace".
    // A variable pointing at a directory that does not exist is not isolation —
    // it is a broken agent, and a tool that falls back to the real /tmp on
    // `ENOENT` has quietly escaped the containment the variable promised.
    assert!(home.is_dir(), "{} is not a directory", home.display());
    assert!(tmpdir.is_dir(), "{} is not a directory", tmpdir.display());
    assert!(home.starts_with(ws.path()));
    assert!(tmpdir.starts_with(ws.path()));

    // Writable, proven by a child using the variable rather than by this
    // process using the path.
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg("printf ok > \"$TMPDIR/probe\" && cat \"$TMPDIR/probe\"")
        .env_clear()
        .envs(env.vars())
        .output()
        .expect("sh");
    assert!(
        out.status.success(),
        "a child could not write to its own TMPDIR: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
    assert!(tmpdir.join("probe").exists());
}

#[test]
fn home_relative_credential_lookup_finds_nothing() {
    // The mechanism §4.9 actually claims: `~/.aws`, `~/.config/gh` and
    // `~/.netrc` are *absent*, so ordinary discovery fails. This does **not**
    // claim an absolute-path read is prevented — SECURITY.md says so explicitly
    // (M12) — and the test proves only what is true.
    let ws = workspace("lookup");
    let env = prepare(ws.path()).expect("prepare");

    for relative in [
        ".aws/credentials",
        ".config/gh/hosts.yml",
        ".netrc",
        ".ssh/id_rsa",
        ".git-credentials",
    ] {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "test -e \"$HOME/{relative}\" && echo found || echo absent"
            ))
            .env_clear()
            .envs(env.vars())
            .output()
            .expect("sh");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "absent",
            "$HOME/{relative} was reachable from the agent's HOME"
        );
    }
}

#[test]
fn the_askpass_program_exists_and_always_fails() {
    // S9's first correction. `GIT_ASKPASS=/bin/false` was shipped at S5 and
    // names a path that does not exist on macOS — `false` is at
    // `/usr/bin/false`. It failed *closed*, because `GIT_TERMINAL_PROMPT=0`
    // catches the fallback, so nothing broke and nothing was noticed. But
    // SECURITY.md is about to claim "GIT_ASKPASS → a program that always exits
    // non-zero", and a claim whose named mechanism is absent is the exact class
    // of false safety statement S9 exists to eliminate.
    //
    // `.is_file()` is the assertion that would have caught it.
    let ws = workspace("askpass");
    let env = prepare(ws.path()).expect("prepare");

    let askpass = env.vars().get("GIT_ASKPASS").expect("GIT_ASKPASS");
    let askpass = Path::new(askpass);
    assert!(
        askpass.is_file(),
        "GIT_ASKPASS names {} which does not exist",
        askpass.display()
    );

    let out = Command::new(askpass)
        .arg("Password for 'https://example.invalid':")
        .env_clear()
        .envs(env.vars())
        .output()
        .expect("the askpass program must be executable");
    assert!(
        !out.status.success(),
        "GIT_ASKPASS returned success; it must always fail"
    );
    assert!(
        out.stdout.is_empty(),
        "GIT_ASKPASS printed something, which git would read as a credential"
    );
}

#[test]
fn the_system_gitconfig_cannot_be_read() {
    // S9's second correction, and the more serious one. A *system* gitconfig is
    // found by absolute path, so it survives `env_clear` and survives a
    // redirected `HOME`. On this host Xcode ships one at
    // `…/Developer/usr/share/git-core/gitconfig` declaring
    // `credential.helper=osxkeychain` — a credential source reachable by an
    // agent that adds its own remote, which is precisely what §4.9 layer 6
    // claims cannot happen.
    //
    // Positive control first: the setting must be observable *without* the
    // mitigation, or this test is asserting against a host that never had the
    // problem and would pass forever after the mitigation was deleted.
    let ws = workspace("nosystem");
    let env = prepare(ws.path()).expect("prepare");

    let mut without: BTreeMap<String, String> = env.vars().clone();
    without.remove("GIT_CONFIG_NOSYSTEM");
    let control = Command::new("git")
        .args(["config", "--list", "--show-origin"])
        .env_clear()
        .envs(&without)
        .current_dir(ws.path())
        .output()
        .expect("git");
    let control = String::from_utf8_lossy(&control.stdout).to_string();

    let guarded = Command::new("git")
        .args(["config", "--list", "--show-origin"])
        .env_clear()
        .envs(env.vars())
        .current_dir(ws.path())
        .output()
        .expect("git");
    let guarded = String::from_utf8_lossy(&guarded.stdout).to_string();

    // The claim: with the mitigation, no system-scoped setting is visible.
    assert!(
        !guarded.contains("credential.helper"),
        "a credential helper is reachable under the run environment:\n{guarded}"
    );

    // The control: on a host that ships a system gitconfig, dropping
    // `GIT_CONFIG_NOSYSTEM` must make it reappear. On a host that ships none,
    // there is nothing to suppress and the test cannot say anything — so it
    // says so rather than passing silently.
    if control.contains("credential.helper") {
        assert!(
            !guarded.contains("credential.helper"),
            "GIT_CONFIG_NOSYSTEM did not suppress the system credential helper"
        );
    } else {
        eprintln!(
            "note: this host ships no system-scoped credential.helper, so the \
             positive control could not demonstrate the leak. The mitigation is \
             still asserted above."
        );
    }
}

#[test]
fn the_run_scaffolding_is_invisible_to_the_repository() {
    // The per-run `HOME` and `TMPDIR` live *inside* the workspace (§4.9, M7),
    // which means the moment they exist they are untracked files in a git
    // repository. Left alone they would appear in `git status --porcelain` on
    // every single run, and §4.8 turns unexplained deltas into findings — so
    // every run would raise one, and a finding that is always present is a
    // finding nobody reads.
    //
    // They are excluded through `.git/info/exclude`, which is local to the
    // clone: nothing is added to the run branch, and the source repository's
    // `.gitignore` is untouched.
    let ws = workspace("scaffolding");
    let repo = ws.path();
    for args in [
        vec!["init", "--quiet", "-b", "main"],
        vec!["config", "user.email", "t@example.invalid"],
        vec!["config", "user.name", "t"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(repo)
                .args(&args)
                .output()
                .expect("git")
                .status
                .success()
        );
    }
    std::fs::write(repo.join("file.txt"), "content\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-qm", "base"]] {
        Command::new("git")
            .current_dir(repo)
            .args(&args)
            .output()
            .expect("git");
    }

    let env = prepare(repo).expect("prepare");
    // The agent uses its TMPDIR, as agents do.
    std::fs::write(env.tmpdir().join("scratch.bin"), b"junk").unwrap();
    std::fs::write(env.home().join(".some-tool-config"), b"junk").unwrap();

    let status = Command::new("git")
        .current_dir(repo)
        .args(["status", "--porcelain"])
        .output()
        .expect("git");
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.trim().is_empty(),
        "the run scaffolding is visible to git and would raise a finding on \
         every run:\n{status}"
    );
}

#[test]
fn git_cannot_prompt_for_a_credential() {
    // Asserted through git itself rather than by reading the variable back,
    // because the question is what git does with it.
    let ws = workspace("git");
    let env = prepare(ws.path()).expect("prepare");
    assert_eq!(
        env.vars().get("GIT_TERMINAL_PROMPT").map(String::as_str),
        Some("0")
    );

    let repo = ws.path().join("fixture");
    std::fs::create_dir_all(&repo).unwrap();
    let init = Command::new("git")
        .current_dir(&repo)
        .args(["init", "--quiet", "-b", "main"])
        .env_clear()
        .envs(env.vars())
        .output()
        .expect("git");
    assert!(init.status.success());

    // A disposable, unroutable URL. Never the real Conductor remote: a security
    // test must be safe even when the mechanism it tests has failed.
    let out = Command::new("git")
        .current_dir(&repo)
        .args(["fetch", "https://127.0.0.1:1/conductor-canary.git"])
        .env_clear()
        .envs(env.vars())
        .output()
        .expect("git");
    assert!(
        !out.status.success(),
        "a fetch to an unroutable host succeeded"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Username for") || stderr.contains("terminal prompts disabled"),
        "git tried to prompt for a username: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// §4.9's "the adapter's own auth variable", when the credential is a directory
// ---------------------------------------------------------------------------

#[test]
fn an_adapter_credential_home_carries_only_the_named_files() {
    // S10 measured that §4.9's phrasing does not fit Codex: `~/.codex/auth.json`
    // has `auth_mode: "chatgpt"` and a **null** `OPENAI_API_KEY` — the credential
    // is a token pair in a *file*, and what Codex needs is `CODEX_HOME`, a
    // directory pointer.
    //
    // Handing it the operator's real `~/.codex` is wrong in two directions.
    // It gives the agent read access to `config.toml`, every profile, and the
    // entire session history — and, worse, **Codex writes into `CODEX_HOME`**,
    // so a contained run would leave session rollouts in the operator's home:
    // outside the workspace, and outside §4.8's audit surface entirely.
    //
    // So Conductor materialises a per-run credential home containing only the
    // files the adapter actually needs. Everything else in the source directory
    // must not appear.
    let ws = workspace("credhome");
    prepare(ws.path()).expect("prepare");

    // A synthetic credential directory. Never the operator's real one: a test
    // that copied a live credential to prove copying works would have proved it
    // by doing the thing it is guarding against.
    let source = workspace("credsource");
    std::fs::write(
        source.path().join("auth.json"),
        r#"{"token":"canary-not-real"}"#,
    )
    .unwrap();
    std::fs::write(
        source.path().join("config.toml"),
        "model = \"secret-preference\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(source.path().join("sessions")).unwrap();
    std::fs::write(
        source.path().join("sessions").join("rollout.jsonl"),
        "{\"prior\":\"conversation\"}\n",
    )
    .unwrap();

    let home =
        materialize_credential_home(ws.path(), source.path(), &["auth.json"]).expect("materialize");

    assert!(
        home.join("auth.json").is_file(),
        "the credential was not copied"
    );
    assert!(
        !home.join("config.toml").exists(),
        "the operator's config followed the credential into the run"
    );
    assert!(
        !home.join("sessions").exists(),
        "the operator's session history followed the credential into the run"
    );
    assert!(
        home.starts_with(ws.path()),
        "the credential home must be inside the workspace, so it dies with the \
         run and is inside the audit surface: {}",
        home.display()
    );
}

#[test]
fn the_credential_home_is_owner_only_and_invisible_to_git() {
    let ws = workspace("credperms");
    // A git repository, so the exclusion can be observed rather than assumed.
    for args in [
        vec!["init", "--quiet", "-b", "main"],
        vec!["config", "user.email", "t@example.invalid"],
        vec!["config", "user.name", "t"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(ws.path())
                .args(&args)
                .output()
                .expect("git")
                .status
                .success()
        );
    }
    std::fs::write(ws.path().join("f.txt"), "x\n").unwrap();
    for args in [vec!["add", "-A"], vec!["commit", "-qm", "base"]] {
        Command::new("git")
            .current_dir(ws.path())
            .args(&args)
            .output()
            .expect("git");
    }
    prepare(ws.path()).expect("prepare");

    let source = workspace("credsource2");
    std::fs::write(
        source.path().join("auth.json"),
        r#"{"token":"canary-not-real"}"#,
    )
    .unwrap();
    let home =
        materialize_credential_home(ws.path(), source.path(), &["auth.json"]).expect("materialize");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::fs::metadata(&home).unwrap().permissions().mode();
        assert_eq!(
            dir & 0o077,
            0,
            "the credential home is group/other readable"
        );
        let file = std::fs::metadata(home.join("auth.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            file & 0o077,
            0,
            "the credential itself is group/other readable"
        );
    }

    // A credential must never become a commit.
    let status = Command::new("git")
        .current_dir(ws.path())
        .args(["status", "--porcelain"])
        .output()
        .expect("git");
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(
        status.trim().is_empty(),
        "the credential home is visible to git and could be committed:\n{status}"
    );
}

#[test]
fn a_credential_the_source_does_not_have_is_an_error_not_a_silent_empty_home() {
    // Failing closed matters here: a run that launched with an empty credential
    // home would fail to authenticate deep inside the agent, where the reason is
    // an opaque 401 rather than "the file you named is not there".
    let ws = workspace("credmissing");
    prepare(ws.path()).expect("prepare");
    let source = workspace("credempty");

    let error = materialize_credential_home(ws.path(), source.path(), &["auth.json"])
        .expect_err("a missing credential must be an error");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}
