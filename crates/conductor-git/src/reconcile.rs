//! `reconcile()` — master plan §4.8.
//!
//! **Pure.** No filesystem, no subprocess, no clock. Observation (`baseline.rs`)
//! runs git; classification runs nothing. That split is what §2.1 means by "the
//! domain is exhaustively testable without a runtime", and it is the reason a
//! verdict can be reproduced from stored evidence years later.
//!
//! Every exit from `RUNNING` passes through here — success, crash, timeout,
//! cancel. This is the invariant that makes an agent's self-report
//! non-authoritative.
//!
//! ## Precedence
//!
//! Exactly one verdict comes out, so the order the conditions are checked in is
//! a design decision rather than an implementation detail:
//!
//! 1. `CORRUPT` — a repository that cannot be read cannot be classified.
//! 2. `CONTRADICTED` — the report asserted something git denies. Git wins.
//! 3. `POLICY_SENSITIVE` — dependency, lockfile, migration, or repository
//!    structure (config, remotes, refs, hooks, stash, submodules) changed.
//! 4. `OUT_OF_SCOPE` — a changed path is outside the declared scope.
//! 5. `NO_CHANGE` — nothing changed at all.
//! 6. `CLEAN_COMPLETE` — changes in scope, report present and consistent.
//! 7. `CLEAN_NO_REPORT` — changes in scope, no report.
//!
//! `POLICY_SENSITIVE` sits above `NO_CHANGE` deliberately. Acceptance row 14 is
//! a `git remote set-url` inside the clone: the tree is byte-identical
//! afterwards, so a classifier that reached for `NO_CHANGE` first would report
//! that nothing happened.
//!
//! ## What is not here
//!
//! §4.8's reconciled surface also names a **secret-pattern scan over the whole
//! diff** (S9) and a **`/tmp` delta during the attempt window** (S7/S9, and
//! meaningful only once there is a per-run `TMPDIR` to diff). Neither is stubbed:
//! a function that returns "no secrets found" without looking is worse than an
//! absent one, because it reads as working. Policy *evaluation* is S7; what S2
//! owns is the verdict that says evaluation is required.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::baseline::{Baseline, Observed};

// `AgentReport` moved to `conductor-core` at S3: the adapter layer produces it
// and reconciliation consumes it, so it belongs in the crate both depend on
// rather than in the one that happens to have needed it first. Re-exported so
// that `conductor_git::AgentReport` keeps meaning what it did.
pub use conductor_core::{AgentReport, ReportClaim};

/// The seven §4.8 verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    /// Tree identical to baseline.
    NoChange,
    /// Changes in scope, report present and consistent.
    CleanComplete,
    /// Changes in scope, no report.
    CleanNoReport,
    /// Changes outside declared scope.
    OutOfScope,
    /// Dependencies, lockfiles, migrations or repository structure touched.
    PolicySensitive,
    /// The report contradicts observed state.
    Contradicted,
    /// The repository is broken.
    Corrupt,
}

/// §4.5's completion criterion 6: "Reconciliation verdict ∈ {`CLEAN_COMPLETE`,
/// `CLEAN_NO_REPORT`}".
///
/// The conversion lives here because [`Verdict`] lives here, while the
/// criterion lives in `conductor-core`, which has no dependencies. Exhaustive
/// on purpose: an eighth verdict added to §4.8 will not compile until somebody
/// decides whether it may complete a task, which is the opposite of defaulting
/// to "clean".
impl From<Verdict> for conductor_core::completion::ReconciliationEvidence {
    fn from(verdict: Verdict) -> Self {
        use conductor_core::completion::ReconciliationEvidence as Evidence;
        match verdict {
            Verdict::CleanComplete | Verdict::CleanNoReport => Evidence::Clean,
            Verdict::NoChange
            | Verdict::OutOfScope
            | Verdict::PolicySensitive
            | Verdict::Contradicted
            | Verdict::Corrupt => Evidence::NotClean {
                verdict: verdict.to_string(),
            },
        }
    }
}

impl Verdict {
    /// The exact string persisted for this verdict.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::NoChange => "NO_CHANGE",
            Verdict::CleanComplete => "CLEAN_COMPLETE",
            Verdict::CleanNoReport => "CLEAN_NO_REPORT",
            Verdict::OutOfScope => "OUT_OF_SCOPE",
            Verdict::PolicySensitive => "POLICY_SENSITIVE",
            Verdict::Contradicted => "CONTRADICTED",
            Verdict::Corrupt => "CORRUPT",
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of delta a finding records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FindingKind {
    /// A local config key was added, removed or changed.
    ConfigChanged,
    /// A remote was added, removed or repointed.
    RemoteChanged,
    /// A ref moved.
    RefChanged,
    /// A ref appeared.
    RefAdded,
    /// A ref was deleted.
    RefRemoved,
    /// A hook file appeared.
    HookAdded,
    /// A hook file changed.
    HookChanged,
    /// A hook file was removed.
    HookRemoved,
    /// The stash list changed.
    StashChanged,
    /// `git submodule status` changed.
    SubmoduleChanged,
    /// A nested repository's own state changed.
    NestedRepoModified,
    /// A nested repository appeared or disappeared.
    NestedRepoChanged,
    /// A changed path lies outside the declared scope.
    OutOfScopePath,
    /// A changed path is a dependency manifest, lockfile or migration.
    PolicySensitivePath,
    /// The report asserted something the repository denies.
    ReportContradicted,
    /// The repository changed in a way the report does not mention.
    ReportOmittedChange,
    /// The repository cannot be read or is mid-operation.
    RepositoryCorrupt,
}

impl FindingKind {
    /// Whether this delta on its own requires policy evaluation before the run
    /// advances (§4.8 `POLICY_SENSITIVE`).
    ///
    /// Config, remotes, refs, hooks, the stash and submodules are repository
    /// *structure*: nothing a task's declared scope can authorise, because a
    /// scope names working-tree paths and none of these live there.
    ///
    /// Nested repositories are deliberately **not** in this set. §4.1 says a
    /// modified nested repository "raise[s] a finding" — it does not say it
    /// halts the run, and §4.8 defines `POLICY_SENSITIVE` as
    /// deps/lockfile/migrations/git-config. The finding still never
    /// auto-resolves, so it reaches a human at review either way.
    pub fn forces_policy_evaluation(&self) -> bool {
        matches!(
            self,
            FindingKind::ConfigChanged
                | FindingKind::RemoteChanged
                | FindingKind::RefChanged
                | FindingKind::RefAdded
                | FindingKind::RefRemoved
                | FindingKind::HookAdded
                | FindingKind::HookChanged
                | FindingKind::HookRemoved
                | FindingKind::StashChanged
                | FindingKind::SubmoduleChanged
                | FindingKind::PolicySensitivePath
        )
    }
}

/// One unexplained delta.
///
/// §4.8: **findings never auto-resolve.** That is enforced structurally — a
/// [`Reconciliation`] exposes its findings by shared reference and offers no way
/// to remove one. A later, cleaner reconciliation produces a *new* record; it
/// does not retract an earlier one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// What kind of delta this is.
    pub kind: FindingKind,
    /// Human-readable evidence.
    pub detail: String,
    /// The path, refname or config key involved.
    pub path: Option<String>,
}

/// A verification result, per the §4.5 outcome set.
///
/// S2 defined this enum here, because here was the only place that used it: it
/// is recorded as evidence attached to the reconciliation and does not
/// influence the verdict (acceptance row 24 — policy wins over green tests, so
/// a `PASS` must never soften anything).
///
/// **S4 moved it to `conductor-core`.** It became a persisted value —
/// `verification_check.outcome` — and `conductor-store` cannot depend on this
/// crate: §2.3 makes the store and the git provider siblings. The re-export
/// keeps every S2 call site working.
pub use conductor_core::VerificationOutcome;

/// The task's declared scope.
///
/// §4.8's signature does not mention scope, but `OUT_OF_SCOPE` cannot be derived
/// from a baseline and an observation alone — the declaration lives on the task
/// (`task.scope_globs`, §5.1). It is therefore an explicit input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scope {
    globs: Vec<String>,
}

impl Scope {
    /// Build a scope from glob patterns.
    pub fn new(globs: impl IntoIterator<Item = String>) -> Self {
        Scope {
            globs: globs.into_iter().collect(),
        }
    }

    /// The patterns, as declared.
    pub fn globs(&self) -> &[String] {
        &self.globs
    }

    /// Whether a repository-relative path is inside the declared scope.
    ///
    /// **Fails closed:** an empty scope contains nothing, so a task that forgot
    /// to declare one halts for review rather than authorising everything.
    pub fn contains(&self, path: &str) -> bool {
        self.globs.iter().any(|g| glob_match(g, path))
    }
}

/// Which paths make a change require policy evaluation.
///
/// A seam, not a policy engine. S7 owns evaluation and will supply these from
/// the policy snapshot; the default below is a working starting set, not a
/// stand-in for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitivePatterns {
    /// Globs matching dependency manifests and lockfiles.
    pub manifests: Vec<String>,
    /// Globs matching migrations.
    pub migrations: Vec<String>,
}

impl Default for SensitivePatterns {
    fn default() -> Self {
        SensitivePatterns {
            manifests: [
                "**/Cargo.toml",
                "**/Cargo.lock",
                "**/package.json",
                "**/package-lock.json",
                "**/yarn.lock",
                "**/pnpm-lock.yaml",
                "**/pyproject.toml",
                "**/poetry.lock",
                "**/requirements*.txt",
                "**/go.mod",
                "**/go.sum",
                "**/Gemfile",
                "**/Gemfile.lock",
                "**/pom.xml",
                "**/build.gradle*",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            migrations: ["**/migrations/**", "**/migrate/**", "**/db/migrate/**"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        }
    }
}

impl SensitivePatterns {
    /// Whether a path requires policy evaluation before the run advances.
    pub fn matches(&self, path: &str) -> bool {
        self.manifests
            .iter()
            .chain(&self.migrations)
            .any(|g| glob_match(g, path))
    }
}

/// The outcome of one reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciliation {
    /// Exactly one of the seven §4.8 verdicts.
    pub verdict: Verdict,
    /// The verification result at the time of reconciliation, recorded as
    /// evidence. It never influences `verdict`.
    pub verification: Option<VerificationOutcome>,
    /// Every path the repository shows as changed, nested repositories excluded.
    pub changed_paths: Vec<String>,
    findings: Vec<Finding>,
}

impl Reconciliation {
    /// The findings raised. There is deliberately no way to remove one.
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }
}

/// Classify an attempt.
///
/// Pure: given the same inputs it returns the same verdict on any machine, at
/// any time, with no repository present.
pub fn reconcile(
    baseline: &Baseline,
    observed: &Observed,
    scope: &Scope,
    sensitive: &SensitivePatterns,
    report: Option<&AgentReport>,
    verification: Option<VerificationOutcome>,
) -> Reconciliation {
    let mut findings = Vec::new();

    findings.extend(structural_findings(baseline, observed));

    let nested_prefixes = nested_prefixes(baseline, observed);
    let changed: Vec<String> = changed_paths(baseline, observed)
        .into_iter()
        .filter(|p| !is_under_any(p, &nested_prefixes))
        .collect();

    for path in changed.iter().filter(|p| sensitive.matches(p)) {
        findings.push(Finding {
            kind: FindingKind::PolicySensitivePath,
            detail: format!("{path} is a dependency manifest, lockfile or migration"),
            path: Some(path.clone()),
        });
    }

    let out_of_scope: Vec<&String> = changed.iter().filter(|p| !scope.contains(p)).collect();
    for path in &out_of_scope {
        findings.push(Finding {
            kind: FindingKind::OutOfScopePath,
            detail: format!("{path} is outside the declared scope {:?}", scope.globs()),
            path: Some((*path).clone()),
        });
    }

    let contradiction = report_findings(report, &changed, &mut findings);

    let corrupt = !observed.health.is_healthy();
    if corrupt {
        findings.push(Finding {
            kind: FindingKind::RepositoryCorrupt,
            detail: describe_corruption(observed),
            path: None,
        });
    }

    let needs_policy = findings.iter().any(|f| f.kind.forces_policy_evaluation());

    let verdict = if corrupt {
        Verdict::Corrupt
    } else if contradiction {
        Verdict::Contradicted
    } else if needs_policy {
        Verdict::PolicySensitive
    } else if !out_of_scope.is_empty() {
        Verdict::OutOfScope
    } else if changed.is_empty() && observed.new_commits.is_empty() {
        Verdict::NoChange
    } else if report.is_some() {
        Verdict::CleanComplete
    } else {
        Verdict::CleanNoReport
    };

    Reconciliation {
        verdict,
        verification,
        changed_paths: changed,
        findings,
    }
}

fn describe_corruption(observed: &Observed) -> String {
    let h = &observed.health;
    let mut reasons = Vec::new();
    if !h.workspace_present {
        reasons.push("workspace is gone");
    }
    if !h.git_dir_readable {
        reasons.push("git directory unreadable");
    }
    if !h.head_resolvable {
        reasons.push("HEAD does not resolve");
    }
    if h.detached_head {
        reasons.push("HEAD is detached");
    }
    if h.merge_in_progress {
        reasons.push("merge in progress");
    }
    if h.rebase_in_progress {
        reasons.push("rebase in progress");
    }
    if h.cherry_pick_in_progress {
        reasons.push("cherry-pick in progress");
    }
    if h.revert_in_progress {
        reasons.push("revert in progress");
    }
    if h.bisect_in_progress {
        reasons.push("bisect in progress");
    }
    if h.index_lock_present {
        reasons.push("index.lock present");
    }
    if !h.object_store_ok {
        reasons.push("object store fails fsck");
    }
    let mut detail = reasons.join("; ");
    if !h.notes.is_empty() {
        detail.push_str(" — ");
        detail.push_str(&h.notes.join(" | "));
    }
    detail
}

/// Deltas in repository structure: config, remotes, refs, hooks, stash,
/// submodules, nested repositories.
///
/// All of these route to `POLICY_SENSITIVE`. None of them is something a task's
/// declared scope can authorise: a scope names paths in the working tree, and
/// none of these live there.
fn structural_findings(baseline: &Baseline, observed: &Observed) -> Vec<Finding> {
    let mut findings = Vec::new();
    let after = &observed.repo;

    diff_multimap(
        &baseline.config,
        &after.config,
        FindingKind::ConfigChanged,
        "config",
        &mut findings,
    );
    diff_multimap(
        &baseline.remotes,
        &after.remotes,
        FindingKind::RemoteChanged,
        "remote",
        &mut findings,
    );

    // The run branch advancing is the run doing its job, not a delta.
    let run_branch = baseline
        .head_branch
        .as_ref()
        .map(|b| format!("refs/heads/{b}"));
    for (name, before) in &baseline.refs {
        if Some(name) == run_branch.as_ref() || name == "refs/stash" {
            continue;
        }
        match after.refs.get(name) {
            None => findings.push(Finding {
                kind: FindingKind::RefRemoved,
                detail: format!("{name} was at {before} and is gone"),
                path: Some(name.clone()),
            }),
            Some(now) if now != before => findings.push(Finding {
                kind: FindingKind::RefChanged,
                detail: format!("{name} moved {before} -> {now}"),
                path: Some(name.clone()),
            }),
            Some(_) => {}
        }
    }
    for name in after.refs.keys() {
        if Some(name) == run_branch.as_ref() || name == "refs/stash" {
            continue;
        }
        if !baseline.refs.contains_key(name) {
            findings.push(Finding {
                kind: FindingKind::RefAdded,
                detail: format!("{name} did not exist at baseline"),
                path: Some(name.clone()),
            });
        }
    }

    for (name, before) in &baseline.hooks {
        match after.hooks.get(name) {
            None => findings.push(Finding {
                kind: FindingKind::HookRemoved,
                detail: format!("hook {name} was removed"),
                path: Some(name.clone()),
            }),
            Some(now) if now != before => findings.push(Finding {
                kind: FindingKind::HookChanged,
                detail: format!("hook {name} changed content or mode"),
                path: Some(name.clone()),
            }),
            Some(_) => {}
        }
    }
    for name in after.hooks.keys() {
        if !baseline.hooks.contains_key(name) {
            findings.push(Finding {
                kind: FindingKind::HookAdded,
                detail: format!("hook {name} was written during the attempt"),
                path: Some(name.clone()),
            });
        }
    }

    if baseline.stash_count != after.stash_count {
        findings.push(Finding {
            kind: FindingKind::StashChanged,
            detail: format!(
                "stash entries {} -> {}",
                baseline.stash_count, after.stash_count
            ),
            path: None,
        });
    }

    if baseline.submodules != after.submodules {
        findings.push(Finding {
            kind: FindingKind::SubmoduleChanged,
            detail: format!(
                "submodule status changed: {:?} -> {:?}",
                baseline.submodules, after.submodules
            ),
            path: None,
        });
    }

    let before_nested: BTreeMap<&str, _> = baseline
        .nested_repos
        .iter()
        .map(|n| (n.path.as_str(), n))
        .collect();
    let after_nested: BTreeMap<&str, _> = after
        .nested_repos
        .iter()
        .map(|n| (n.path.as_str(), n))
        .collect();
    for (path, before) in &before_nested {
        match after_nested.get(path) {
            None => findings.push(Finding {
                kind: FindingKind::NestedRepoChanged,
                detail: format!("nested repository {path} disappeared"),
                path: Some((*path).to_string()),
            }),
            Some(now) if now.head != before.head || now.dirty != before.dirty => {
                findings.push(Finding {
                    kind: FindingKind::NestedRepoModified,
                    detail: format!(
                        "nested repository {path} moved {:?} -> {:?} (dirty {} -> {})",
                        before.head, now.head, before.dirty, now.dirty
                    ),
                    path: Some((*path).to_string()),
                })
            }
            Some(_) => {}
        }
    }
    for path in after_nested.keys() {
        if !before_nested.contains_key(path) {
            findings.push(Finding {
                kind: FindingKind::NestedRepoChanged,
                detail: format!("nested repository {path} appeared during the attempt"),
                path: Some((*path).to_string()),
            });
        }
    }

    findings
}

fn diff_multimap(
    before: &BTreeMap<String, Vec<String>>,
    after: &BTreeMap<String, Vec<String>>,
    kind: FindingKind,
    noun: &str,
    findings: &mut Vec<Finding>,
) {
    for (key, old) in before {
        match after.get(key) {
            None => findings.push(Finding {
                kind,
                detail: format!("{noun} {key} was removed (was {old:?})"),
                path: Some(key.clone()),
            }),
            Some(new) if new != old => findings.push(Finding {
                kind,
                detail: format!("{noun} {key} changed {old:?} -> {new:?}"),
                path: Some(key.clone()),
            }),
            Some(_) => {}
        }
    }
    for (key, new) in after {
        if !before.contains_key(key) {
            findings.push(Finding {
                kind,
                detail: format!("{noun} {key} was added ({new:?})"),
                path: Some(key.clone()),
            });
        }
    }
}

/// Every working-tree path that differs from baseline, plus everything the new
/// commits changed.
fn changed_paths(baseline: &Baseline, observed: &Observed) -> Vec<String> {
    let mut changed: BTreeSet<String> = BTreeSet::new();

    let before: BTreeMap<&str, &str> = baseline
        .status
        .iter()
        .map(|e| (e.path.as_str(), e.xy.as_str()))
        .collect();
    let after: BTreeMap<&str, &str> = observed
        .repo
        .status
        .iter()
        .map(|e| (e.path.as_str(), e.xy.as_str()))
        .collect();

    for (path, xy) in &after {
        if before.get(path) != Some(xy) {
            changed.insert((*path).to_string());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.insert((*path).to_string());
        }
    }

    let before_untracked: BTreeSet<&str> = baseline.untracked.iter().map(String::as_str).collect();
    let after_untracked: BTreeSet<&str> =
        observed.repo.untracked.iter().map(String::as_str).collect();
    for path in after_untracked.symmetric_difference(&before_untracked) {
        changed.insert((*path).to_string());
    }

    for change in &observed.committed_changes {
        changed.insert(change.path.clone());
        if let Some(orig) = &change.orig_path {
            changed.insert(orig.clone());
        }
    }

    changed.into_iter().collect()
}

fn nested_prefixes(baseline: &Baseline, observed: &Observed) -> Vec<String> {
    let mut prefixes: BTreeSet<String> = BTreeSet::new();
    for nested in baseline
        .nested_repos
        .iter()
        .chain(&observed.repo.nested_repos)
    {
        prefixes.insert(nested.path.trim_end_matches('/').to_string());
    }
    prefixes.into_iter().collect()
}

fn is_under_any(path: &str, prefixes: &[String]) -> bool {
    let path = path.trim_end_matches('/');
    prefixes
        .iter()
        .any(|p| path == p || path.starts_with(&format!("{p}/")))
}

/// Returns whether the report contradicts the repository.
fn report_findings(
    report: Option<&AgentReport>,
    changed: &[String],
    findings: &mut Vec<Finding>,
) -> bool {
    let Some(report) = report else {
        return false;
    };
    let changed_set: BTreeSet<&str> = changed.iter().map(String::as_str).collect();
    let mut contradicted = false;

    if report.claim == ReportClaim::Complete && changed.is_empty() {
        findings.push(Finding {
            kind: FindingKind::ReportContradicted,
            detail: "the report claims the task is complete, and the repository is unchanged"
                .to_string(),
            path: None,
        });
        contradicted = true;
    }

    for path in &report.files_touched {
        if !changed_set.contains(path.as_str()) {
            findings.push(Finding {
                kind: FindingKind::ReportContradicted,
                detail: format!("the report claims {path} was modified; the repository disagrees"),
                path: Some(path.clone()),
            });
            contradicted = true;
        }
    }

    let claimed: BTreeSet<&str> = report.files_touched.iter().map(String::as_str).collect();
    for path in changed {
        if !claimed.contains(path.as_str()) {
            findings.push(Finding {
                kind: FindingKind::ReportOmittedChange,
                detail: format!("{path} changed and the report does not mention it"),
                path: Some(path.clone()),
            });
        }
    }

    contradicted
}

/// Glob matching for scope and sensitivity patterns.
///
/// The supported subset is deliberately small and stated rather than inherited
/// from a dependency: `**` spans any number of path segments, `*` and `?` match
/// within one segment, everything else is literal. Nothing here needs character
/// classes or brace expansion, and a matcher whose semantics live in this file
/// is a matcher whose over-matching can be reviewed — over-matching being the
/// dangerous direction, since it would silently widen a task's scope.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.trim_end_matches('/').split('/').collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|i| match_segments(rest, &path[i..])),
        Some((segment, rest)) => match path.split_first() {
            Some((candidate, path_rest)) if match_segment(segment, candidate) => {
                match_segments(rest, path_rest)
            }
            _ => false,
        },
    }
}

fn match_segment(pattern: &str, candidate: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let candidate: Vec<char> = candidate.chars().collect();
    match_chars(&pattern, &candidate)
}

fn match_chars(pattern: &[char], candidate: &[char]) -> bool {
    match pattern.split_first() {
        None => candidate.is_empty(),
        Some(('*', rest)) => (0..=candidate.len()).any(|i| match_chars(rest, &candidate[i..])),
        Some(('?', rest)) => !candidate.is_empty() && match_chars(rest, &candidate[1..]),
        Some((c, rest)) => match candidate.split_first() {
            Some((d, candidate_rest)) if c == d => match_chars(rest, candidate_rest),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_star_spans_segments_and_single_star_does_not() {
        assert!(glob_match("crates/**", "crates/conductor-git/src/lib.rs"));
        assert!(glob_match("crates/**", "crates"));
        assert!(!glob_match("crates/*", "crates/a/b.rs"));
        assert!(glob_match("crates/*", "crates/a.rs"));
        assert!(!glob_match("crates/**", "docs/notes.md"));
    }

    #[test]
    fn a_leading_double_star_matches_at_any_depth_including_the_root() {
        assert!(glob_match("**/Cargo.toml", "Cargo.toml"));
        assert!(glob_match("**/Cargo.toml", "crates/a/Cargo.toml"));
        assert!(!glob_match("**/Cargo.toml", "crates/a/Cargo.toml.bak"));
    }

    #[test]
    fn wildcards_do_not_leak_across_a_separator() {
        assert!(!glob_match("src/*.rs", "src/nested/file.rs"));
        assert!(glob_match("src/*.rs", "src/file.rs"));
        assert!(glob_match("src/?.rs", "src/a.rs"));
        assert!(!glob_match("src/?.rs", "src/ab.rs"));
    }

    #[test]
    fn an_empty_scope_contains_nothing() {
        // Fails closed. A task that declared no scope must halt, not authorise
        // the whole repository.
        let scope = Scope::default();
        assert!(!scope.contains("src/lib.rs"));
        assert!(!scope.contains(""));
    }

    #[test]
    fn migrations_are_sensitive_at_any_depth() {
        let sensitive = SensitivePatterns::default();
        assert!(sensitive.matches("db/migrations/0001_init.sql"));
        assert!(sensitive.matches("migrations/0001_init.sql"));
        assert!(sensitive.matches("Cargo.lock"));
        assert!(sensitive.matches("crates/x/Cargo.toml"));
        assert!(!sensitive.matches("src/lib.rs"));
    }

    #[test]
    fn every_verdict_has_a_distinct_persisted_string() {
        let all = [
            Verdict::NoChange,
            Verdict::CleanComplete,
            Verdict::CleanNoReport,
            Verdict::OutOfScope,
            Verdict::PolicySensitive,
            Verdict::Contradicted,
            Verdict::Corrupt,
        ];
        let names: BTreeSet<&str> = all.iter().map(|v| v.as_str()).collect();
        assert_eq!(names.len(), 7, "§4.8 defines exactly seven verdicts");
    }
}
