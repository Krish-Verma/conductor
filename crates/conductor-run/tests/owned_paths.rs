//! Unique ownership of generated paths — carried forward from S0.
//!
//! S0's completion report records that "**the subagent's primary result file was
//! clobbered** by my own concurrent re-run with different parameters … Shared
//! output directories need provenance checks when more than one agent writes to
//! them."
//!
//! S3 is where concurrency starts: workers claim runs, supervisors write
//! artifacts, and startup recovery re-derives paths for runs it did not create.
//! Two independent workers computing the same artifact path is not a
//! hypothetical here — it is what a re-claimed run *does*. So the rule is made
//! structural before the concurrency arrives: **a generated path is created
//! exclusively or not at all, and it carries provenance saying who owns it.**
//!
//! The mechanism is `O_EXCL`, not a check-then-write. A "does it exist?" test
//! followed by a write is the same race in a longer form.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Barrier};

use conductor_core::RunId;
use conductor_run::paths::{ArtifactRoot, Owner, OwnershipError};

fn owner(name: &str) -> Owner {
    Owner::new(name, std::process::id() as i32)
}

#[test]
fn two_workers_cannot_both_claim_the_same_generated_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());
    let run = RunId::new("r-0041").expect("run id");

    let first = root
        .claim_attempt_dir(&run, 1, &owner("worker-a"))
        .expect("the first claim succeeds");

    let err = root
        .claim_attempt_dir(&run, 1, &owner("worker-b"))
        .expect_err("the second must be refused, not silently allowed");

    match err {
        OwnershipError::AlreadyOwned { path, by } => {
            assert_eq!(path, first.path().to_path_buf());
            assert_eq!(
                by.worker, "worker-a",
                "the refusal must name who actually holds it"
            );
        }
        other => panic!("expected AlreadyOwned, got {other:?}"),
    }
}

#[test]
fn the_first_writers_content_survives_a_second_workers_attempt() {
    // The S0 failure was not "an error was reported" — it was that a result
    // file was silently replaced. This is that exact assertion.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());
    let run = RunId::new("r-0041").expect("run id");

    let owned = root
        .claim_attempt_dir(&run, 1, &owner("worker-a"))
        .expect("claim");
    owned
        .write_new("result.json", b"{\"from\":\"worker-a\"}")
        .expect("write");

    let _ = root.claim_attempt_dir(&run, 1, &owner("worker-b"));

    let contents = std::fs::read_to_string(owned.path().join("result.json")).expect("read");
    assert_eq!(contents, "{\"from\":\"worker-a\"}");
}

#[test]
fn writing_an_existing_file_inside_an_owned_directory_is_also_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());
    let run = RunId::new("r-0041").expect("run id");
    let owned = root
        .claim_attempt_dir(&run, 1, &owner("worker-a"))
        .expect("claim");

    owned.write_new("report.json", b"first").expect("write");
    let err = owned
        .write_new("report.json", b"second")
        .expect_err("an existing artifact must not be overwritten");
    assert!(matches!(err, OwnershipError::AlreadyExists { .. }));
    assert_eq!(
        std::fs::read_to_string(owned.path().join("report.json")).expect("read"),
        "first"
    );
}

#[test]
fn provenance_says_who_generated_the_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());
    let run = RunId::new("r-0041").expect("run id");
    let owned = root
        .claim_attempt_dir(&run, 3, &owner("worker-a"))
        .expect("claim");

    let provenance = root
        .read_provenance(owned.path())
        .expect("read")
        .expect("a provenance record");
    assert_eq!(provenance.worker, "worker-a");
    assert_eq!(provenance.run_id, run);
    assert_eq!(provenance.attempt_ordinal, 3);
    assert_eq!(provenance.pid, std::process::id() as i32);
    assert!(provenance.created_at > 0);
}

#[test]
fn exactly_one_of_many_concurrent_claimants_wins() {
    // Threads rather than one-after-another calls: the check-then-create version
    // of this passes the sequential test and loses the race here.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());
    let run = RunId::new("r-0041").expect("run id");

    const WORKERS: usize = 8;
    let barrier = Arc::new(Barrier::new(WORKERS));
    let root = Arc::new(root);
    let run = Arc::new(run);

    let handles: Vec<_> = (0..WORKERS)
        .map(|i| {
            let barrier = Arc::clone(&barrier);
            let root = Arc::clone(&root);
            let run = Arc::clone(&run);
            std::thread::spawn(move || {
                barrier.wait();
                root.claim_attempt_dir(&run, 1, &owner(&format!("worker-{i}")))
                    .map(|owned| owned.path().to_path_buf())
                    .map_err(|e| e.to_string())
            })
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("join"))
        .collect();
    let winners: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one worker may own a generated path; {} did",
        winners.len()
    );

    let provenance = root
        .read_provenance(&root.attempt_dir(&run, 1))
        .expect("read")
        .expect("a provenance record");
    assert!(provenance.worker.starts_with("worker-"));
}

#[test]
fn different_attempts_of_one_run_get_different_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());
    let run = RunId::new("r-0041").expect("run id");

    let paths: BTreeSet<_> = (1..=4)
        .map(|ordinal| {
            root.claim_attempt_dir(&run, ordinal, &owner("worker-a"))
                .expect("claim")
                .path()
                .to_path_buf()
        })
        .collect();
    assert_eq!(paths.len(), 4, "attempts must not share a directory");
}

#[test]
fn different_runs_get_different_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());

    let a = root.attempt_dir(&RunId::new("r-0041").expect("id"), 1);
    let b = root.attempt_dir(&RunId::new("r-0042").expect("id"), 1);
    assert_ne!(a, b);
}

#[test]
fn the_path_is_deterministic_so_recovery_can_find_it_again() {
    // Recovery re-derives the path for a run it did not create. If the path
    // carried a timestamp or a random suffix, a restart could not locate the
    // artifacts of the attempt it is recovering.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());
    let run = RunId::new("r-0041").expect("run id");

    let first = root.attempt_dir(&run, 2);
    let second = ArtifactRoot::new(dir.path()).attempt_dir(&run, 2);
    assert_eq!(first, second);
}

#[test]
fn adopting_a_directory_this_worker_already_owns_is_allowed() {
    // Recovery legitimately re-enters its own attempt directory. Refusing that
    // would make a restart unable to finish the work it started, which is the
    // opposite of the point.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = ArtifactRoot::new(dir.path());
    let run = RunId::new("r-0041").expect("run id");

    let owner_a = owner("worker-a");
    let first = root.claim_attempt_dir(&run, 1, &owner_a).expect("claim");
    let again = root
        .reclaim_attempt_dir(&run, 1, &owner_a)
        .expect("the same owner may re-enter");
    assert_eq!(first.path(), again.path());

    let err = root
        .reclaim_attempt_dir(&run, 1, &owner("worker-b"))
        .expect_err("a different worker may not");
    assert!(matches!(err, OwnershipError::AlreadyOwned { .. }));
}
