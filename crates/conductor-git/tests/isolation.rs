//! **The acceptance test that matters** — master plan §4.1, S2's stop point,
//! ADR-0001, acceptance rows 14 and 15.
//!
//! An adversarial routine runs *inside the clone* and the operator's repository
//! must come out byte-identical. Everything downstream of S2 assumes this.
//!
//! ## Non-vacuity
//!
//! An isolation test is worthless unless it can fail. §4.1 states the reason
//! this one is known not to be: a default (hardlinked) clone **fails it** (M2).
//! That claim is not left as prose here — [`negative_control_the_same_assertion_fails_against_a_default_clone`]
//! runs the identical hostile routine against a plain `git clone` and asserts
//! that the identical assertion **panics**. If the negative control ever stops
//! damaging the source, that test fails and says so, which is the only way to
//! learn that the positive test has quietly gone vacuous.

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use conductor_core::{PolicyHash, RunId, TaskId};
use conductor_git::clone::{WorkspaceRequest, create_workspace};

use common::{clean_repo, git_out, git_try, loose_object_files, object_store_contents, pack_files};

/// Everything §4.1 names, plus the raw object bytes.
///
/// §4.1 says "asserted by hashing `.git/config`, `git show-ref` output, and
/// `git cat-file --batch-all-objects --batch-check`". Comparing the bytes
/// themselves is strictly stronger than comparing a digest of them, and it is
/// what "byte-identical" actually means, so that is what this does.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceFingerprint {
    config: Vec<u8>,
    show_ref: String,
    batch_check: String,
    batch_check_status: Option<i32>,
    fsck_status: Option<i32>,
    objects: BTreeMap<String, Vec<u8>>,
}

fn fingerprint(repo: &Path) -> SourceFingerprint {
    let show_ref = git_try(repo, &["show-ref"]);
    let batch = git_try(repo, &["cat-file", "--batch-all-objects", "--batch-check"]);
    let fsck = git_try(repo, &["fsck", "--no-progress", "--no-dangling"]);

    SourceFingerprint {
        config: std::fs::read(repo.join(".git/config")).expect("read .git/config"),
        show_ref: String::from_utf8_lossy(&show_ref.stdout).to_string(),
        batch_check: String::from_utf8_lossy(&batch.stdout).to_string(),
        batch_check_status: batch.status.code(),
        fsck_status: fsck.status.code(),
        objects: object_store_contents(repo),
    }
}

/// The isolation assertion. Used unchanged by the positive test and by the
/// negative control, so the control proves *this* assertion can fail.
fn assert_source_unchanged(before: &SourceFingerprint, after: &SourceFingerprint) {
    assert_eq!(after.config, before.config, ".git/config changed");
    assert_eq!(
        after.show_ref, before.show_ref,
        "git show-ref output changed"
    );
    assert_eq!(
        after.batch_check, before.batch_check,
        "git cat-file --batch-all-objects --batch-check output changed"
    );
    assert_eq!(
        after.batch_check_status, before.batch_check_status,
        "git cat-file --batch-all-objects --batch-check exit status changed"
    );
    assert_eq!(
        after.fsck_status,
        Some(0),
        "git fsck on the source no longer exits 0"
    );
    assert_eq!(
        after.objects.len(),
        before.objects.len(),
        "the number of files in the source object store changed"
    );
    assert!(
        after.objects == before.objects,
        "the source object store is not byte-identical"
    );
}

/// What the hostile routine actually managed to do.
#[derive(Debug)]
struct HostileReport {
    mutated_before_gc: usize,
    mutated_after_gc: usize,
    /// Object files that were hardlinked to something else at attack time.
    /// Zero under `--no-hardlinks`; non-zero for a default clone (M1).
    shared_inodes_seen: usize,
}

/// Everything an agent might plausibly do to a repository it does not own.
///
/// The step order is §4.1's, with one addition: object files are mutated
/// **before** `gc` as well as after. `git gc` runs `prune-packed` and
/// `repack -d`, both of which *unlink* the shared object files — and unlink only
/// decrements a link count (ADR-0001). Mutating solely after `gc`, as §4.1's
/// sentence reads literally, would therefore find nothing hardlinked left to
/// mutate and the negative control would silently pass while proving nothing.
fn hostile_routine(workspace: &Path) -> HostileReport {
    // 1. Point the repository somewhere else, and add a remote of our own.
    git_try(
        workspace,
        &[
            "remote",
            "set-url",
            "origin",
            "https://example.invalid/evil.git",
        ],
    );
    git_try(
        workspace,
        &["remote", "add", "evil", "https://example.invalid/evil.git"],
    );

    // 2. Delete a branch, and plant one.
    git_try(workspace, &["branch", "-D", "main"]);
    git_try(workspace, &["update-ref", "refs/heads/attacker", "HEAD"]);

    // 3. Write a hook that would fire under the operator's own hands.
    let hooks = workspace.join(".git/hooks");
    let _ = std::fs::create_dir_all(&hooks);
    let hook = hooks.join("pre-commit");
    if std::fs::write(&hook, "#!/bin/sh\necho pwned\nexit 1\n").is_ok()
        && let Ok(meta) = std::fs::metadata(&hook)
    {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(&hook, perms);
    }
    git_try(workspace, &["config", "core.hooksPath", ".git/hooks"]);

    // 4. Rewrite config directly, not through git.
    let config_path = workspace.join(".git/config");
    if let Ok(mut config) = std::fs::read_to_string(&config_path) {
        config.push_str("[conductor]\n\towned = true\n");
        let _ = std::fs::write(&config_path, config);
    }

    // 5. The one that actually corrupted the source in M2.
    let shared_inodes_seen = object_files(workspace)
        .iter()
        .filter(|p| std::fs::metadata(p).map(|m| m.nlink() > 1).unwrap_or(false))
        .count();
    let mutated_before_gc = mutate_object_files_in_place(workspace);

    // 6. Repack, prune, and expire the reflog.
    git_try(workspace, &["gc", "--prune=now", "--quiet"]);
    git_try(workspace, &["reflog", "expire", "--expire=now", "--all"]);

    // 7. And again, over whatever gc produced.
    let mutated_after_gc = mutate_object_files_in_place(workspace);

    HostileReport {
        mutated_before_gc,
        mutated_after_gc,
        shared_inodes_seen,
    }
}

fn object_files(repo: &Path) -> Vec<PathBuf> {
    let mut files = loose_object_files(repo);
    files.extend(pack_files(repo));
    files
}

/// Overwrite bytes in the middle of every object file, in place, without
/// changing its length.
///
/// In place matters. Replacing a file would unlink the old inode, which is
/// harmless to a hardlinked peer; writing *through* the same inode is what M2
/// showed reaches the source. Object files are mode `-r--r--r--`, but the same
/// user owns them, so `chmod u+w` succeeds trivially.
fn mutate_object_files_in_place(repo: &Path) -> usize {
    use std::io::{Seek, SeekFrom, Write};

    let mut mutated = 0;
    for path in object_files(repo) {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() < 16 {
            continue;
        }
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o200);
        if std::fs::set_permissions(&path, perms).is_err() {
            continue;
        }
        let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(&path) else {
            continue;
        };
        if file.seek(SeekFrom::Start(meta.len() / 2)).is_err() {
            continue;
        }
        if file
            .write_all(&[0xa5, 0x5a, 0xa5, 0x5a, 0xa5, 0x5a, 0xa5, 0x5a])
            .is_ok()
        {
            let _ = file.flush();
            mutated += 1;
        }
    }
    mutated
}

#[test]
fn a_hostile_agent_inside_a_conductor_workspace_cannot_reach_the_source() {
    let source = clean_repo();
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    let before = fingerprint(source.path());
    assert_eq!(
        before.fsck_status,
        Some(0),
        "the fixture must start healthy or the test proves nothing"
    );

    create_workspace(&WorkspaceRequest {
        source: source.path().to_path_buf(),
        workspace: ws.clone(),
        run_id: RunId::new("r-0041").expect("valid"),
        task_id: TaskId::new("T-0012").expect("valid"),
        base_commit: head,
        policy_hash: PolicyHash::new("blake3:deadbeef").expect("valid"),
    })
    .expect("create workspace");

    let report = hostile_routine(&ws);

    assert!(
        report.mutated_before_gc > 0 && report.mutated_after_gc > 0,
        "the routine mutated no object files, so it tested nothing: {report:?}"
    );
    let after = fingerprint(source.path());
    assert_source_unchanged(&before, &after);
    assert_eq!(
        report.shared_inodes_seen, 0,
        "a conductor workspace must share no object inode with the source (M3): {report:?}"
    );
}

#[test]
fn negative_control_the_same_assertion_fails_against_a_default_clone() {
    // §4.1: "This test is known non-vacuous: the default (hardlinked) clone
    // fails it (M2)." Asserted, not asserted-in-prose.
    //
    // If this test ever fails, do NOT weaken the positive test. Either git
    // stopped hardlinking local clones on this platform, or the hostile routine
    // stopped reaching shared inodes. Both mean the isolation test above has
    // gone vacuous and needs a new mechanism, and both contradict M1/M2.
    let source = clean_repo();
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("hardlinked");

    let before = fingerprint(source.path());
    assert_eq!(
        before.fsck_status,
        Some(0),
        "the fixture must start healthy"
    );

    // A plain `git clone`: exactly what ADR-0001 rejected.
    let clone = git_try(
        root.path(),
        &[
            "clone",
            "--no-checkout",
            &source.path().to_string_lossy(),
            &ws.to_string_lossy(),
        ],
    );
    assert!(clone.status.success(), "clone failed: {clone:?}");

    let report = hostile_routine(&ws);

    assert!(
        report.shared_inodes_seen > 0,
        "a default local clone shared no object inode with the source, which \
         contradicts M1. The isolation test above is now unproven: fix the \
         control, do not relax the assertion. Report: {report:?}"
    );
    assert!(
        report.mutated_before_gc > 0,
        "the routine mutated no object files: {report:?}"
    );

    let after = fingerprint(source.path());

    eprintln!("NEGATIVE CONTROL EVIDENCE");
    eprintln!("  report                : {report:?}");
    eprintln!("  source fsck before    : {:?}", before.fsck_status);
    eprintln!("  source fsck after     : {:?}", after.fsck_status);
    eprintln!(
        "  object bytes identical: {}",
        after.objects == before.objects
    );
    eprintln!(
        "  batch-check exit      : {:?} -> {:?}",
        before.batch_check_status, after.batch_check_status
    );
    eprintln!(
        "  fsck stderr           : {}",
        String::from_utf8_lossy(&git_try(source.path(), &["fsck", "--no-progress"]).stderr)
            .lines()
            .take(3)
            .collect::<Vec<_>>()
            .join(" / ")
    );
    eprintln!(
        "  cat-file HEAD         : {:?}",
        git_try(source.path(), &["cat-file", "-p", "HEAD"])
            .status
            .code()
    );

    // The damage, stated directly…
    assert!(
        after.fsck_status != Some(0) || after.objects != before.objects,
        "the hostile routine left the source undamaged through a hardlinked \
         clone. This contradicts M2 and means the positive isolation test may \
         be vacuous. Investigate before trusting it. Report: {report:?}, \
         fsck={:?}",
        after.fsck_status
    );

    // …and the same assertion the positive test relies on, shown to fail.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // the panic below is the expected result
    let outcome = std::panic::catch_unwind(|| assert_source_unchanged(&before, &after));
    std::panic::set_hook(previous_hook);

    assert!(
        outcome.is_err(),
        "assert_source_unchanged passed against a hardlinked clone, so it \
         cannot distinguish isolation from its absence. Report: {report:?}"
    );
}

#[test]
fn the_workspace_itself_is_left_visibly_damaged_so_row_15_has_something_to_detect() {
    // Acceptance row 15's other half: the clone *is* damaged, and Conductor is
    // expected to notice. Isolation is not "nothing happened"; it is "it
    // happened over here".
    let source = clean_repo();
    let root = tempfile::tempdir().expect("tempdir");
    let ws = root.path().join("run");
    let head = git_out(source.path(), &["rev-parse", "HEAD"]);

    create_workspace(&WorkspaceRequest {
        source: source.path().to_path_buf(),
        workspace: ws.clone(),
        run_id: RunId::new("r-0041").expect("valid"),
        task_id: TaskId::new("T-0012").expect("valid"),
        base_commit: head,
        policy_hash: PolicyHash::new("blake3:deadbeef").expect("valid"),
    })
    .expect("create workspace");

    hostile_routine(&ws);

    let fsck = git_try(&ws, &["fsck", "--no-progress"]);
    assert_ne!(
        fsck.status.code(),
        Some(0),
        "the workspace survived the hostile routine intact, so the routine is \
         not hostile enough to prove anything about the source"
    );
}
