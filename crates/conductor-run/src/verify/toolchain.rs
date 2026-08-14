//! The toolchain fingerprint — master plan §4.5.
//!
//! ```yaml
//! toolchain_fingerprint:            # participates in the result cache key
//!   - "rustc --version"
//!   - "cargo --version"
//! ```
//!
//! It is a cache-key component and nothing else. Its whole job is that
//! upgrading, downgrading or **removing** a tool invalidates every cached
//! result that was produced with the old one — "passed *when*, on *what tree*,
//! with *what toolchain*" (§11.2).
//!
//! **A missing tool is a value, not an error.** If `rustc --version` cannot be
//! run, the fingerprint records that fact and therefore differs from the
//! fingerprint taken while it was present, so the cache misses and the check is
//! re-run. The re-run then fails to spawn and classifies `INCONCLUSIVE` —
//! infrastructure, not a defect. Erroring out here instead would collapse those
//! two distinct behaviours into one refusal to work.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command as OsCommand;

use serde::Serialize;

use super::profile::Command;

/// What one fingerprint command reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ProbeStatus {
    /// It ran and exited 0; the string is its trimmed stdout.
    Reported(String),
    /// It could not be started at all.
    Absent(String),
    /// It ran and exited non-zero.
    Failed {
        /// The exit code, when there was one.
        code: Option<i32>,
    },
}

/// One command's contribution to the fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolchainProbe {
    /// The command, as configured.
    pub command: String,
    /// What it said.
    pub status: ProbeStatus,
}

/// The fingerprint and the evidence behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolchainFingerprint {
    value: String,
    probes: Vec<ToolchainProbe>,
}

impl ToolchainFingerprint {
    /// The `blake3:<hex>` that goes in the cache key.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// What each command reported, sorted by command.
    pub fn probes(&self) -> &[ToolchainProbe] {
        &self.probes
    }

    /// Whether any configured tool could not be run.
    pub fn has_absent_tool(&self) -> bool {
        self.probes
            .iter()
            .any(|p| matches!(p.status, ProbeStatus::Absent(_)))
    }
}

/// Fingerprint the toolchain by running every configured command.
///
/// Probes are sorted by command before hashing, so re-ordering the lines of a
/// `verification.yaml` describes the same toolchain and does not invalidate a
/// single cached result. Nothing else about the encoding is negotiable: the
/// three [`ProbeStatus`] shapes are given distinct tags so that "reported
/// nothing", "could not be started" and "ran and failed" are three values, not
/// one empty string.
pub fn fingerprint(
    commands: &[Command],
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> ToolchainFingerprint {
    let mut probes: Vec<ToolchainProbe> = commands
        .iter()
        .map(|command| ToolchainProbe {
            command: command.to_string(),
            status: probe(command, cwd, env),
        })
        .collect();
    probes.sort_by(|a, b| a.command.cmp(&b.command));

    let mut bytes = Vec::new();
    // A version marker, so that a future change to this encoding is a cache
    // miss rather than a silent collision with results hashed the old way.
    bytes.extend_from_slice(b"conductor.toolchain.v1");
    bytes.push(0x1e);
    for probe in &probes {
        bytes.extend_from_slice(probe.command.as_bytes());
        bytes.push(0x1f);
        match &probe.status {
            ProbeStatus::Reported(output) => {
                bytes.extend_from_slice(b"reported");
                bytes.push(0x1f);
                bytes.extend_from_slice(output.as_bytes());
            }
            ProbeStatus::Absent(detail) => {
                bytes.extend_from_slice(b"absent");
                bytes.push(0x1f);
                bytes.extend_from_slice(detail.as_bytes());
            }
            ProbeStatus::Failed { code } => {
                bytes.extend_from_slice(b"failed");
                bytes.push(0x1f);
                bytes.extend_from_slice(
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string())
                        .as_bytes(),
                );
            }
        }
        bytes.push(0x1e);
    }

    ToolchainFingerprint {
        value: conductor_core::effect::content_hash(&bytes),
        probes,
    }
}

fn probe(command: &Command, cwd: &Path, env: &BTreeMap<String, String>) -> ProbeStatus {
    let argv = command.argv();
    let mut os = OsCommand::new(&argv[0]);
    os.args(&argv[1..])
        .current_dir(cwd)
        // §4.9's allowlist, for the same reason the agent gets one: a
        // fingerprint taken with the operator's whole environment would move
        // whenever anything unrelated in it moved.
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null());

    match os.output() {
        Err(error) => ProbeStatus::Absent(error.kind().to_string()),
        Ok(output) if output.status.success() => {
            ProbeStatus::Reported(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => ProbeStatus::Failed {
            code: output.status.code(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }
        env
    }

    fn commands(specs: &[&str]) -> Vec<Command> {
        specs
            .iter()
            .map(|s| Command::parse(s).expect("cmd"))
            .collect()
    }

    #[test]
    fn the_same_toolchain_fingerprints_the_same_way() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cmds = commands(&["/bin/echo rustc 1.97.0"]);
        assert_eq!(
            fingerprint(&cmds, dir.path(), &env()).as_str(),
            fingerprint(&cmds, dir.path(), &env()).as_str()
        );
    }

    #[test]
    fn a_different_version_string_is_a_different_fingerprint() {
        // This is the whole mechanism: upgrading a toolchain must invalidate
        // every cached result taken with the old one.
        let dir = tempfile::tempdir().expect("tempdir");
        let before = fingerprint(&commands(&["/bin/echo rustc 1.97.0"]), dir.path(), &env());
        let after = fingerprint(&commands(&["/bin/echo rustc 1.98.0"]), dir.path(), &env());
        assert_ne!(before.as_str(), after.as_str());
    }

    #[test]
    fn a_tool_that_has_been_removed_changes_the_fingerprint_rather_than_erroring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let present = fingerprint(&commands(&["/bin/echo rustc 1.97.0"]), dir.path(), &env());
        let removed = fingerprint(
            &commands(&["/nonexistent/bin/rustc --version"]),
            dir.path(),
            &env(),
        );

        assert_ne!(
            present.as_str(),
            removed.as_str(),
            "removing a tool must invalidate the cache"
        );
        assert!(removed.has_absent_tool());
        assert!(matches!(removed.probes()[0].status, ProbeStatus::Absent(_)));
        assert!(!present.has_absent_tool());
    }

    #[test]
    fn a_tool_that_runs_and_fails_is_distinguished_from_one_that_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let failed = fingerprint(&commands(&["/usr/bin/false"]), dir.path(), &env());
        let absent = fingerprint(&commands(&["/nonexistent/false"]), dir.path(), &env());
        assert_ne!(failed.as_str(), absent.as_str());
        assert!(matches!(
            failed.probes()[0].status,
            ProbeStatus::Failed { .. }
        ));
    }

    #[test]
    fn listing_the_same_commands_in_another_order_is_the_same_toolchain() {
        // A profile is edited by humans. Moving two lines describes the same
        // toolchain, and busting every cached result over it would be a cost
        // with no corresponding fact behind it.
        let dir = tempfile::tempdir().expect("tempdir");
        let a = fingerprint(
            &commands(&["/bin/echo one", "/bin/echo two"]),
            dir.path(),
            &env(),
        );
        let b = fingerprint(
            &commands(&["/bin/echo two", "/bin/echo one"]),
            dir.path(),
            &env(),
        );
        assert_eq!(a.as_str(), b.as_str());
    }

    #[test]
    fn a_profile_with_no_fingerprint_commands_still_has_a_stable_value() {
        // §4.5 does not require the section. An empty fingerprint is a fixed
        // constant rather than an empty string, so a key built from it is still
        // four components wide.
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = fingerprint(&[], dir.path(), &env());
        assert_eq!(
            empty.as_str(),
            fingerprint(&[], dir.path(), &env()).as_str()
        );
        assert!(empty.as_str().starts_with("blake3:"));
        assert!(!empty.has_absent_tool());
    }

    #[test]
    fn the_probes_are_evidence_a_human_can_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = fingerprint(&commands(&["/bin/echo rustc 1.97.0"]), dir.path(), &env());
        assert_eq!(f.probes()[0].command, "/bin/echo rustc 1.97.0");
        assert_eq!(
            f.probes()[0].status,
            ProbeStatus::Reported("rustc 1.97.0".to_string())
        );
    }
}
