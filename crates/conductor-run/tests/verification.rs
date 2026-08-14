//! The verification runner end to end — master plan §4.5, Part 9 rows 7, 8, 26.
//!
//! Test names begin `verify_` so that the slice's own command,
//! `cargo test -p conductor-run verify`, reaches both these and the in-module
//! unit tests under `verify::`.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use conductor_core::{Fence, RunId, VerificationOutcome};
use conductor_run::paths::{ArtifactRoot, Owner};
use conductor_run::verify::runner::{CheckKind, RunnerConfig, VerificationReport, run_profile};
use conductor_run::verify::{classify, profile};
use conductor_store::Store;

const RUN: &str = "r-0041";
const NOW: i64 = 1_770_000_000_000;

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

struct World {
    dir: tempfile::TempDir,
    store: Store,
    fence: Fence,
    workspace: PathBuf,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        make_repo(&workspace);

        let mut store = Store::open_or_create(dir.path().join("conductor.db")).expect("store");
        conductor_store::with_immediate(store.conn_mut(), |tx| {
            tx.execute(
                "INSERT INTO project (id, root_path, repo_identity, default_branch, config_hash, created_at)
                 VALUES ('p-1', '/fixture', 'blake3:repo', 'main', 'blake3:cfg', 0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO plan_version (id, project_id, version, content_hash, state, source_path)
                 VALUES ('pv-1', 'p-1', 1, 'blake3:plan', 'APPROVED', 'plan.yaml')",
                [],
            )?;
            tx.execute(
                "INSERT INTO policy_snapshot (hash, canonical_blob, created_at) VALUES (?1, '{}', 0)",
                rusqlite::params![common::agent::POLICY_HASH],
            )?;
            tx.execute(
                "INSERT INTO task (id, plan_version_id, slice_id, state, scope_globs,
                                   verification_profile, attempt_budget, created_at)
                 VALUES ('T-0012', 'pv-1', 'S4', 'READY', '[\"src/**\"]', 'default', 3, 0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO run (id, task_id, policy_hash, base_commit, run_branch, state,
                                  priority, lease_epoch, created_at)
                 VALUES (?1, 'T-0012', ?2, 'abc123', 'conductor/T-0012/r-0041', 'READY', 100, 0, 0)",
                rusqlite::params![RUN, common::agent::POLICY_HASH],
            )?;
            Ok(())
        })
        .expect("seed");

        let fence = store
            .claim_run(&RunId::new(RUN).expect("id"), "worker-1", NOW, 60_000)
            .expect("claim")
            .expect("a READY run is claimable")
            .fence();

        World {
            dir,
            store,
            fence,
            workspace,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config(&self) -> RunnerConfig {
        let mut env = BTreeMap::new();
        if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".to_string(), path);
        }
        RunnerConfig {
            workspace: self.workspace.clone(),
            scratch_index: self.dir.path().join("scratch").join("verify-index"),
            artifacts: ArtifactRoot::new(self.dir.path().join("artifacts")),
            run_id: RunId::new(RUN).expect("id"),
            attempt_ordinal: 1,
            attempt_id: None,
            owner: Owner::new("worker-1", std::process::id() as i32),
            env,
            commit_sha: "abc123".to_string(),
            changed_paths: Vec::new(),
            startup_grace: std::time::Duration::from_secs(30),
        }
    }

    fn run(&mut self, yaml: &str) -> VerificationReport {
        self.run_with(yaml, self.config())
    }

    fn run_with(&mut self, yaml: &str, config: RunnerConfig) -> VerificationReport {
        let loaded = profile::parse(yaml).expect("profile");
        run_profile(&mut self.store, &self.fence, &config, &loaded, NOW).expect("run profile")
    }
}

fn make_repo(repo: &Path) {
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    std::fs::create_dir_all(repo.join("target")).expect("mkdir");
    std::fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n").expect("write");
    std::fs::write(repo.join(".gitignore"), "/target/\n").expect("write");
    git(repo, &["init", "--initial-branch=main"]);
    git(repo, &["config", "user.name", "Fixture"]);
    git(repo, &["config", "user.email", "fixture@localhost"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-m", "initial"]);
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A YAML list literal for a `/bin/sh -c <script>` command.
fn sh(script: &str) -> String {
    format!(
        "[\"/bin/sh\", \"-c\", {}]",
        serde_json::to_string(script).expect("json string")
    )
}

fn outcome(report: &VerificationReport, check_id: &str) -> VerificationOutcome {
    report
        .results
        .iter()
        .find(|r| r.check_id == check_id)
        .unwrap_or_else(|| panic!("no result for {check_id}: {:?}", report.results))
        .outcome
}

// ---------------------------------------------------------------------------
// the four outcomes
// ---------------------------------------------------------------------------

#[test]
fn verify_a_check_that_exits_zero_is_pass() {
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: ok\n      command: {}\n",
        sh("echo fine; exit 0")
    ));
    assert_eq!(outcome(&report, "ok"), VerificationOutcome::Pass);
    assert!(report.findings.is_empty());
}

#[test]
fn verify_a_check_that_exits_non_zero_is_fail() {
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: bad\n      command: {}\n",
        sh("echo broken; exit 3")
    ));
    assert_eq!(outcome(&report, "bad"), VerificationOutcome::Fail);
    let result = &report.results[0];
    assert_eq!(result.exit_code, Some(3));
}

#[test]
fn verify_a_check_that_overruns_is_inconclusive_and_not_a_failure() {
    // Part 9 row 8: verification timeout → INCONCLUSIVE, **no budget spent**.
    // The script speaks before it hangs, so the hang is measured against the
    // check's own budget rather than against the startup grace.
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: hangs\n      command: {}\n      timeout_seconds: 1\n",
        sh("echo starting; sleep 60")
    ));
    assert_eq!(outcome(&report, "hangs"), VerificationOutcome::Inconclusive);
}

#[test]
fn verify_a_check_that_overruns_fails_only_where_the_profile_asks_for_that() {
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: hangs\n      command: {}\n      timeout_seconds: 1\n      on_timeout: fail\n",
        sh("echo starting; sleep 60")
    ));
    assert_eq!(outcome(&report, "hangs"), VerificationOutcome::Fail);
}

#[test]
fn verify_a_check_whose_program_is_missing_is_inconclusive_never_fail() {
    // D8's rule, and §4.5's reason for four outcomes: a vanished toolchain is
    // not evidence that the code is wrong, and routing it to repair would spend
    // agent attempts on an environment problem.
    let mut w = World::new();
    let report = w.run(
        "verification:\n  required:\n    - id: gone\n      command: [\"/nonexistent/bin/cargo\", \"check\"]\n",
    );
    assert_eq!(outcome(&report, "gone"), VerificationOutcome::Inconclusive);
}

// ---------------------------------------------------------------------------
// VOID — Part 9 row 26
// ---------------------------------------------------------------------------

/// Two FIFOs, giving an exact rendezvous with the background writer.
///
/// A FIFO blocks on `open` until both ends are present, so this is
/// synchronisation rather than a sleep that hopes. The ordering it forces is:
///
/// ```text
/// runner: hash tree (before)
/// runner: spawn check
/// check:  echo > started        ─┐ blocks until the writer opens `started`
/// writer: read < started        ─┘ rendezvous: the check is definitely running
/// writer: write into workspace
/// writer: echo > proceed        ─┐ blocks until the check opens `proceed`
/// check:  read < proceed        ─┘ rendezvous: the write is definitely done
/// check:  exit 0
/// runner: hash tree (after)
/// ```
///
/// The write therefore lands strictly after the check started and strictly
/// before it exited. There is no ordering in which it falls outside the window,
/// so the test cannot pass or fail by luck.
///
/// The FIFOs live **outside** the workspace. Inside, they would themselves be
/// untracked files that moved the tree, and the test would pass while proving
/// nothing.
struct Rendezvous {
    started: PathBuf,
    proceed: PathBuf,
}

impl Rendezvous {
    fn new(sync_dir: &Path) -> Rendezvous {
        std::fs::create_dir_all(sync_dir).expect("mkdir");
        let started = sync_dir.join("started");
        let proceed = sync_dir.join("proceed");
        for fifo in [&started, &proceed] {
            let status = Command::new("mkfifo")
                .arg(fifo)
                .status()
                .expect("mkfifo must be available");
            assert!(status.success(), "mkfifo {}", fifo.display());
        }
        Rendezvous { started, proceed }
    }

    /// The check: announce, then wait to be released.
    fn check_script(&self) -> String {
        format!(
            "echo check-running; echo go > {started}; read _ < {proceed}; exit 0",
            started = self.started.display(),
            proceed = self.proceed.display(),
        )
    }

    /// The background writer, as a detached process.
    fn spawn_writer(&self, write_to: &Path) -> std::process::Child {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "read _ < {started}; printf 'mutated by a stray watcher' > {target}; \
                 echo released > {proceed}",
                started = self.started.display(),
                target = write_to.display(),
                proceed = self.proceed.display(),
            ))
            .spawn()
            .expect("spawn the background writer")
    }
}

#[test]
fn verify_a_tree_mutated_by_a_background_process_mid_check_is_void() {
    // Part 9 row 26 · §4.5: "a result whose tree moved under it is VOID, not
    // PASS". The check below exits 0 — without the after-check tree hash this
    // is an unambiguous PASS.
    let mut w = World::new();
    let rendezvous = Rendezvous::new(&w.path().join("sync"));
    let mut writer = rendezvous.spawn_writer(&w.workspace.join("src").join("injected.rs"));

    let report = w.run(&format!(
        "verification:\n  required:\n    - id: racy\n      command: {}\n      timeout_seconds: 30\n",
        sh(&rendezvous.check_script())
    ));
    writer.wait().expect("the writer must finish");

    assert_eq!(
        outcome(&report, "racy"),
        VerificationOutcome::Void,
        "a green check on a tree that moved under it must be VOID"
    );
    assert_eq!(
        report.results[0].exit_code,
        Some(0),
        "the check itself passed"
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == classify::TREE_MUTATED_DURING_CHECK),
        "a void result must raise a finding: {:?}",
        report.findings
    );
    // And the mutation really did land inside the window.
    assert!(w.workspace.join("src").join("injected.rs").exists());
}

#[test]
fn verify_a_void_result_is_never_cached() {
    // §4.5's cache is keyed by tree hash. A VOID says "we do not know which
    // tree this observed", so filing it under one as that tree's answer would
    // be a lie the cache then serves back.
    let mut w = World::new();
    let rendezvous = Rendezvous::new(&w.path().join("sync"));
    let mut writer = rendezvous.spawn_writer(&w.workspace.join("src").join("injected.rs"));
    let yaml = format!(
        "verification:\n  required:\n    - id: racy\n      command: {}\n      timeout_seconds: 30\n",
        sh(&rendezvous.check_script())
    );
    let report = w.run(&yaml);
    writer.wait().expect("writer");
    assert_eq!(outcome(&report, "racy"), VerificationOutcome::Void);
    assert!(!report.results[0].from_cache);

    // Re-running at the (now settled) tree must actually run the check again
    // rather than find the void in the cache.
    let second = w.run(&format!(
        "verification:\n  required:\n    - id: racy\n      command: {}\n",
        sh("echo settled; exit 0")
    ));
    assert!(!second.results[0].from_cache);
    assert_eq!(outcome(&second, "racy"), VerificationOutcome::Pass);
}

// ---------------------------------------------------------------------------
// the cache
// ---------------------------------------------------------------------------

/// A check that records every invocation outside the workspace, so that
/// "did it run?" is a fact rather than an inference from a timing figure.
fn counting_check(counter: &Path) -> String {
    sh(&format!("echo ran >> {}; exit 0", counter.display()))
}

fn runs(counter: &Path) -> usize {
    std::fs::read_to_string(counter)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[test]
fn verify_an_unchanged_tree_is_a_cache_hit_and_the_command_does_not_run_again() {
    let mut w = World::new();
    let counter = w.path().join("invocations");
    let yaml = format!(
        "verification:\n  required:\n    - id: counted\n      command: {}\n",
        counting_check(&counter)
    );

    let first = w.run(&yaml);
    assert_eq!(outcome(&first, "counted"), VerificationOutcome::Pass);
    assert!(!first.results[0].from_cache);
    assert_eq!(runs(&counter), 1);

    let second = w.run(&yaml);
    assert_eq!(outcome(&second, "counted"), VerificationOutcome::Pass);
    assert!(
        second.results[0].from_cache,
        "the second run must be a lookup"
    );
    assert_eq!(runs(&counter), 1, "a cache hit must not re-run the command");
}

#[test]
fn verify_a_changed_tree_misses_the_cache() {
    let mut w = World::new();
    let counter = w.path().join("invocations");
    let yaml = format!(
        "verification:\n  required:\n    - id: counted\n      command: {}\n",
        counting_check(&counter)
    );

    w.run(&yaml);
    std::fs::write(
        w.workspace.join("src/lib.rs"),
        "pub fn a() { /* edit */ }\n",
    )
    .expect("write");
    let second = w.run(&yaml);

    assert!(!second.results[0].from_cache);
    assert_eq!(runs(&counter), 2);
}

#[test]
fn verify_a_changed_toolchain_misses_the_cache() {
    // D5, and §11.2's "passed *when*, on *what tree*, with *what toolchain*".
    let mut w = World::new();
    let counter = w.path().join("invocations");
    let yaml = |version: &str| {
        format!(
            "verification:\n  toolchain_fingerprint:\n    - {}\n  required:\n    - id: counted\n      command: {}\n",
            sh(&format!("echo rustc {version}")),
            counting_check(&counter)
        )
    };

    w.run(&yaml("1.97.0"));
    assert_eq!(runs(&counter), 1);

    // Same tree, same command, different toolchain.
    let second = w.run(&yaml("1.98.0"));
    assert!(
        !second.results[0].from_cache,
        "a toolchain upgrade must invalidate the cached result"
    );
    assert_eq!(runs(&counter), 2);

    // And going back to the first toolchain finds the first result again.
    let third = w.run(&yaml("1.97.0"));
    assert!(third.results[0].from_cache);
    assert_eq!(runs(&counter), 2);
}

#[test]
fn verify_a_removed_toolchain_misses_the_cache_and_the_rerun_is_inconclusive() {
    // The two halves of D8's "remove the toolchain between runs": the
    // fingerprint moves (so the cache misses) and the re-run cannot spawn (so
    // it is INCONCLUSIVE, not FAIL).
    let mut w = World::new();
    let tool = w.path().join("bin").join("faketool");
    std::fs::create_dir_all(tool.parent().expect("parent")).expect("mkdir");
    std::fs::write(&tool, "#!/bin/sh\necho faketool 1.0\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let yaml = format!(
        "verification:\n  toolchain_fingerprint:\n    - [\"{tool}\"]\n  required:\n    - id: uses-tool\n      command: [\"{tool}\"]\n",
        tool = tool.display()
    );

    let first = w.run(&yaml);
    assert_eq!(outcome(&first, "uses-tool"), VerificationOutcome::Pass);

    std::fs::remove_file(&tool).expect("remove the toolchain");

    let second = w.run(&yaml);
    assert!(
        !second.results[0].from_cache,
        "the fingerprint must have moved"
    );
    assert_eq!(
        outcome(&second, "uses-tool"),
        VerificationOutcome::Inconclusive,
        "a missing toolchain is infrastructure, never a failing check"
    );
}

#[test]
fn verify_a_cached_result_survives_losing_the_worker() {
    // §4.7 step 6: "re-run verification only if the tree hash has no cached
    // valid result". A fresh store handle standing in for a restarted daemon
    // must find the earlier answer.
    let mut w = World::new();
    let counter = w.path().join("invocations");
    let yaml = format!(
        "verification:\n  required:\n    - id: counted\n      command: {}\n",
        counting_check(&counter)
    );
    w.run(&yaml);
    assert_eq!(runs(&counter), 1);

    // Reopen the database the way a restarted process would, and reclaim.
    let db = w.path().join("conductor.db");
    drop(std::mem::replace(
        &mut w.store,
        Store::open_or_create(&db).expect("reopen"),
    ));
    w.store.expire_leases(NOW + 120_000).expect("sweep");
    w.fence = w
        .store
        .claim_run(
            &RunId::new(RUN).expect("id"),
            "worker-2",
            NOW + 120_000,
            60_000,
        )
        .expect("claim")
        .expect("a swept run is claimable")
        .fence();

    let after_restart = w.run(&yaml);
    assert!(after_restart.results[0].from_cache);
    assert_eq!(
        runs(&counter),
        1,
        "a restart must not re-run a decided tree"
    );
}

// ---------------------------------------------------------------------------
// conditional and invariant checks
// ---------------------------------------------------------------------------

#[test]
fn verify_a_conditional_check_runs_only_when_the_diff_touches_its_paths() {
    let mut w = World::new();
    let counter = w.path().join("invocations");
    let yaml = format!(
        "verification:\n  conditional:\n    - when: {{changed_paths: [\"migrations/**\"]}}\n      checks:\n        - id: migrate\n          command: {}\n",
        counting_check(&counter)
    );

    let mut config = w.config();
    config.changed_paths = vec!["src/lib.rs".to_string()];
    let untriggered = w.run_with(&yaml, config);
    assert!(
        untriggered.results.is_empty(),
        "an untriggered conditional must not run: {:?}",
        untriggered.results
    );
    assert_eq!(runs(&counter), 0);

    let mut config = w.config();
    config.changed_paths = vec!["migrations/001_init.sql".to_string()];
    let triggered = w.run_with(&yaml, config);
    assert_eq!(outcome(&triggered, "migrate"), VerificationOutcome::Pass);
    assert_eq!(triggered.results[0].kind, CheckKind::Conditional);
    assert_eq!(runs(&counter), 1);
}

#[test]
fn verify_invariants_run_whatever_the_diff_says() {
    // §4.5: "cheap, always, never skipped".
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  invariants:\n    - id: no-secrets\n      command: {}\n",
        sh("exit 0")
    ));
    assert_eq!(outcome(&report, "no-secrets"), VerificationOutcome::Pass);
    assert_eq!(report.results[0].kind, CheckKind::Invariant);
}

#[test]
fn verify_checks_run_sequentially_in_one_workspace() {
    // §11.2: "No matrix, no cross-check parallelism. If you want a matrix, you
    // want CI." Two checks appending to one file record the order they ran in.
    let mut w = World::new();
    let order = w.path().join("order");
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: first\n      command: {}\n    - id: second\n      command: {}\n",
        sh(&format!("echo first >> {}", order.display())),
        sh(&format!("echo second >> {}", order.display()))
    ));
    assert_eq!(report.results.len(), 2);
    assert_eq!(
        std::fs::read_to_string(&order).expect("order"),
        "first\nsecond\n"
    );
}

// ---------------------------------------------------------------------------
// flaky retry
// ---------------------------------------------------------------------------

#[test]
fn verify_a_flaky_check_whose_two_runs_disagree_is_inconclusive() {
    // §4.5: "exactly one; disagreement ⇒ INCONCLUSIVE" — explicitly not "the
    // good one wins". The script fails the first time and passes the second.
    let mut w = World::new();
    let counter = w.path().join("invocations");
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: flaky\n      command: {}\n      flaky_retry: 1\n",
        sh(&format!(
            "echo ran >> {c}; if [ $(wc -l < {c}) -eq 1 ]; then exit 1; fi; exit 0",
            c = counter.display()
        ))
    ));

    assert_eq!(runs(&counter), 2, "flaky_retry: 1 means exactly one retry");
    assert_eq!(
        outcome(&report, "flaky"),
        VerificationOutcome::Inconclusive,
        "the passing run must not win"
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == classify::FLAKY_CHECK_DISAGREEMENT)
    );
}

#[test]
fn verify_a_check_that_fails_twice_is_a_failure_not_an_inconclusive() {
    let mut w = World::new();
    let counter = w.path().join("invocations");
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: flaky\n      command: {}\n      flaky_retry: 1\n",
        sh(&format!("echo ran >> {}; exit 1", counter.display()))
    ));
    assert_eq!(runs(&counter), 2);
    assert_eq!(outcome(&report, "flaky"), VerificationOutcome::Fail);
}

#[test]
fn verify_a_passing_check_is_not_retried() {
    let mut w = World::new();
    let counter = w.path().join("invocations");
    w.run(&format!(
        "verification:\n  required:\n    - id: fine\n      command: {}\n      flaky_retry: 1\n",
        counting_check(&counter)
    ));
    assert_eq!(runs(&counter), 1);
}

// ---------------------------------------------------------------------------
// logs and secrets
// ---------------------------------------------------------------------------

#[test]
fn verify_the_log_goes_to_the_path_4_5_names_with_both_streams_and_a_digest() {
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: talkative\n      command: {}\n",
        sh("echo hello from stdout; echo hello from stderr >&2; exit 0")
    ));

    let result = &report.results[0];
    let log = result.log_path.as_ref().expect("a log path");
    assert!(log.exists(), "the log was not written");
    assert_eq!(
        log,
        &w.path()
            .join("artifacts")
            .join(RUN)
            .join("verification")
            .join("talkative-1.log"),
        "§4.5: artifacts/<run>/verification/<check>-<attempt>.log"
    );

    let contents = std::fs::read_to_string(log).expect("read log");
    assert!(contents.contains("hello from stdout"));
    assert!(contents.contains("hello from stderr"));

    let digest = result.log_digest.as_ref().expect("a digest");
    assert_eq!(
        digest,
        &conductor_core::effect::content_hash(&std::fs::read(log).expect("bytes")),
        "the recorded digest must be of the log that was written"
    );
}

#[test]
fn verify_a_long_log_is_carried_as_a_bounded_excerpt_never_in_full() {
    // §4.5: logs are "never inlined into packets", but an *excerpt* is exactly
    // what a packet carries. So the property is not "no log text in the
    // report" — it is that the report carries a bounded tail and a pointer,
    // however large the log gets.
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: chatty\n      command: {}\n",
        sh("i=1; while [ $i -le 2000 ]; do echo log-line-$i; i=$((i+1)); done; exit 0")
    ));

    let result = &report.results[0];
    let log = std::fs::read_to_string(result.log_path.as_ref().expect("log")).expect("read");
    assert_eq!(log.lines().count(), 2000);

    let excerpt = result.excerpt.as_ref().expect("an excerpt");
    assert!(
        excerpt.lines().count() <= conductor_run::verify::runner::EXCERPT_LINES,
        "the excerpt is unbounded: {} lines",
        excerpt.lines().count()
    );
    assert!(
        excerpt.contains("log-line-2000"),
        "the tail is what matters"
    );

    let report_json = serde_json::to_string(&report).expect("serialize");
    assert!(
        !report_json.contains("log-line-1\n") && !report_json.contains("log-line-500"),
        "the whole log reached the report"
    );
    assert!(
        report_json.len() < log.len() / 4,
        "the report is not meaningfully smaller than the log it points at"
    );
}

#[test]
fn verify_a_secret_in_a_log_is_redacted_before_it_reaches_an_excerpt() {
    // §4.5: "secret-scanned before any excerpt enters a packet". §11.2 requires
    // this be tested with planted secrets. The value below is synthetic.
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: leaky\n      command: {}\n",
        sh(
            "echo 'remote: rejected, ghp_1234567890abcdefghijklmnopqrstuvwxyzAB is revoked'; exit 1"
        )
    ));

    let result = &report.results[0];
    let excerpt = result.excerpt.as_ref().expect("an excerpt");
    assert!(
        !excerpt.contains("ghp_1234567890abcdefghijklmnopqrstuvwxyzAB"),
        "the secret reached the excerpt: {excerpt}"
    );
    assert!(excerpt.contains("[REDACTED:github-token]"));
    // The diagnosis survives: an excerpt a repair agent cannot read is a
    // wasted attempt.
    assert!(excerpt.contains("remote: rejected"));

    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == classify::SECRET_IN_VERIFICATION_LOG),
        "a secret in a log is a finding: {:?}",
        report.findings
    );

    // The log on disk is deliberately untouched — it is evidence, and it is
    // never the thing that travels.
    let raw = std::fs::read_to_string(result.log_path.as_ref().expect("log")).expect("read");
    assert!(raw.contains("ghp_1234567890abcdefghijklmnopqrstuvwxyzAB"));
}

// ---------------------------------------------------------------------------
// the profile itself
// ---------------------------------------------------------------------------

#[test]
fn verify_an_unknown_profile_key_becomes_a_finding_rather_than_a_refusal() {
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: ok\n      command: {}\n      timeout_second: 5\n",
        sh("exit 0")
    ));
    assert_eq!(outcome(&report, "ok"), VerificationOutcome::Pass);
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.kind == classify::PROFILE_UNKNOWN_KEY
                && f.detail.contains("timeout_second")),
        "a typo in a human-written profile must reach a human: {:?}",
        report.findings
    );
}

#[test]
fn verify_every_result_is_persisted_with_the_tree_it_observed() {
    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: ok\n      command: {}\n",
        sh("exit 0")
    ));
    let tree = report.results[0].tree_hash.clone();

    let rows: Vec<(String, String)> = w
        .store
        .conn()
        .prepare("SELECT check_id, tree_hash FROM verification_check")
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");

    assert_eq!(rows, vec![("ok".to_string(), tree)]);
}

// ---------------------------------------------------------------------------
// the bridge into §4.5's completion gate
// ---------------------------------------------------------------------------

#[test]
fn verify_a_green_report_produces_evidence_the_completion_gate_accepts() {
    // The mapping from real results to the gate's input lives in one tested
    // place, so S5 does not have to invent it — and cannot invent it wrongly.
    use conductor_core::completion::{
        AcceptanceEvidence, CompletionEvidence, FindingsEvidence, PolicyEvidence,
        ReconciliationEvidence, Slice, evaluate,
    };

    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: typecheck\n      command: {}\n  invariants:\n    - id: no-secrets\n      command: {}\n",
        sh("exit 0"),
        sh("exit 0")
    ));
    let tree = report.results[0].tree_hash.clone();

    let evidence = CompletionEvidence {
        tree_hash: tree.clone(),
        required: report.checks_evidence(CheckKind::Required),
        conditional: report.checks_evidence(CheckKind::Conditional),
        invariants: report.checks_evidence(CheckKind::Invariant),
        findings: FindingsEvidence::unresolved(0),
        reconciliation: ReconciliationEvidence::Clean,
        acceptance: AcceptanceEvidence::NotEvaluated {
            owner: Slice::S11PlanLedger,
        },
        policy: PolicyEvidence::NotEvaluated {
            owner: Slice::S7Policy,
        },
    };
    let verified = evaluate(&evidence).expect("a green profile completes");
    assert_eq!(verified.tree_hash(), tree);
}

#[test]
fn verify_an_inconclusive_check_cannot_complete_a_task() {
    // The whole reason INCONCLUSIVE is not FAIL is that they route differently
    // — but neither of them completes anything.
    use conductor_core::completion::{
        AcceptanceEvidence, CompletionEvidence, FindingsEvidence, PolicyEvidence,
        ReconciliationEvidence, Slice, evaluate,
    };

    let mut w = World::new();
    let report = w.run(&format!(
        "verification:\n  required:\n    - id: hangs\n      command: {}\n      timeout_seconds: 1\n",
        sh("echo starting; sleep 60")
    ));
    assert_eq!(outcome(&report, "hangs"), VerificationOutcome::Inconclusive);

    let evidence = CompletionEvidence {
        tree_hash: report.results[0].tree_hash.clone(),
        required: report.checks_evidence(CheckKind::Required),
        conditional: report.checks_evidence(CheckKind::Conditional),
        invariants: report.checks_evidence(CheckKind::Invariant),
        findings: FindingsEvidence::unresolved(0),
        reconciliation: ReconciliationEvidence::Clean,
        acceptance: AcceptanceEvidence::NotEvaluated {
            owner: Slice::S11PlanLedger,
        },
        policy: PolicyEvidence::NotEvaluated {
            owner: Slice::S7Policy,
        },
    };
    let refusals = evaluate(&evidence).expect_err("INCONCLUSIVE is not a pass");
    assert!(refusals[0].detail.contains("INCONCLUSIVE"));
}

#[test]
fn verify_re_verifying_the_same_check_never_overwrites_the_earlier_log() {
    // §4.5 names the log `<check>-<attempt>.log`, which is not unique: one
    // attempt can legitimately verify the same check at two trees — that is
    // exactly what happens after a VOID, where §4.5 says to "re-run at the new
    // tree". Truncating the first log would destroy the evidence behind the
    // finding that caused the re-run.
    let mut w = World::new();
    let yaml = format!(
        "verification:\n  required:\n    - id: twice\n      command: {}\n",
        sh("echo first-tree-evidence; exit 0")
    );
    let first = w.run(&yaml);
    let first_log = first.results[0].log_path.clone().expect("log");

    // Move the tree, so the second run is a genuine cache miss.
    std::fs::write(
        w.workspace.join("src/lib.rs"),
        "pub fn a() { /* moved */ }\n",
    )
    .expect("write");
    let second_yaml = format!(
        "verification:\n  required:\n    - id: twice\n      command: {}\n",
        sh("echo second-tree-evidence; exit 0")
    );
    let second = w.run(&second_yaml);
    let second_log = second.results[0].log_path.clone().expect("log");

    assert_ne!(
        first_log, second_log,
        "the second run reused the first log path"
    );
    assert!(
        std::fs::read_to_string(&first_log)
            .expect("the first log must still exist")
            .contains("first-tree-evidence"),
        "the first tree's evidence was destroyed"
    );
    assert!(
        std::fs::read_to_string(&second_log)
            .expect("second log")
            .contains("second-tree-evidence")
    );
}
