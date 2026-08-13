//! Capturing what a repository looks like — the I/O half of reconciliation.
//!
//! Master plan §4.1 fixes the baseline surface and §4.8 fixes the reconciled
//! surface. The split enforced here is the one §2.1 depends on: **observation
//! runs git, classification runs no I/O at all**. Everything in this module
//! produces data; nothing in it decides anything.
//!
//! Observation is deliberately tolerant. A workspace that has been deleted,
//! corrupted or left locked is not an error — it is the input that makes
//! `reconcile()` return `CORRUPT`. The only `Err` that escapes is a host with no
//! usable `git`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::GitResult;
use crate::git::{GitOutput, nul_records, run_git, run_git_ok, run_git_stdin};

/// How deep the nested-repository walk goes. A bound, not a limit anybody should
/// hit: a repository nesting repositories twelve levels down is already outside
/// what v1 claims to reason about.
const NESTED_WALK_MAX_DEPTH: usize = 12;

/// One `git status --porcelain=v2` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    /// The two-letter staged/unstaged code, or `?`/`!` for untracked/ignored.
    pub xy: String,
    /// Path, repository-relative.
    pub path: String,
    /// Original path for a rename or copy.
    pub orig_path: Option<String>,
}

/// One entry in a name-status diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// `A`, `M`, `D`, `R100`, …
    pub status: String,
    /// Path, repository-relative.
    pub path: String,
    /// Original path for a rename or copy.
    pub orig_path: Option<String>,
}

/// A file in the hooks directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEntry {
    /// Size in bytes.
    pub size: u64,
    /// Whether any execute bit is set.
    pub executable: bool,
    /// Git's own object id for the content. Computed with `git hash-object` so
    /// the identity of a file is decided by the same binary that decides
    /// everything else here (§2.2), not by a hash function of our choosing.
    pub content_id: String,
}

/// A repository nested inside the working tree without being a submodule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NestedRepo {
    /// Path relative to the outer repository root.
    pub path: String,
    /// The nested repository's own HEAD, when it resolves.
    pub head: Option<String>,
    /// Whether the nested repository has uncommitted changes.
    pub dirty: bool,
}

/// A commit that did not exist at baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRecord {
    /// Commit id.
    pub oid: String,
    /// Parent ids, in order.
    pub parents: Vec<String>,
    /// Subject line.
    pub subject: String,
}

/// The §4.1 baseline: everything captured about a repository at one instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    /// `HEAD`'s commit.
    pub base_commit: String,
    /// `HEAD`'s tree.
    pub tree_hash: String,
    /// The checked-out branch, or `None` when HEAD is detached.
    pub head_branch: Option<String>,
    /// `git status --porcelain=v2`, tracked entries only.
    pub status: Vec<StatusEntry>,
    /// Untracked paths.
    pub untracked: Vec<String>,
    /// `git config --list --local`. Multi-valued keys keep every value, in
    /// order; keys are lowercased by git itself.
    pub config: BTreeMap<String, Vec<String>>,
    /// `git remote -v`, as name → urls.
    pub remotes: BTreeMap<String, Vec<String>>,
    /// Every ref, as refname → object id.
    pub refs: BTreeMap<String, String>,
    /// `git submodule status` lines.
    pub submodules: Vec<String>,
    /// `git stash list` entry count.
    pub stash_count: usize,
    /// The hooks directory listing, as filename → entry.
    pub hooks: BTreeMap<String, HookEntry>,
    /// Repositories nested in the working tree that are not submodules.
    ///
    /// Not named in §4.1's baseline list, but §4.1 requires nested repositories
    /// to be "detected at baseline" — which is only possible if the baseline
    /// records them.
    pub nested_repos: Vec<NestedRepo>,
}

/// Whether the repository is in a state that can be reasoned about at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoHealth {
    /// The workspace directory still exists.
    pub workspace_present: bool,
    /// `git rev-parse --git-dir` succeeded.
    pub git_dir_readable: bool,
    /// `HEAD` resolves to a commit.
    pub head_resolvable: bool,
    /// `HEAD` is detached.
    pub detached_head: bool,
    /// A merge is in progress.
    pub merge_in_progress: bool,
    /// A rebase is in progress.
    pub rebase_in_progress: bool,
    /// A cherry-pick is in progress.
    pub cherry_pick_in_progress: bool,
    /// A revert is in progress.
    pub revert_in_progress: bool,
    /// A bisect is in progress.
    pub bisect_in_progress: bool,
    /// `.git/index.lock` is present — another git process is running, or died.
    pub index_lock_present: bool,
    /// `git fsck` reported no errors.
    pub object_store_ok: bool,
    /// What went wrong, in git's own words. Evidence, not decisions.
    pub notes: Vec<String>,
}

impl RepoHealth {
    /// Whether every dimension is in the state a runnable workspace is in.
    pub fn is_healthy(&self) -> bool {
        self.workspace_present
            && self.git_dir_readable
            && self.head_resolvable
            && !self.detached_head
            && !self.merge_in_progress
            && !self.rebase_in_progress
            && !self.cherry_pick_in_progress
            && !self.revert_in_progress
            && !self.bisect_in_progress
            && !self.index_lock_present
            && self.object_store_ok
    }
}

/// The §4.8 reconciled surface, observed after an attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observed {
    /// The same surface the baseline captured, captured again.
    pub repo: Baseline,
    /// Whether the repository can be reasoned about.
    pub health: RepoHealth,
    /// Commits reachable from HEAD that were not reachable at baseline.
    pub new_commits: Vec<CommitRecord>,
    /// Files changed by those commits.
    pub committed_changes: Vec<FileChange>,
    /// `git diff --cached --name-status`.
    pub staged: Vec<FileChange>,
    /// `git diff --name-status`.
    pub unstaged: Vec<FileChange>,
    /// HEAD's reflog, newest first.
    pub reflog: Vec<String>,
}

/// Capture the §4.1 baseline of a repository.
pub fn capture_baseline(repo: &Path) -> GitResult<Baseline> {
    let base_commit = run_git_ok(repo, &["rev-parse", "HEAD"])?.stdout_trimmed();
    let tree_hash = run_git_ok(repo, &["rev-parse", "HEAD^{tree}"])?.stdout_trimmed();
    let head_branch = symbolic_head(repo)?;
    let (status, untracked) = status_entries(repo)?;

    Ok(Baseline {
        base_commit,
        tree_hash,
        head_branch,
        status,
        untracked,
        config: local_config(repo)?,
        remotes: remotes(repo)?,
        refs: all_refs(repo)?,
        submodules: submodule_status(repo)?,
        stash_count: stash_count(repo)?,
        hooks: hooks(repo)?,
        nested_repos: nested_repos(repo)?,
    })
}

/// Observe a repository after an attempt.
///
/// Never fails because the repository is broken — that is the point. A deleted,
/// corrupted or locked workspace comes back as `health`, and `reconcile()` turns
/// it into `CORRUPT`.
pub fn observe(repo: &Path, baseline: &Baseline) -> GitResult<Observed> {
    let health = health(repo)?;

    if !health.workspace_present || !health.git_dir_readable || !health.head_resolvable {
        // Nothing else can be believed. Report the baseline's own shape with
        // empty content so the classifier sees "unreadable", not "unchanged".
        return Ok(Observed {
            repo: empty_like(baseline),
            health,
            new_commits: Vec::new(),
            committed_changes: Vec::new(),
            staged: Vec::new(),
            unstaged: Vec::new(),
            reflog: Vec::new(),
        });
    }

    let repo_state = capture_baseline(repo)?;
    let range = format!("{}..HEAD", baseline.base_commit);

    Ok(Observed {
        new_commits: new_commits(repo, &range)?,
        committed_changes: name_status(repo, &["diff", "--name-status", "-z", &range])?,
        staged: name_status(repo, &["diff", "--cached", "--name-status", "-z"])?,
        unstaged: name_status(repo, &["diff", "--name-status", "-z"])?,
        reflog: reflog(repo)?,
        repo: repo_state,
        health,
    })
}

fn empty_like(baseline: &Baseline) -> Baseline {
    Baseline {
        base_commit: String::new(),
        tree_hash: String::new(),
        head_branch: None,
        status: Vec::new(),
        untracked: Vec::new(),
        config: BTreeMap::new(),
        remotes: BTreeMap::new(),
        refs: BTreeMap::new(),
        submodules: Vec::new(),
        stash_count: 0,
        hooks: BTreeMap::new(),
        nested_repos: baseline.nested_repos.clone(),
    }
}

fn symbolic_head(repo: &Path) -> GitResult<Option<String>> {
    let out = run_git(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    Ok(if out.ok() {
        Some(out.stdout_trimmed())
    } else {
        None
    })
}

fn status_entries(repo: &Path) -> GitResult<(Vec<StatusEntry>, Vec<String>)> {
    let out = run_git_ok(
        repo,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    let records = nul_records(&out.stdout);

    let mut status = Vec::new();
    let mut untracked = Vec::new();
    let mut i = 0;
    while i < records.len() {
        let record = &records[i];
        i += 1;
        let Some((tag, rest)) = record.split_once(' ') else {
            continue;
        };
        match tag {
            "1" | "u" => {
                if let Some(entry) = ordinary_entry(rest, None) {
                    status.push(entry);
                }
            }
            "2" => {
                // A rename/copy record is followed by its original path in its
                // own NUL-terminated field.
                let orig = records.get(i).cloned();
                i += 1;
                if let Some(entry) = ordinary_entry(rest, orig) {
                    status.push(entry);
                }
            }
            "?" => untracked.push(rest.to_string()),
            _ => {}
        }
    }
    untracked.sort();
    status.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((status, untracked))
}

/// `<XY> <sub> …fields… <path>` — the path is everything after the last field,
/// and the field count differs between `1`/`2` and `u` records, so the path is
/// found by counting from the front rather than by splitting on the last space
/// (paths contain spaces).
fn ordinary_entry(rest: &str, orig: Option<String>) -> Option<StatusEntry> {
    let mut parts = rest.splitn(2, ' ');
    let xy = parts.next()?.to_string();
    let tail = parts.next()?;
    // Remaining fixed fields before the path: sub, mH, mI, mW, hH, hI (6) for
    // `1`; those plus the rename score (7) for `2`; sub, m1..m3, mW, h1..h3 (8)
    // for `u`. Rather than branch on the record type, take the path as the last
    // whitespace-delimited run *after* skipping fields that cannot contain a
    // space: every fixed field is a mode, a hash or a score.
    let mut path_start = 0;
    for field in tail.split(' ') {
        if is_fixed_status_field(field) {
            path_start += field.len() + 1;
        } else {
            break;
        }
    }
    let path = tail.get(path_start..)?.to_string();
    if path.is_empty() {
        return None;
    }
    Some(StatusEntry {
        xy,
        path,
        orig_path: orig,
    })
}

fn is_fixed_status_field(field: &str) -> bool {
    !field.is_empty()
        && (field.chars().all(|c| c.is_ascii_hexdigit())
            || (field.starts_with(['R', 'C']) && field[1..].chars().all(|c| c.is_ascii_digit()))
            || field == "N...")
}

fn local_config(repo: &Path) -> GitResult<BTreeMap<String, Vec<String>>> {
    let out = run_git(repo, &["config", "--local", "--list", "-z"])?;
    let mut config: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if !out.ok() {
        return Ok(config);
    }
    for record in nul_records(&out.stdout) {
        let (key, value) = match record.split_once('\n') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (record, String::new()),
        };
        config.entry(key).or_default().push(value);
    }
    Ok(config)
}

fn remotes(repo: &Path) -> GitResult<BTreeMap<String, Vec<String>>> {
    let out = run_git_ok(repo, &["remote", "-v"])?;
    let mut remotes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in out.stdout_lossy().lines() {
        let mut parts = line.split('\t');
        let (Some(name), Some(rest)) = (parts.next(), parts.next()) else {
            continue;
        };
        let url = rest.rsplit_once(' ').map(|(u, _)| u).unwrap_or(rest);
        let entry = remotes.entry(name.to_string()).or_default();
        if !entry.iter().any(|u| u == url) {
            entry.push(url.to_string());
        }
    }
    Ok(remotes)
}

fn all_refs(repo: &Path) -> GitResult<BTreeMap<String, String>> {
    let out = run_git_ok(repo, &["for-each-ref", "--format=%(objectname) %(refname)"])?;
    let mut refs = BTreeMap::new();
    for line in out.stdout_lossy().lines() {
        if let Some((oid, name)) = line.split_once(' ') {
            refs.insert(name.to_string(), oid.to_string());
        }
    }
    Ok(refs)
}

fn submodule_status(repo: &Path) -> GitResult<Vec<String>> {
    let out = run_git(repo, &["submodule", "status"])?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout_lossy()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn stash_count(repo: &Path) -> GitResult<usize> {
    let out = run_git(repo, &["stash", "list"])?;
    if !out.ok() {
        return Ok(0);
    }
    Ok(out.stdout_lossy().lines().filter(|l| !l.is_empty()).count())
}

fn hooks(repo: &Path) -> GitResult<BTreeMap<String, HookEntry>> {
    use std::os::unix::fs::PermissionsExt;

    // The *directory*, not `core.hooksPath`. Pointing hooksPath at /dev/null
    // stops hooks running; it does not stop an agent writing one, and a hook
    // file that appears during a run is exactly what §4.8 wants to see. A change
    // to `core.hooksPath` itself is caught by the config diff.
    let git_dir = run_git_ok(repo, &["rev-parse", "--absolute-git-dir"])?.stdout_trimmed();
    let hooks_dir = Path::new(&git_dir).join("hooks");

    let mut names = Vec::new();
    let mut metas = Vec::new();
    let Ok(entries) = std::fs::read_dir(&hooks_dir) else {
        return Ok(BTreeMap::new());
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        names.push(entry.file_name().to_string_lossy().to_string());
        metas.push((meta.len(), meta.permissions().mode() & 0o111 != 0));
    }

    if names.is_empty() {
        return Ok(BTreeMap::new());
    }

    // One invocation for every file: `git hash-object --stdin-paths` reads paths
    // from stdin and writes one id per line, in order.
    let mut stdin = String::new();
    for name in &names {
        stdin.push_str(&hooks_dir.join(name).to_string_lossy());
        stdin.push('\n');
    }
    let ids = run_git_stdin(repo, &["hash-object", "--stdin-paths"], stdin.as_bytes())?;
    let ids: Vec<String> = ids.stdout_lossy().lines().map(str::to_string).collect();

    let mut hooks = BTreeMap::new();
    for (i, name) in names.into_iter().enumerate() {
        let (size, executable) = metas[i];
        hooks.insert(
            name,
            HookEntry {
                size,
                executable,
                content_id: ids.get(i).cloned().unwrap_or_default(),
            },
        );
    }
    Ok(hooks)
}

fn nested_repos(repo: &Path) -> GitResult<Vec<NestedRepo>> {
    let submodule_paths: Vec<String> = submodule_status(repo)?
        .iter()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect();

    let mut found = Vec::new();
    walk_for_nested(repo, repo, 0, &submodule_paths, &mut found);
    found.sort_by(|a: &NestedRepo, b: &NestedRepo| a.path.cmp(&b.path));

    let mut described = Vec::new();
    for mut nested in found {
        let path = repo.join(&nested.path);
        let head = run_git(&path, &["rev-parse", "HEAD"])?;
        nested.head = head.ok().then(|| head.stdout_trimmed());
        let status = run_git(&path, &["status", "--porcelain=v2"])?;
        nested.dirty = status.ok() && !status.stdout_lossy().trim().is_empty();
        described.push(nested);
    }
    Ok(described)
}

fn walk_for_nested(
    root: &Path,
    dir: &Path,
    depth: usize,
    submodules: &[String],
    out: &mut Vec<NestedRepo>,
) {
    if depth > NESTED_WALK_MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if submodules.contains(&rel) {
            continue;
        }
        if path.join(".git").exists() {
            out.push(NestedRepo {
                path: rel,
                head: None,
                dirty: false,
            });
            // Do not descend: what is inside a nested repository is that
            // repository's business, and v1 does not claim to reason about it.
            continue;
        }
        walk_for_nested(root, &path, depth + 1, submodules, out);
    }
}

fn health(repo: &Path) -> GitResult<RepoHealth> {
    let mut notes = Vec::new();

    if !repo.exists() {
        notes.push(format!("workspace {} does not exist", repo.display()));
        return Ok(RepoHealth {
            workspace_present: false,
            git_dir_readable: false,
            head_resolvable: false,
            detached_head: false,
            merge_in_progress: false,
            rebase_in_progress: false,
            cherry_pick_in_progress: false,
            revert_in_progress: false,
            bisect_in_progress: false,
            index_lock_present: false,
            object_store_ok: false,
            notes,
        });
    }

    let git_dir_out = run_git(repo, &["rev-parse", "--absolute-git-dir"])?;
    let git_dir_readable = git_dir_out.ok();
    if !git_dir_readable {
        notes.push(git_dir_out.stderr.clone());
    }
    let git_dir = git_dir_readable.then(|| git_dir_out.stdout_trimmed());

    let head_out = run_git(repo, &["rev-parse", "--verify", "HEAD"])?;
    let head_resolvable = head_out.ok();
    if !head_resolvable {
        notes.push(head_out.stderr.clone());
    }

    let detached_head = git_dir_readable
        && head_resolvable
        && !run_git(repo, &["symbolic-ref", "--quiet", "HEAD"])?.ok();

    let exists = |name: &str| -> bool {
        git_dir
            .as_ref()
            .map(|d| Path::new(d).join(name).exists())
            .unwrap_or(false)
    };

    let object_store_ok = if git_dir_readable {
        let fsck = run_git(repo, &["fsck", "--no-progress", "--no-dangling"])?;
        if !fsck.ok() {
            notes.push(fsck.stderr.clone());
        }
        fsck.ok()
    } else {
        false
    };

    Ok(RepoHealth {
        workspace_present: true,
        git_dir_readable,
        head_resolvable,
        detached_head,
        merge_in_progress: exists("MERGE_HEAD"),
        rebase_in_progress: exists("rebase-merge") || exists("rebase-apply"),
        cherry_pick_in_progress: exists("CHERRY_PICK_HEAD"),
        revert_in_progress: exists("REVERT_HEAD"),
        bisect_in_progress: exists("BISECT_LOG"),
        index_lock_present: exists("index.lock"),
        object_store_ok,
        notes,
    })
}

fn new_commits(repo: &Path, range: &str) -> GitResult<Vec<CommitRecord>> {
    let out = run_git(repo, &["log", "--format=%H%x1f%P%x1f%s", range])?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout_lossy()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\u{1f}');
            let oid = fields.next()?.to_string();
            let parents = fields
                .next()?
                .split_whitespace()
                .map(str::to_string)
                .collect();
            let subject = fields.next().unwrap_or_default().to_string();
            Some(CommitRecord {
                oid,
                parents,
                subject,
            })
        })
        .collect())
}

fn name_status(repo: &Path, args: &[&str]) -> GitResult<Vec<FileChange>> {
    let out = run_git(repo, args)?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(parse_name_status(&out))
}

fn parse_name_status(out: &GitOutput) -> Vec<FileChange> {
    let records = nul_records(&out.stdout);
    let mut changes = Vec::new();
    let mut i = 0;
    while i < records.len() {
        let status = records[i].clone();
        i += 1;
        let Some(first) = records.get(i).cloned() else {
            break;
        };
        i += 1;
        if status.starts_with(['R', 'C']) {
            let Some(second) = records.get(i).cloned() else {
                break;
            };
            i += 1;
            changes.push(FileChange {
                status,
                path: second,
                orig_path: Some(first),
            });
        } else {
            changes.push(FileChange {
                status,
                path: first,
                orig_path: None,
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

fn reflog(repo: &Path) -> GitResult<Vec<String>> {
    let out = run_git(repo, &["reflog", "--format=%H %gd %gs"])?;
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(out.stdout_lossy().lines().map(str::to_string).collect())
}
