//! Fixture repositories, built programmatically under `tempfile` directories.
//!
//! Nothing here is committed as a binary fixture: a checked-in `.git` directory
//! is opaque to review, and this suite's whole point is that repository state is
//! inspectable evidence.
//!
//! Fixture `git` calls are **hermetic** (`GIT_CONFIG_GLOBAL=/dev/null`, fixed
//! identity and dates) so a fixture does not depend on the developer's global
//! config. Production code under test is deliberately *not* hermetic (§2.2): it
//! must observe the repository the operator observes.
#![allow(dead_code)] // one copy is compiled into each test binary; each uses a subset

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// A disposable repository plus the temp dir that owns it.
pub struct Fixture {
    /// Keeps the directory alive.
    pub dir: TempDir,
    /// The repository root inside it.
    pub path: PathBuf,
}

impl Fixture {
    /// The repository root.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Run git hermetically, requiring success.
pub fn git(cwd: &Path, args: &[&str]) -> Output {
    let out = git_try(cwd, args);
    assert!(
        out.status.success(),
        "git {args:?} in {cwd:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Run git hermetically, allowing failure.
pub fn git_try(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "fixture@localhost")
        .env("GIT_COMMITTER_NAME", "Fixture Author")
        .env("GIT_COMMITTER_EMAIL", "fixture@localhost")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00 +0000")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00 +0000")
        .output()
        .expect("spawn git")
}

/// Trimmed stdout of a successful git command.
pub fn git_out(cwd: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git(cwd, args).stdout)
        .trim_end()
        .to_string()
}

/// Write a file, creating parents.
pub fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(&path, contents).unwrap_or_else(|e| panic!("write {path:?}: {e}"));
}

/// Stage everything and commit.
pub fn commit_all(root: &Path, message: &str) -> String {
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
    head(root)
}

/// `HEAD`'s commit id.
pub fn head(root: &Path) -> String {
    git_out(root, &["rev-parse", "HEAD"])
}

/// An empty initialised repository on `main`.
pub fn init_repo() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    // macOS puts temp dirs behind /var -> /private/var. Canonicalise once so
    // path comparisons in tests are not comparing two spellings of one path.
    let path = dir.path().canonicalize().expect("canonicalize tempdir");
    git(&path, &["init", "-q", "-b", "main"]);
    Fixture { dir, path }
}

/// The baseline fixture: several commits, a pack **and** loose objects.
///
/// Both storage forms matter. M1/M2 hardlink and corrupt loose objects; a
/// repository that had been fully packed would make the negative control in
/// `isolation.rs` silently vacuous.
pub fn clean_repo() -> Fixture {
    let f = init_repo();
    write(&f.path, "README.md", "# fixture\n");
    write(&f.path, "src/lib.rs", "pub fn one() -> u32 { 1 }\n");
    commit_all(&f.path, "first");

    write(&f.path, "src/two.rs", "pub fn two() -> u32 { 2 }\n");
    commit_all(&f.path, "second");

    // Pack what exists so far…
    git(&f.path, &["gc", "-q"]);

    // …then add commits that stay loose.
    write(&f.path, "src/three.rs", "pub fn three() -> u32 { 3 }\n");
    write(&f.path, "docs/notes.md", "notes\n");
    commit_all(&f.path, "third");

    assert!(
        !loose_object_files(&f.path).is_empty(),
        "clean_repo must leave loose objects behind"
    );
    assert!(
        !pack_files(&f.path).is_empty(),
        "clean_repo must leave a pack behind"
    );
    f
}

/// A repository with uncommitted modifications and an untracked file.
pub fn dirty_repo() -> Fixture {
    let f = clean_repo();
    write(
        &f.path,
        "README.md",
        "# fixture\nlocally edited, never committed\n",
    );
    write(&f.path, "scratch.txt", "untracked user work\n");
    git(&f.path, &["add", "src/three.rs"]);
    write(&f.path, "src/three.rs", "pub fn three() -> u32 { 33 }\n");
    f
}

/// A repository whose HEAD is detached.
pub fn detached_repo() -> Fixture {
    let f = clean_repo();
    let first = git_out(&f.path, &["rev-list", "--max-parents=0", "HEAD"]);
    git(&f.path, &["checkout", "-q", "--detach", &first]);
    f
}

/// A repository with a real submodule.
///
/// `protocol.file.allow=always` is required because git ≥2.38 refuses the file
/// transport for submodules by default.
pub fn submodule_repo() -> Fixture {
    let inner = clean_repo();
    let outer = clean_repo();
    let inner_path = inner.path.to_string_lossy().to_string();
    git(
        &outer.path,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &inner_path,
            "vendor/inner",
        ],
    );
    commit_all(&outer.path, "add submodule");
    // `inner` is dropped here on purpose: the submodule is registered in
    // .gitmodules and the gitlink is committed, which is all `git submodule
    // status` needs to report a non-empty result.
    drop(inner);
    outer
}

/// A repository containing an unregistered nested repository — a `.git`
/// directory git itself treats as opaque, with no `.gitmodules` entry.
pub fn nested_repo() -> Fixture {
    let outer = clean_repo();
    let nested = outer.path.join("vendor/nested");
    fs::create_dir_all(&nested).expect("create nested dir");
    git(&nested, &["init", "-q", "-b", "main"]);
    write(&nested, "inner.txt", "nested content\n");
    commit_all(&nested, "nested first");
    outer
}

/// A larger repository: many files, so clone timing and object counts are not
/// trivially small. Deliberately modest — this suite runs on every `cargo test`.
pub fn large_repo() -> Fixture {
    let f = init_repo();
    for i in 0..200 {
        let body: String = (0..64)
            .map(|j| format!("line {i}-{j} of filler text\n"))
            .collect();
        write(&f.path, &format!("gen/file_{i:03}.txt"), &body);
    }
    commit_all(&f.path, "bulk");
    for i in 0..20 {
        write(&f.path, &format!("gen/file_{i:03}.txt"), "changed\n");
    }
    commit_all(&f.path, "bulk edit");
    f
}

/// Every loose object file in a repository, as absolute paths.
pub fn loose_object_files(repo: &Path) -> Vec<PathBuf> {
    let objects = repo.join(".git/objects");
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(&objects) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() != 2 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        if let Ok(inner) = fs::read_dir(entry.path()) {
            for object in inner.flatten() {
                found.push(object.path());
            }
        }
    }
    found.sort();
    found
}

/// Every pack-related file in a repository.
pub fn pack_files(repo: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(repo.join(".git/objects/pack")) else {
        return found;
    };
    for entry in entries.flatten() {
        found.push(entry.path());
    }
    found.sort();
    found
}

/// Every regular file under `.git/objects`, keyed by path relative to
/// `.git/objects`, with full contents.
///
/// Full bytes, not a digest: this is what "byte-identical" means, and comparing
/// the bytes is strictly stronger than comparing a hash of them.
pub fn object_store_contents(repo: &Path) -> BTreeMap<String, Vec<u8>> {
    let root = repo.join(".git/objects");
    let mut map = BTreeMap::new();
    collect_files(&root, &root, &mut map);
    map
}

fn collect_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect_files(root, &path, out),
            Ok(t) if t.is_file() => {
                let rel = path
                    .strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .to_string();
                if let Ok(bytes) = fs::read(&path) {
                    out.insert(rel, bytes);
                }
            }
            _ => {}
        }
    }
}
