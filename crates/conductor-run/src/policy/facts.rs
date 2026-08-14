//! Deterministic fact extractors — master plan §4.4's table.
//!
//! | Action | Deterministic fact source |
//! |---|---|
//! | `dependency.add.runtime` | diff of `[dependencies]` / `dependencies` in the manifest |
//! | `lockfile.modify` | path match on `Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`, `uv.lock` |
//! | `git.remote.modify` | `git config --get-regexp '^remote\.'` before vs after |
//! | `database.migration.create` | new file under configured migration globs |
//! | `filesystem.write.outside_workspace` | comparison against workspace root |
//! | `credential.access` | secret-pattern scan of the diff |
//! | `architecture.change` | **not deterministic** — see [`ProxyObservation`] |
//!
//! # Every function here is pure
//!
//! They take text — a manifest, a diff, `git config` output, a path list — and
//! return facts. Nothing here shells out. The reason is the one §4.4 gives for
//! calling these facts deterministic in the first place: a fact that depends on
//! when it was gathered is not reproducible, and an approval request carrying a
//! non-reproducible fact cannot be re-checked at grant time. The callers that
//! *do* run `git` ([`remote_config`]) are separated out and do nothing but
//! collect the text these functions read.
//!
//! # `architecture.change` is deliberately crippled
//!
//! §4.4: *"**not deterministic.** Path globs are a proxy → `model_assisted` →
//! `require_approval` at most, never `deny`, always with the diff attached."*
//!
//! [`architecture_change_proxy`] therefore does not return [`Fact`]s. It returns
//! [`ProxyObservation`], whose only exit is [`ProxyObservation::into_fact`], and
//! that function hard-codes [`FactSource::ModelAssisted`]. There is no
//! constructor and no field access that could produce a deterministic fact from
//! a path glob, so no future edit can promote a proxy into evidence strong
//! enough to block work.
//!
//! S7 does not produce model-assisted facts from a model — that is out of scope.
//! What it produces is the *type* that stops a future one denying.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use conductor_git::GitResult;

use super::model::{Fact, FactSource};
use crate::verify::secrets;

/// The fact keys this module produces.
///
/// Constants rather than string literals at each call site: a rule's `when:`
/// names these keys, so a typo in one place would silently make a rule never
/// apply — the failure mode §4.4's "negative results are what people debug"
/// exists to prevent.
pub mod key {
    /// A runtime dependency appeared in a manifest.
    pub const DEPENDENCY_ADDED: &str = "dependency_added";
    /// A development-only dependency appeared in a manifest.
    pub const DEV_DEPENDENCY_ADDED: &str = "dev_dependency_added";
    /// A dependency disappeared from a manifest.
    pub const DEPENDENCY_REMOVED: &str = "dependency_removed";
    /// A lockfile is among the changed paths.
    pub const LOCKFILE_MODIFIED: &str = "lockfile_modified";
    /// A `remote.*` git config entry was added, removed or changed.
    pub const REMOTE_MODIFIED: &str = "remote_modified";
    /// A new file appeared under a configured migration glob.
    pub const MIGRATION_ADDED: &str = "migration_added";
    /// A write landed outside the run workspace.
    pub const WRITE_OUTSIDE_WORKSPACE: &str = "write_outside_workspace";
    /// The secret detector matched something in the diff.
    pub const SECRET_MATCH: &str = "secret_match";
    /// Whether the repository is registered with Conductor. `"true"`/`"false"`.
    pub const REPOSITORY_REGISTERED: &str = "repository_registered";
    /// A path glob that *might* indicate an architectural change. Never
    /// deterministic — see [`super::ProxyObservation`].
    pub const ARCHITECTURE_CHANGE: &str = "architecture_change";
}

/// The lockfiles §4.4 names, plus the two spellings its list implies.
///
/// A path matches only on its final component: `notes/uv.lock.md` is a note
/// about a lockfile, not a lockfile.
pub const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "uv.lock",
    "yarn.lock",
    "poetry.lock",
    "Gemfile.lock",
    "go.sum",
];

/// Lockfiles among the changed paths — §4.4's `lockfile.modify` row.
pub fn lockfile_modified(changed_paths: &[String]) -> Vec<Fact> {
    changed_paths
        .iter()
        .filter(|path| {
            Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| LOCKFILES.contains(&name))
        })
        .map(|path| Fact::deterministic(key::LOCKFILE_MODIFIED, path))
        .collect()
}

/// Dependencies added to or removed from one manifest — §4.4's
/// `dependency.add.runtime` row.
///
/// Two manifest dialects, chosen by file name: TOML tables (`[dependencies]`,
/// `[dev-dependencies]`) and JSON objects (`dependencies`, `devDependencies`).
/// Anything else yields no facts, which is the honest answer: a fact this module
/// cannot derive deterministically must not be invented, because §4.4 lets a
/// deterministic fact carry a `deny`.
///
/// Runtime additions come first, then development ones, then removals — so that
/// the order of an approval request's facts does not depend on hash iteration.
pub fn dependency_manifest_diff(path: &str, before: &str, after: &str) -> Vec<Fact> {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);

    let (before_runtime, before_dev) = if name.ends_with(".json") {
        json_dependencies(before)
    } else {
        toml_dependencies(before)
    };
    let (after_runtime, after_dev) = if name.ends_with(".json") {
        json_dependencies(after)
    } else {
        toml_dependencies(after)
    };

    let mut facts = Vec::new();
    for added in after_runtime.difference(&before_runtime) {
        facts.push(Fact::deterministic(key::DEPENDENCY_ADDED, added).with_evidence(path));
    }
    for added in after_dev.difference(&before_dev) {
        facts.push(Fact::deterministic(key::DEV_DEPENDENCY_ADDED, added).with_evidence(path));
    }
    for removed in before_runtime
        .difference(&after_runtime)
        .chain(before_dev.difference(&after_dev))
    {
        facts.push(Fact::deterministic(key::DEPENDENCY_REMOVED, removed).with_evidence(path));
    }
    facts
}

/// `(runtime, development)` dependency names in a TOML manifest.
///
/// Hand-walked rather than parsed with a TOML crate: §2.2's dependency list has
/// no TOML parser, the shape being read is two table headers and the bare keys
/// under them, and the failure mode of getting it wrong is a *missing* fact —
/// which produces `allow` where a `require_approval` was wanted. That is the
/// direction this module must not fail in, so the tests plant every shape.
fn toml_dependencies(text: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut runtime = BTreeSet::new();
    let mut dev = BTreeSet::new();
    let mut section: Option<bool> = None; // Some(true) = development

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let header = header.trim().trim_start_matches("workspace.");
            // `[dependencies.serde]` names one dependency and opens its table.
            if let Some(rest) = header.strip_prefix("dependencies.") {
                runtime.insert(rest.to_string());
                section = None;
                continue;
            }
            if let Some(rest) = header
                .strip_prefix("dev-dependencies.")
                .or_else(|| header.strip_prefix("build-dependencies."))
            {
                dev.insert(rest.to_string());
                section = None;
                continue;
            }
            section = match header {
                "dependencies" => Some(false),
                "dev-dependencies" | "build-dependencies" => Some(true),
                _ => None,
            };
            continue;
        }
        let Some(is_dev) = section else { continue };
        let Some((name, _)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().trim_matches('"');
        if name.is_empty() {
            continue;
        }
        if is_dev {
            dev.insert(name.to_string());
        } else {
            runtime.insert(name.to_string());
        }
    }
    (runtime, dev)
}

/// `(runtime, development)` dependency names in a `package.json`.
fn json_dependencies(text: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        // Unparseable JSON yields no facts. A guess here would be a
        // deterministic fact that is not deterministic.
        return (BTreeSet::new(), BTreeSet::new());
    };
    let names = |field: &str| -> BTreeSet<String> {
        value
            .get(field)
            .and_then(|v| v.as_object())
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    };
    let mut dev = names("devDependencies");
    dev.extend(names("peerDependencies"));
    (names("dependencies"), dev)
}

/// `git config --get-regexp '^remote\.'` for one repository.
///
/// The one impure function in this module, and it does nothing but collect the
/// text [`remotes_changed`] compares.
pub fn remote_config(repo: &Path) -> GitResult<String> {
    let output = conductor_git::run_git(repo, &["config", "--get-regexp", "^remote\\."])?;
    // Exit status 1 means "no matches", which is a repository with no remotes —
    // a fact, not a failure.
    Ok(output.stdout_lossy().into_owned())
}

/// Remotes that changed between two `git config` snapshots — §4.4's
/// `git.remote.modify` row.
///
/// Compares whole `key value` lines, so a retargeted remote (same key, new URL)
/// is reported as well as an added or removed one. The fact's value is the
/// **key**, never the URL: a remote URL can carry credentials
/// (`https://user:token@host`), and a fact travels into approval requests and
/// review packets.
pub fn remotes_changed(before: &str, after: &str) -> Vec<Fact> {
    let parse = |text: &str| -> BTreeSet<(String, String)> {
        text.lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                let (key, value) = line.split_once(char::is_whitespace)?;
                Some((key.to_string(), value.trim().to_string()))
            })
            .collect()
    };
    let before = parse(before);
    let after = parse(after);

    let mut keys: BTreeSet<&String> = BTreeSet::new();
    for (key, _) in after.symmetric_difference(&before) {
        keys.insert(key);
    }
    keys.into_iter()
        .map(|key| Fact::deterministic(key::REMOTE_MODIFIED, key))
        .collect()
}

/// New files under the configured migration globs — §4.4's
/// `database.migration.create` row.
pub fn migrations_added(changed_paths: &[String], globs: &[String]) -> Vec<Fact> {
    changed_paths
        .iter()
        .filter(|path| {
            globs
                .iter()
                .any(|glob| conductor_git::glob_match(glob, path))
        })
        .map(|path| Fact::deterministic(key::MIGRATION_ADDED, path))
        .collect()
}

/// Paths that fall outside the workspace root — §4.4's
/// `filesystem.write.outside_workspace` row.
///
/// **Lexical, not `canonicalize`.** The paths being judged may not exist (a
/// write that was denied leaves nothing behind), and `canonicalize` on a missing
/// path is an error, which would turn "this escaped the workspace" into "we
/// could not tell". `..` is resolved textually, so `<root>/../escape` is
/// correctly outside.
pub fn outside_workspace(root: &Path, paths: &[PathBuf]) -> Vec<Fact> {
    let root = lexically_normal(root);
    paths
        .iter()
        .filter(|path| {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            !lexically_normal(&absolute).starts_with(&root)
        })
        .map(|path| Fact::deterministic(key::WRITE_OUTSIDE_WORKSPACE, path.display().to_string()))
        .collect()
}

/// Resolve `.` and `..` without touching the filesystem.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Secrets in a diff — §4.4's `credential.access` row.
///
/// Reuses `verify::secrets` rather than restating its patterns: two detectors
/// would drift, and the one that drifted downwards would be the one that
/// silently stopped reporting.
///
/// One fact per *kind*, never per occurrence, and the fact carries the kind and
/// a **redacted** excerpt. A fact travels into approval requests, findings and
/// review packets; a fact carrying the secret would defeat the invariant that
/// produced it.
pub fn secrets_in_diff(diff: &str) -> Vec<Fact> {
    let mut seen = BTreeSet::new();
    let mut facts = Vec::new();
    for line in diff.lines() {
        let redacted = secrets::redact(line);
        for kind in redacted.kinds {
            if seen.insert(kind) {
                facts.push(
                    Fact::deterministic(key::SECRET_MATCH, kind.label())
                        .with_evidence(redacted.text.clone()),
                );
            }
        }
    }
    facts
}

/// A path-glob hit that *suggests* an architectural change.
///
/// §4.4 line 629 refuses to call this deterministic, so this type refuses to
/// become a deterministic [`Fact`]. Its only exit is [`Self::into_fact`], which
/// hard-codes [`FactSource::ModelAssisted`], and evaluation caps a `deny` resting
/// on a model-assisted fact at `require_approval`. The consequence — never a
/// `deny`, always with the diff attached — is therefore a property of the types
/// rather than a rule someone has to remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyObservation {
    path: String,
    glob: String,
    diff: String,
}

impl ProxyObservation {
    /// The path that matched.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The glob it matched.
    pub fn glob(&self) -> &str {
        &self.glob
    }

    /// Convert to a fact. **Always model-assisted, always with the diff.**
    pub fn into_fact(self) -> Fact {
        debug_assert_eq!(
            Fact::model_assisted(key::ARCHITECTURE_CHANGE, &self.path).source,
            FactSource::ModelAssisted
        );
        Fact::model_assisted(key::ARCHITECTURE_CHANGE, self.path).with_evidence(self.diff)
    }
}

/// Path globs that suggest an architectural change — §4.4's `architecture.change`
/// row, which is explicitly **not** a deterministic fact source.
pub fn architecture_change_proxy(
    changed_paths: &[String],
    globs: &[String],
    diff: &str,
) -> Vec<ProxyObservation> {
    let mut out = Vec::new();
    for path in changed_paths {
        for glob in globs {
            if conductor_git::glob_match(glob, path) {
                out.push(ProxyObservation {
                    path: path.clone(),
                    glob: glob.clone(),
                    diff: diff.to_string(),
                });
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toml_sub_table_names_a_dependency() {
        let (runtime, dev) = toml_dependencies(
            "[dependencies.serde]\nversion = \"1\"\n\n[dev-dependencies.tempfile]\nversion = \"3\"\n",
        );
        assert!(runtime.contains("serde"));
        assert!(dev.contains("tempfile"));
    }

    #[test]
    fn a_comment_or_a_stray_key_outside_a_dependency_table_is_ignored() {
        let (runtime, dev) = toml_dependencies(
            "[package]\nname = \"a\"\nversion = \"0.1\"\n\n# a comment\n[dependencies]\n# another\nserde = \"1\"\n",
        );
        assert_eq!(runtime.iter().collect::<Vec<_>>(), ["serde"]);
        assert!(dev.is_empty());
    }

    #[test]
    fn a_build_dependency_counts_as_development() {
        let (runtime, dev) = toml_dependencies("[build-dependencies]\ncc = \"1\"\n");
        assert!(runtime.is_empty());
        assert!(dev.contains("cc"));
    }

    #[test]
    fn a_retargeted_remote_is_reported_by_key_and_never_by_url() {
        let facts = remotes_changed(
            "remote.origin.url https://alice:hunter2swordfish@github.com/a/b.git\n",
            "remote.origin.url https://github.com/a/b.git\n",
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].value, "remote.origin.url");
        assert!(!facts[0].value.contains("hunter2"));
    }

    #[test]
    fn a_removed_remote_is_a_change() {
        let facts = remotes_changed("remote.origin.url git@x:y.git\n", "");
        assert_eq!(facts.len(), 1);
    }

    #[test]
    fn a_relative_path_is_inside_the_workspace_unless_it_escapes() {
        let root = Path::new("/tmp/ws");
        let facts = outside_workspace(
            root,
            &[PathBuf::from("src/lib.rs"), PathBuf::from("../escape")],
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].value, "../escape");
    }

    #[test]
    fn unparseable_json_yields_no_facts_rather_than_a_guess() {
        assert!(dependency_manifest_diff("package.json", "{", "}").is_empty());
    }
}
