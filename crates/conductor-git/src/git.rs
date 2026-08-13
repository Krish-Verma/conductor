//! Running `git` as a subprocess.
//!
//! Master plan §2.2: git is invoked as a subprocess, **never** via `libgit2` or
//! `gix`. Conductor's job is to observe the state the operator observes, and the
//! operator's ground truth is the `git` binary, including their `core.*`
//! settings, hooks and filters. The environment is therefore inherited
//! unmodified — a scrubbed environment would make Conductor observe a repository
//! nobody else sees.

use std::borrow::Cow;
use std::path::Path;
use std::process::Command;

use crate::error::{GitError, GitResult};

/// stderr is kept for diagnosis, not for logs; anything longer than this is
/// noise and CLAUDE.md forbids carrying large logs around.
const STDERR_LIMIT: usize = 4096;

/// What one `git` invocation produced.
#[derive(Debug, Clone)]
pub struct GitOutput {
    /// Arguments passed, for error messages.
    pub args: Vec<String>,
    /// Exit code, or `None` when the process was terminated by a signal.
    pub code: Option<i32>,
    /// Raw stdout. Bytes, not `String`: `--porcelain=v2 -z` and
    /// `config --list -z` are NUL-separated and paths are not required to be
    /// UTF-8.
    pub stdout: Vec<u8>,
    /// stderr, truncated to [`STDERR_LIMIT`].
    pub stderr: String,
}

impl GitOutput {
    /// Whether the command exited 0.
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// stdout as text, replacing anything that is not UTF-8. Only for output
    /// git itself defines as text (hashes, refnames, config).
    pub fn stdout_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// stdout as text with the trailing newline removed. For single-value
    /// commands such as `rev-parse`.
    pub fn stdout_trimmed(&self) -> String {
        self.stdout_lossy()
            .trim_end_matches(['\n', '\r'])
            .to_string()
    }

    fn status_text(&self) -> String {
        match self.code {
            Some(code) => code.to_string(),
            None => "by signal".to_string(),
        }
    }

    fn into_error(self) -> GitError {
        GitError::Command {
            args: self.args.join(" "),
            status: self.status_text(),
            stderr: self.stderr,
        }
    }
}

/// Run `git` in `cwd` and return what happened, including failure.
///
/// Only a spawn failure is an `Err`: a non-zero exit is the ordinary way git
/// reports facts Conductor needs to classify.
pub fn run_git(cwd: &Path, args: &[&str]) -> GitResult<GitOutput> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|source| GitError::GitUnavailable { source })?;

    let mut stderr = String::from_utf8_lossy(&output.stderr)
        .trim_end()
        .to_string();
    if stderr.len() > STDERR_LIMIT {
        stderr.truncate(STDERR_LIMIT);
        stderr.push_str(" …[truncated]");
    }

    Ok(GitOutput {
        args: args.iter().map(|a| (*a).to_string()).collect(),
        code: output.status.code(),
        stdout: output.stdout,
        stderr,
    })
}

/// Run `git` in `cwd` and require exit 0.
pub fn run_git_ok(cwd: &Path, args: &[&str]) -> GitResult<GitOutput> {
    let out = run_git(cwd, args)?;
    if out.ok() {
        Ok(out)
    } else {
        Err(out.into_error())
    }
}

/// Run `git` in `cwd`, feeding `input` on stdin, and require exit 0.
pub fn run_git_stdin(cwd: &Path, args: &[&str], input: &[u8]) -> GitResult<GitOutput> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| GitError::GitUnavailable { source })?;

    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input)
        .map_err(|source| GitError::GitUnavailable { source })?;

    let output = child
        .wait_with_output()
        .map_err(|source| GitError::GitUnavailable { source })?;

    let out = GitOutput {
        args: args.iter().map(|a| (*a).to_string()).collect(),
        code: output.status.code(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr)
            .trim_end()
            .to_string(),
    };
    if out.ok() {
        Ok(out)
    } else {
        Err(out.into_error())
    }
}

/// Split NUL-separated output into records, dropping the empty tail.
pub fn nul_records(stdout: &[u8]) -> Vec<String> {
    stdout
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_command_reports_its_stdout_and_zero_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = run_git(dir.path(), &["--version"]).expect("git runs");
        assert!(out.ok(), "expected exit 0, got {:?}", out.code);
        assert!(
            out.stdout_lossy().starts_with("git version"),
            "unexpected stdout: {:?}",
            out.stdout_lossy()
        );
    }

    #[test]
    fn a_non_zero_exit_is_data_not_an_error() {
        // Reconciliation has to classify a broken repository, so the runner must
        // hand back failures rather than short-circuit them.
        let dir = tempfile::tempdir().expect("tempdir");
        let out = run_git(dir.path(), &["rev-parse", "--git-dir"]).expect("git runs");
        assert!(!out.ok(), "expected non-zero outside a repository");
        assert!(
            out.stderr.contains("not a git repository"),
            "unexpected stderr: {:?}",
            out.stderr
        );
    }

    #[test]
    fn run_git_ok_converts_a_non_zero_exit_into_an_error_naming_the_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = run_git_ok(dir.path(), &["rev-parse", "--git-dir"]).expect_err("must fail");
        let rendered = err.to_string();
        assert!(
            rendered.contains("rev-parse"),
            "error must name the command: {rendered}"
        );
        assert!(
            rendered.contains("not a git repository"),
            "error must carry stderr: {rendered}"
        );
    }

    #[test]
    fn stdout_is_bytes_so_nul_separated_output_survives() {
        // `--porcelain=v2 -z` and `config --list -z` are NUL-separated; a runner
        // that returned `String` and split on lines would silently merge records
        // for paths containing newlines.
        let dir = tempfile::tempdir().expect("tempdir");
        run_git_ok(dir.path(), &["init", "-q", "-b", "main"]).expect("init");
        run_git_ok(dir.path(), &["config", "--local", "a.b", "one\ntwo"]).expect("set config");
        let out = run_git_ok(dir.path(), &["config", "--local", "--list", "-z"]).expect("list");
        assert!(
            out.stdout.contains(&0u8),
            "expected NUL bytes in raw stdout: {:?}",
            out.stdout_lossy()
        );
        assert!(
            out.stdout_lossy().contains("a.b\none\ntwo"),
            "multi-line value must survive: {:?}",
            out.stdout_lossy()
        );
    }
}
