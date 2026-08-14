//! One test per fake-agent scenario — the S3 slice's "Tests" line.
//!
//! > **Tests.** One per scenario, asserting persisted state, attempt outcome,
//! > reconciliation verdict.
//!
//! Each case runs the real sequence — claim, workspace, baseline, spawn,
//! supervise, classify, reconcile, route — against a real git clone and a real
//! subprocess, and then asserts what the **database** says. Not what the run
//! returned: §4.7's recovery reads the database, so the database is what has to
//! be right.

mod common;

use std::path::Path;
use std::time::Duration;

use conductor_agent::fake::FakeAgent;
use conductor_core::{AttemptOutcome, AttemptState, RunState};
use conductor_git::{Scope, SensitivePatterns, Verdict};
use conductor_run::supervise::SupervisorConfig;
use conductor_run::worker::{AttemptOutcomeRecord, WorkerConfig, run_one_attempt};
use conductor_store::Store;

use common::agent::{fake_agent_binary, scenario_file, warm_the_binary};

const NOW: i64 = 1_770_000_000_000;
const LEASE_MS: i64 = 60_000;

struct Harness {
    _dir: tempfile::TempDir,
    store: Store,
    config: WorkerConfig,
}

/// Budgets long enough that no timer can fire.
///
/// The default, because most scenarios assert how an agent *ended itself* —
/// `EXITED`, `CRASHED` — and §6.4 makes a timeout outrank the signal Conductor
/// used to enforce it. A budget short enough to fire first therefore does not
/// make those tests stricter, it makes them race: under parallel load a scenario
/// that shells out to `git`, or merely sleeps, overruns a sub-second idle budget
/// and the attempt is correctly recorded `TIMED_OUT` instead. Observed, not
/// theorised: it is what made `crash_matrix.rs` flake.
const NO_TIMERS: SupervisorConfig = SupervisorConfig {
    startup_timeout: Duration::from_secs(60),
    idle_timeout: Duration::from_secs(60),
    wall_timeout: Duration::from_secs(120),
    terminate_grace: Duration::from_millis(300),
    poll_interval: Duration::from_millis(10),
};

/// Budgets short enough that the timers are what is being measured.
///
/// Used only by the two tests whose subject *is* a timeout. There the short
/// budget is the instrument, not an obstacle.
const TIGHT_TIMERS: SupervisorConfig = SupervisorConfig {
    // M29's absorber, generous even here: this budget covers the operating
    // system's first-execution scan, never the agent's work.
    startup_timeout: Duration::from_secs(60),
    idle_timeout: Duration::from_millis(700),
    wall_timeout: Duration::from_secs(3),
    terminate_grace: Duration::from_millis(300),
    poll_interval: Duration::from_millis(10),
};

fn harness(root: &Path, scope: &[&str], supervisor: SupervisorConfig) -> WorkerConfig {
    WorkerConfig {
        worker_id: "worker-1".to_string(),
        workspaces_root: root.join("workspaces"),
        artifacts_root: root.join("artifacts"),
        source_repo: root.join("source"),
        supervisor,
        lease_ms: LEASE_MS,
        heartbeat_interval: Duration::from_millis(100),
        scope: Scope::new(scope.iter().map(|s| s.to_string())),
        sensitive: SensitivePatterns::default(),
        agent_env_extra: Default::default(),
        agent_session_id: None,
    }
}

/// Build a store, a source repository and a seeded `READY` run.
fn setup(scope: &[&str]) -> Harness {
    setup_with(scope, NO_TIMERS)
}

fn setup_with(scope: &[&str], supervisor: SupervisorConfig) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = common::agent::source_repo(dir.path());
    let mut store = Store::open_or_create(dir.path().join("conductor.db")).expect("store");
    seed_run(&mut store, &common::agent::head(&source));
    let config = harness(dir.path(), scope, supervisor);
    Harness {
        _dir: dir,
        store,
        config,
    }
}

fn seed_run(store: &mut Store, base_commit: &str) {
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
             VALUES ('T-0012', 'pv-1', 'S3', 'READY', '[\"src/**\"]', 'default', 3, 0)",
            [],
        )?;
        tx.execute(
            "INSERT INTO run (id, task_id, policy_hash, base_commit, run_branch, state,
                              priority, lease_epoch, created_at)
             VALUES ('r-0041', 'T-0012', ?1, ?2, 'conductor/T-0012/r-0041', 'READY', 100, 0, 0)",
            rusqlite::params![common::agent::POLICY_HASH, base_commit],
        )?;
        Ok(())
    })
    .expect("seed");
}

/// Run one scenario end to end and hand back what the database says.
fn run_scenario(scenario_id: &str, scope: &[&str]) -> (Harness, AttemptOutcomeRecord) {
    run_scenario_with(scenario_id, scope, NO_TIMERS)
}

fn run_scenario_with(
    scenario_id: &str,
    scope: &[&str],
    supervisor: SupervisorConfig,
) -> (Harness, AttemptOutcomeRecord) {
    warm_the_binary();
    let mut harness = setup_with(scope, supervisor);
    let scenario = scenario_file(harness._dir.path(), scenario_id);
    let adapter = FakeAgent::new(fake_agent_binary(), scenario).with_max_lifetime_ms(20_000);

    let claimed = harness
        .store
        .claim_next_run("worker-1", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");
    assert_eq!(claimed.state, RunState::Running);

    let outcome = run_one_attempt(
        &mut harness.store,
        &claimed.fence(),
        &adapter,
        &harness.config,
        &mut (),
    )
    .expect("the attempt ran");
    (harness, outcome)
}

fn persisted_attempt(store: &Store) -> (AttemptState, AttemptOutcome) {
    let row = store
        .attempts_for_run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("attempts")
        .pop()
        .expect("one attempt");
    (row.state, row.outcome.expect("an outcome"))
}

fn persisted_run_state(store: &Store) -> RunState {
    store
        .run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("run")
        .expect("a row")
        .state
}

// ---------------------------------------------------------------------------

#[test]
fn success_exits_cleanly_and_reconciles_as_complete() {
    let (harness, outcome) = run_scenario("success", &["src/**"]);
    assert_eq!(outcome.verdict, Verdict::CleanComplete);
    assert_eq!(
        persisted_attempt(&harness.store),
        (AttemptState::Reconciled, AttemptOutcome::Exited)
    );
    // Not COMPLETE: §5.2 forbids any → COMPLETE without verification bound to
    // the final tree hash, and verification is S4.
    assert_eq!(persisted_run_state(&harness.store), RunState::Verifying);
}

#[test]
fn success_with_report_reads_the_report_from_the_file_channel() {
    let (harness, outcome) = run_scenario("success-with-report", &["src/**"]);
    assert_eq!(outcome.verdict, Verdict::CleanComplete);
    assert_eq!(persisted_run_state(&harness.store), RunState::Verifying);
    assert_eq!(
        persisted_attempt(&harness.store),
        (AttemptState::Reconciled, AttemptOutcome::Exited)
    );
}

#[test]
fn partial_edits_are_clean_work_the_agent_admits_is_unfinished() {
    let (harness, outcome) = run_scenario("partial-edits", &["src/**"]);
    // The agent's honesty is not the verdict: the repository shows in-scope
    // changes and a consistent report, which is CLEAN_COMPLETE. Whether the
    // *task* is done is verification's question, not reconciliation's.
    assert_eq!(outcome.verdict, Verdict::CleanComplete);
    assert_eq!(persisted_run_state(&harness.store), RunState::Verifying);
}

#[test]
fn crash_before_edits_is_crashed_and_no_change() {
    // Acceptance row 2.
    let (harness, outcome) = run_scenario("crash-before-edits", &["src/**"]);
    assert_eq!(outcome.verdict, Verdict::NoChange);
    assert_eq!(
        persisted_attempt(&harness.store),
        (AttemptState::Reconciled, AttemptOutcome::Crashed)
    );
    // §4.8: "attempt failed to act → repair or review".
    assert_eq!(persisted_run_state(&harness.store), RunState::Repairing);
}

#[test]
fn crash_after_edits_keeps_the_work_and_reconciles_without_a_report() {
    // Acceptance row 3: `CRASHED`, `CLEAN_NO_REPORT` — the work survives the
    // agent, because the work is in the repository and the repository is the
    // authority.
    let (harness, outcome) = run_scenario("crash-after-edits", &["src/**"]);
    assert_eq!(outcome.verdict, Verdict::CleanNoReport);
    assert_eq!(
        persisted_attempt(&harness.store),
        (AttemptState::Reconciled, AttemptOutcome::Crashed)
    );
    assert_eq!(persisted_run_state(&harness.store), RunState::Verifying);

    let workspace = harness.config.workspaces_root.join("r-0041");
    assert!(
        workspace.join("src/added.rs").exists(),
        "the crash must not have cost the edit"
    );
}

#[test]
fn a_stall_is_timed_out_with_reason_stall() {
    let (harness, _) = run_scenario_with("stall", &["src/**"], TIGHT_TIMERS);
    assert_eq!(
        persisted_attempt(&harness.store),
        (AttemptState::Reconciled, AttemptOutcome::TimedOut)
    );
    let reason: String = harness
        .store
        .conn()
        .query_row(
            "SELECT payload FROM event WHERE kind='ATTEMPT_FINISHED'",
            [],
            |row| row.get(0),
        )
        .expect("the finished event");
    assert!(
        reason.contains("\"stall\""),
        "§6.4 records reason=stall: {reason}"
    );
}

#[test]
fn a_wall_clock_overrun_is_timed_out_for_a_different_reason() {
    let (harness, _) = run_scenario_with("timeout", &["src/**"], TIGHT_TIMERS);
    assert_eq!(
        persisted_attempt(&harness.store),
        (AttemptState::Reconciled, AttemptOutcome::TimedOut)
    );
    let reason: String = harness
        .store
        .conn()
        .query_row(
            "SELECT payload FROM event WHERE kind='ATTEMPT_FINISHED'",
            [],
            |row| row.get(0),
        )
        .expect("the finished event");
    assert!(
        reason.contains("wall_clock"),
        "the two timers must be distinguishable in the record: {reason}"
    );
}

#[test]
fn a_malformed_report_raises_a_finding_and_the_run_still_advances() {
    // Acceptance row 5: `EXITED`, finding `REPORT_UNPARSEABLE`, verification
    // decides, **finding stays**.
    let (harness, outcome) = run_scenario("malformed-report", &["src/**"]);
    assert_eq!(
        persisted_attempt(&harness.store),
        (AttemptState::Reconciled, AttemptOutcome::Exited)
    );
    assert_eq!(outcome.verdict, Verdict::CleanNoReport);
    assert_eq!(persisted_run_state(&harness.store), RunState::Verifying);

    let findings = harness
        .store
        .findings_for_run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("findings");
    assert!(
        findings.iter().any(|f| f.kind == "REPORT_UNPARSEABLE"),
        "row 5 requires the finding: {findings:?}"
    );
    assert!(
        findings.iter().all(|f| f.resolution.is_none()),
        "§4.8: findings never auto-resolve"
    );
}

#[test]
fn a_missing_report_is_not_an_error() {
    // Acceptance row 4: "report is not required for correctness".
    let (harness, outcome) = run_scenario("missing-report", &["src/**"]);
    assert_eq!(outcome.verdict, Verdict::CleanNoReport);
    assert_eq!(
        persisted_attempt(&harness.store),
        (AttemptState::Reconciled, AttemptOutcome::Exited)
    );
    assert_eq!(persisted_run_state(&harness.store), RunState::Verifying);
    let findings = harness
        .store
        .findings_for_run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("findings");
    assert!(
        !findings.iter().any(|f| f.kind == "REPORT_UNPARSEABLE"),
        "a report that was never written is not an unparseable one"
    );
}

#[test]
fn work_that_will_fail_verification_still_reconciles_clean() {
    // Acceptance row 7. Reconciliation classifies what the *repository* shows;
    // whether the code works is a question only S4's runner can answer, and
    // conflating the two would let a reconciler start judging code.
    let (harness, outcome) = run_scenario("verification-failure", &["src/**"]);
    assert_eq!(outcome.verdict, Verdict::CleanComplete);
    assert_eq!(persisted_run_state(&harness.store), RunState::Verifying);
}

#[test]
fn the_same_failure_twice_produces_the_same_evidence() {
    // Acceptance row 9 turns on two attempts being *recognisably* identical.
    // Stopping after the second is S6's; producing a stable fingerprint is S3's,
    // and without it row 9 cannot be implemented at all.
    warm_the_binary();
    let mut harness = setup(&["src/**"]);
    let scenario = scenario_file(harness._dir.path(), "same-failure-repeatedly");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario).with_max_lifetime_ms(20_000);

    let mut summaries = Vec::new();
    for attempt in 0..2 {
        let claimed = harness
            .store
            .claim_next_run("worker-1", NOW + attempt, LEASE_MS)
            .expect("claim")
            .expect("a run");
        let outcome = run_one_attempt(
            &mut harness.store,
            &claimed.fence(),
            &adapter,
            &harness.config,
            &mut (),
        )
        .expect("attempt");
        summaries.push((outcome.verdict, outcome.route));

        // Re-arm: the run went to REPAIRING, which is claimable only via
        // RECONCILING, so put it back the way a repair loop (S6) would.
        harness
            .store
            .conn()
            .execute("UPDATE run SET state='READY' WHERE id='r-0041'", [])
            .expect("re-arm");
    }

    assert_eq!(
        summaries[0], summaries[1],
        "two runs of a deterministic failure must be indistinguishable"
    );

    let attempts = harness
        .store
        .attempts_for_run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("attempts");
    assert_eq!(attempts.len(), 2);
    assert!(
        attempts
            .iter()
            .all(|a| a.outcome == Some(AttemptOutcome::Crashed)),
        "the scenario exits 1, which §6.4 classifies as CRASHED"
    );
    assert_eq!(attempts[0].ordinal, 1);
    assert_eq!(attempts[1].ordinal, 2);
}

#[test]
fn an_unexpected_dependency_change_is_policy_sensitive() {
    // Acceptance row 13. The scope deliberately includes the manifest, so the
    // verdict cannot come from the path merely being out of scope.
    let (harness, outcome) =
        run_scenario("unexpected-dependency-change", &["src/**", "Cargo.toml"]);
    assert_eq!(outcome.verdict, Verdict::PolicySensitive);
    assert_eq!(
        persisted_run_state(&harness.store),
        RunState::AwaitingApproval
    );
    let findings = harness
        .store
        .findings_for_run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("findings");
    assert!(findings.iter().any(|f| f.kind.contains("POLICYSENSITIVE")));
}

#[test]
fn a_forbidden_git_change_is_caught_even_though_the_tree_is_unchanged() {
    // Acceptance row 14, and the reason §4.8 gives verdict precedence: setting a
    // remote leaves the tree byte-identical to baseline, so a tree-first
    // classifier reports NO_CHANGE and advances.
    let (harness, outcome) = run_scenario("forbidden-git-change", &["src/**"]);
    assert_eq!(
        outcome.verdict,
        Verdict::PolicySensitive,
        "a repository-structure change must outrank NO_CHANGE"
    );
    assert_eq!(
        persisted_run_state(&harness.store),
        RunState::AwaitingApproval
    );

    let findings = harness
        .store
        .findings_for_run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("findings");
    assert!(
        findings.iter().any(|f| f.kind.contains("REMOTE")),
        "the remote change must be named in a finding: {findings:?}"
    );

    // …and the operator's repository is untouched, which is the containment
    // half of row 14.
    let source_remotes = std::process::Command::new("git")
        .args(["remote", "-v"])
        .current_dir(&harness.config.source_repo)
        .output()
        .expect("git remote");
    assert!(
        String::from_utf8_lossy(&source_remotes.stdout)
            .trim()
            .is_empty(),
        "the source repository must not have gained a remote"
    );
}

#[test]
fn a_duplicate_attempt_cannot_claim_the_same_artifact_directory() {
    // Two supervisors that both believe they are attempt 1. The second is
    // refused by the kernel, not by a check-then-write.
    warm_the_binary();
    let harness = setup(&["src/**"]);
    let artifacts = conductor_run::ArtifactRoot::new(&harness.config.artifacts_root);
    let run = conductor_core::RunId::new("r-0041").expect("id");

    let first = artifacts
        .claim_attempt_dir(&run, 1, &conductor_run::Owner::new("worker-1", 1))
        .expect("the first supervisor claims it");
    let err = artifacts
        .claim_attempt_dir(&run, 1, &conductor_run::Owner::new("worker-2", 2))
        .expect_err("the duplicate must be refused");
    assert!(matches!(
        err,
        conductor_run::OwnershipError::AlreadyOwned { .. }
    ));
    assert!(first.path().exists());
}

#[test]
fn a_workspace_cloned_but_never_recorded_does_not_strand_the_run() {
    // The gap between `git clone` returning and the store being told about it is
    // a real crash window, and it is the one window whose durable evidence lives
    // only on disk: the clone is there, complete and carrying this run's
    // descriptor, and `run.workspace_path` is still NULL.
    //
    // §4.7 requires a restart to converge "with no human input". A next attempt
    // that refuses because the directory it is about to create already exists
    // does not converge — it strands the run for good, because every subsequent
    // attempt derives the same path and hits the same refusal.
    warm_the_binary();
    let mut harness = setup(&["src/**"]);
    let scenario = scenario_file(harness._dir.path(), "success");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario).with_max_lifetime_ms(20_000);

    // Exactly what the dead worker left behind: a real clone, made the same way
    // the worker makes it, and a store that never heard about it.
    common::agent::workspace(
        &harness.config.source_repo,
        &harness.config.workspaces_root,
        "r-0041",
    );
    assert!(
        harness
            .store
            .run(&conductor_core::RunId::new("r-0041").expect("id"))
            .expect("run")
            .expect("a row")
            .workspace_path
            .is_none(),
        "the fixture must reproduce the crash window, not a recorded workspace"
    );

    let claimed = harness
        .store
        .claim_next_run("worker-1", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");

    let outcome = run_one_attempt(
        &mut harness.store,
        &claimed.fence(),
        &adapter,
        &harness.config,
        &mut (),
    )
    .expect("a re-attempt must not be stranded by the workspace its predecessor left behind");

    assert_eq!(outcome.verdict, Verdict::CleanComplete);
    assert_eq!(persisted_run_state(&harness.store), RunState::Verifying);
    // …and the recovered workspace is now recorded, so the next crash does not
    // land in the same window again.
    assert!(
        harness
            .store
            .run(&conductor_core::RunId::new("r-0041").expect("id"))
            .expect("run")
            .expect("a row")
            .workspace_path
            .is_some()
    );
}

#[test]
fn an_attempt_to_reach_the_control_socket_raises_a_critical_finding() {
    // Acceptance row 28. The launcher here is `none`, so the connection
    // succeeds — §4.9 layer 4/5 are absent — and Conductor's only move is to
    // notice, which is the honest outcome the row describes for the unsandboxed
    // case.
    warm_the_binary();
    let mut harness = setup(&["src/**"]);
    let scenario = scenario_file(harness._dir.path(), "control-socket");

    // A real listener, so a successful connect is a real connect.
    let socket_path = harness._dir.path().join("conductor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
    // Non-blocking with a deadline. A blocking `accept()` here would turn "the
    // agent never connected" — the exact regression this test exists to catch —
    // into a hung suite rather than a failed assertion.
    listener.set_nonblocking(true).expect("nonblocking");
    let accepted = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => return true,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return false,
            }
        }
        false
    });

    let adapter = FakeAgent::new(fake_agent_binary(), scenario).with_max_lifetime_ms(20_000);
    let claimed = harness
        .store
        .claim_next_run("worker-1", NOW, LEASE_MS)
        .expect("claim")
        .expect("a run");

    // The agent is told the path explicitly. That is not a weakening of §4.9's
    // allowlist — it is what M10 measured: "path was known in every case". Row
    // 28 is about what happens when an agent that knows the path reaches for
    // it, and an agent that cannot find the socket tests nothing.
    let mut config = harness.config.clone();
    config.agent_env_extra.insert(
        "CONDUCTOR_SOCK".to_string(),
        socket_path.display().to_string(),
    );

    let _outcome = run_one_attempt(
        &mut harness.store,
        &claimed.fence(),
        &adapter,
        &config,
        &mut (),
    )
    .expect("the attempt ran");
    assert!(
        accepted.join().expect("the accept thread"),
        "the agent did not actually reach the socket, so nothing was tested"
    );

    let findings = harness
        .store
        .findings_for_run(&conductor_core::RunId::new("r-0041").expect("id"))
        .expect("findings");
    let socket_finding = findings
        .iter()
        .find(|f| f.kind == "CONTROL_SOCKET_ATTEMPT")
        .expect("row 28 requires the attempt to be recorded");
    assert_eq!(socket_finding.severity, "CRITICAL");
    assert!(
        socket_finding.resolution.is_none(),
        "findings never auto-resolve"
    );

    // Row 28: "no grant created."
    let grants: i64 = harness
        .store
        .conn()
        .query_row("SELECT COUNT(*) FROM approval_grant", [], |row| row.get(0))
        .expect("count grants");
    assert_eq!(grants, 0);
}
