//! §4.9's allowlisted environment, built rather than filtered.
//!
//! > An **allowlisted** environment (`PATH`, redirected `HOME`, `LANG`, `TERM`,
//! > the adapter's own auth variable, nothing else). Not a denylist — a denylist
//! > misses the next variable name.
//!
//! The map returned here is the *complete* environment of the agent process.
//! `supervise::spawn` calls `env_clear` before applying it, so nothing arrives
//! by inheritance and nothing can be forgotten: adding a variable requires
//! naming it here.
//!
//! # Why [`prepare`] does I/O and there is no pure alternative
//!
//! S5 shipped a pure `agent_environment()` that pointed `HOME` and `TMPDIR` at
//! directories **nobody created**. A variable naming a directory that does not
//! exist is not isolation. Depending on the tool it is either an immediate
//! failure or — worse — a silent fallback to the real `/tmp`, which is the
//! containment hole the variable was supposed to close.
//!
//! So the only way to obtain a [`RunEnvironment`] is [`prepare`], which creates
//! the directories and writes the askpass program first. The environment cannot
//! be constructed without the filesystem state it promises, because the type has
//! no other constructor. That is deliberate: the S5 bug was not a missing
//! `mkdir`, it was an API that let a caller *have* the environment without it.
//!
//! # Two corrections S9 measured
//!
//! **`/bin/false` is not a file on macOS.** S5 set `GIT_ASKPASS=/bin/false`;
//! macOS puts `false` at `/usr/bin/false`. Git reported `cannot exec
//! '/bin/false'` and then refused to prompt because `GIT_TERMINAL_PROMPT=0` — so
//! the outcome was safe and the *named mechanism* was absent. SECURITY.md is
//! about to claim this mechanism by name, so it is now a file Conductor writes
//! itself, into the per-run `HOME`, whose existence a test asserts.
//!
//! **A system gitconfig outlives both `env_clear` and a redirected `HOME`.** It
//! is located by absolute path. On a macOS host with Xcode's git, that file
//! declares `credential.helper=osxkeychain`: an agent that adds its own remote
//! could ask the keychain for a credential, defeating §4.9's layer 6 without
//! touching a single environment variable. `GIT_CONFIG_NOSYSTEM=1` closes it,
//! and `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` are pinned to `/dev/null` as
//! belt and braces for git versions that read them.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// Every variable name the agent's environment may contain.
///
/// The test that matters asserts **set equality** between this list and what a
/// child actually observes — not containment. A child that receives an extra
/// variable nobody listed is the failure an allowlist exists to prevent, and a
/// containment assertion would not see it.
///
/// `agent_env_extra` (§6.1's "the adapter's own auth variable") is *not* here:
/// it is added per adapter, by name, at the call site, and
/// [`RunEnvironment::with_extra`] is the only way in.
pub const AGENT_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "TERM",
    "GIT_TERMINAL_PROMPT",
    "GIT_ASKPASS",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
];

/// The per-run `HOME`, relative to the workspace.
pub const HOME_DIR: &str = ".conductor-home";
/// The per-run `TMPDIR`, relative to the workspace (§4.9, M7).
pub const TMP_DIR: &str = ".conductor-tmp";
/// The askpass program, relative to the per-run `HOME`.
pub const ASKPASS_FILE: &str = "askpass-always-fails";
/// The per-run credential home, relative to the workspace.
pub const AGENT_HOME_DIR: &str = ".conductor-agent-home";

/// A program that always exits non-zero and prints nothing.
///
/// Written per run rather than referenced from the host, because the host path
/// that S5 named does not exist on the primary supported platform.
const ASKPASS_SOURCE: &str = "#!/bin/sh\n\
                              # Conductor: git must never obtain a credential (master plan §4.9).\n\
                              # Prints nothing on stdout — git reads stdout as the credential.\n\
                              exit 1\n";

/// The complete environment of one run's child processes, with the filesystem
/// state it names already in place.
///
/// Obtainable only from [`prepare`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnvironment {
    vars: BTreeMap<String, String>,
    home: PathBuf,
    tmpdir: PathBuf,
}

impl RunEnvironment {
    /// The variables, as `spawn` applies them after `env_clear`.
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// The variables, consumed.
    pub fn into_vars(self) -> BTreeMap<String, String> {
        self.vars
    }

    /// The per-run `HOME`.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The per-run `TMPDIR`.
    pub fn tmpdir(&self) -> &Path {
        &self.tmpdir
    }

    /// Add §4.9's last clause — "the adapter's own auth variable" — by name.
    ///
    /// Additive and explicit. There is no filtering step anywhere in this
    /// module, so an adapter that needs a token must say which one, and the
    /// allowlist test will show it as an extra key rather than hiding it.
    pub fn with_extra(mut self, extra: &BTreeMap<String, String>) -> RunEnvironment {
        self.vars.extend(extra.clone());
        self
    }
}

/// Build the run environment and create everything it points at.
///
/// Idempotent: a second attempt in the same workspace reuses the directories
/// rather than failing, because §4.7's recovery may re-enter this path after a
/// crash.
pub fn prepare(workspace: &Path) -> io::Result<RunEnvironment> {
    let home = workspace.join(HOME_DIR);
    let tmpdir = workspace.join(TMP_DIR);
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&tmpdir)?;

    let askpass = home.join(ASKPASS_FILE);
    write_askpass(&askpass)?;
    hide_from_git(workspace)?;

    let mut vars = BTreeMap::new();
    // `PATH` because agents run tools. Inherited *by value* — it is the one
    // variable whose absence would make every adapter unusable — and it is a
    // search path, not a credential.
    vars.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
    );
    vars.insert("HOME".to_string(), home.display().to_string());
    vars.insert("TMPDIR".to_string(), tmpdir.display().to_string());
    vars.insert("LANG".to_string(), "C".to_string());
    vars.insert("TERM".to_string(), "dumb".to_string());

    // §4.9: git must never be able to prompt for, or find, a credential.
    vars.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    vars.insert("GIT_ASKPASS".to_string(), askpass.display().to_string());
    // The system gitconfig is read by absolute path and would otherwise survive
    // every other control in this function.
    vars.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
    vars.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
    vars.insert("GIT_CONFIG_SYSTEM".to_string(), "/dev/null".to_string());

    debug_assert!(
        vars.keys().all(|k| AGENT_ENV_KEYS.contains(&k.as_str()))
            && AGENT_ENV_KEYS.iter().all(|k| vars.contains_key(*k)),
        "AGENT_ENV_KEYS and the built map disagree"
    );

    Ok(RunEnvironment { vars, home, tmpdir })
}

/// Keep the run scaffolding out of `git status`.
///
/// The per-run `HOME` and `TMPDIR` live inside the workspace, so from git's
/// point of view they are untracked files. §4.8 turns unexplained deltas into
/// findings, and a finding raised by *every* run is a finding nobody reads — the
/// audit's signal would be buried under Conductor's own scaffolding on the first
/// day.
///
/// Written to `.git/info/exclude`, which is local to the clone: nothing joins
/// the run branch and the operator's `.gitignore` is untouched. This mirrors
/// `conductor_git::descriptor::exclude_descriptor_locally`, which does the same
/// thing for the run descriptor, and is done here rather than there because the
/// module that invents these directory names is the one that should hide them.
///
/// **Excluded from git is not excluded from the audit.** These directories are
/// agent-writable, so [`super::audit`] reads what the agent left in them — being
/// invisible to `git status` is precisely why something else has to look.
///
/// A workspace that is not a git repository is not an error: tests and the
/// probe harness prepare environments in plain directories.
fn hide_from_git(workspace: &Path) -> io::Result<()> {
    let git_dir = workspace.join(".git");
    if !git_dir.is_dir() {
        return Ok(());
    }
    let info = git_dir.join("info");
    std::fs::create_dir_all(&info)?;
    let exclude = info.join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let mut contents = existing.clone();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    let mut added = false;
    for name in [HOME_DIR, TMP_DIR, AGENT_HOME_DIR] {
        let line = format!("/{name}");
        // Idempotent: recovery re-enters this path, and appending the same two
        // lines on every attempt would grow the file without bound.
        if existing.lines().any(|l| l.trim() == line) {
            continue;
        }
        if !added {
            contents.push_str(
                "# Conductor's per-run HOME and TMPDIR (master plan §4.9).\n\
                 # Local to this clone; never part of the run branch.\n",
            );
            added = true;
        }
        contents.push_str(&line);
        contents.push('\n');
    }
    if !added {
        return Ok(());
    }
    std::fs::write(&exclude, contents)
}

fn write_askpass(path: &Path) -> io::Result<()> {
    // The file is left mode `0500`, so a second `prepare` — which §4.7's
    // recovery performs whenever it re-enters an existing workspace — cannot
    // overwrite it in place. Removing first is what makes preparation
    // idempotent, and it also means an askpass an *agent* replaced is
    // reinstated from Conductor's own source on the next attempt rather than
    // trusted because it happens to exist.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::write(path, ASKPASS_SOURCE)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Owner-only: an executable an agent could rewrite is an executable
        // Conductor hands git without knowing what it does. `0500` is
        // read+execute with no write bit, even for the owner.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500))?;
    }
    Ok(())
}

/// What an adapter needs copied into its per-run credential home, and the
/// variable that names it.
///
/// # Why this is a *request* the caller states, and not something inferred
///
/// [`materialize_credential_home`] cannot run until the workspace exists, and
/// the workspace does not exist until the run has been claimed and cloned — so
/// the value of `CODEX_HOME` is not knowable when a caller builds its
/// configuration. `agent_env_extra` holds the variables whose values *are*
/// knowable then; this holds the one that is not.
///
/// It stays a request rather than becoming a method on the adapter for the same
/// reason §4.9's allowlist is a build rather than a filter: the caller that
/// chose the adapter names the variable, by hand, in a place a reader can find
/// it. An adapter that could quietly ask for a credential would be a filter
/// wearing an allowlist's clothes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialHomeRequest {
    /// The variable the agent reads — `CODEX_HOME` for §6.2's adapter.
    pub variable: String,
    /// The operator's own credential directory. Read by Conductor, **never**
    /// handed to the agent: see [`materialize_credential_home`].
    pub source: PathBuf,
    /// The file names to copy out of `source`. Only these.
    pub files: Vec<String>,
}

/// Materialise a per-run credential directory for an adapter that needs one.
///
/// # Why a directory, when §4.9 says "the adapter's own auth **variable**"
///
/// Because for the first real adapter it is not a variable. S10 measured
/// `~/.codex/auth.json` on this host: `auth_mode` is `"chatgpt"` and
/// `OPENAI_API_KEY` is **null** — the credential is a refresh/access token pair
/// in a *file*, and what Codex reads is `CODEX_HOME`, a directory pointer. The
/// clause in §4.9 was written for the API-key case and does not cover it.
///
/// # Why not simply point the variable at the operator's real directory
///
/// Two reasons, and the second is the one that matters.
///
/// It would hand the agent read access to `config.toml`, every profile file,
/// and the entire session history — none of which is a credential and all of
/// which is the operator's.
///
/// And **the agent writes there**. A run pointed at the real `~/.codex` leaves
/// session rollouts in the operator's home: outside the workspace, outside
/// §4.8's reconciled surface, and outside the per-run `TMPDIR` audit. The
/// containment story would have a hole shaped exactly like the credential that
/// was supposed to be its foundation.
///
/// So the run gets its own directory, inside the workspace, containing **only**
/// the files named — `0700`, with each file `0600`. It dies with the workspace,
/// it is inside the audit surface, and it is excluded from git by [`prepare`]
/// so a credential can never become a commit.
///
/// # Fails closed on a missing file
///
/// A credential that is not there is an error here, not an empty directory. An
/// empty credential home fails deep inside the agent as an opaque `401`, at
/// which point the reason — "the file you named does not exist" — is several
/// layers away from the symptom.
///
/// The caller passes the returned path to the adapter by name, through
/// `agent_env_extra`. Nothing here decides which variable: that is adapter
/// knowledge, and the allowlist test will show it as an extra key rather than
/// letting it in silently.
pub fn materialize_credential_home(
    workspace: &Path,
    source: &Path,
    files: &[&str],
) -> io::Result<PathBuf> {
    let home = workspace.join(AGENT_HOME_DIR);
    std::fs::create_dir_all(&home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    }

    for name in files {
        let from = source.join(name);
        if !from.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "the adapter needs {} from {}, and it is not there; refusing to \
                     launch with an empty credential home rather than failing as a \
                     401 inside the agent",
                    name,
                    source.display()
                ),
            ));
        }
        let to = home.join(name);
        // Remove first: `prepare` may have run before, and a `0600` file cannot
        // be overwritten in place on a second attempt.
        match std::fs::remove_file(&to) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::copy(&from, &to)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparing_twice_is_not_an_error() {
        // §4.7's recovery re-enters the attempt path after a crash, and the
        // workspace it re-enters already has these directories.
        let dir = tempfile::tempdir().unwrap();
        let first = prepare(dir.path()).expect("first");
        let second = prepare(dir.path()).expect("second");
        assert_eq!(first, second);
    }

    #[test]
    fn the_askpass_file_is_not_writable() {
        let dir = tempfile::tempdir().unwrap();
        let env = prepare(dir.path()).unwrap();
        let askpass = Path::new(env.vars().get("GIT_ASKPASS").unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(askpass).unwrap().permissions().mode();
            assert_eq!(mode & 0o222, 0, "the askpass program is writable");
            assert_ne!(mode & 0o100, 0, "the askpass program is not executable");
        }
    }

    #[test]
    fn an_extra_variable_is_added_and_nothing_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let base = prepare(dir.path()).unwrap();
        let mut extra = BTreeMap::new();
        extra.insert("CODEX_API_KEY".to_string(), "x".to_string());
        let with = base.clone().with_extra(&extra);
        assert_eq!(
            with.vars().get("CODEX_API_KEY").map(String::as_str),
            Some("x")
        );
        for key in AGENT_ENV_KEYS {
            assert!(with.vars().contains_key(*key), "{key} was lost");
        }
    }
}
