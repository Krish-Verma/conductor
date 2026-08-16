//! Startup recovery — §4.7's nine steps, one at a time.
//!
//! `crash_matrix.rs` proves recovery converges after a real kill. This file
//! pins the individual steps, including the ones a crash is an awkward way to
//! reach: a recycled pid, an orphan workspace, an approval whose TTL lapsed
//! while the host was down.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use conductor_core::{AttemptOutcome, AttemptState, RunId, RunState};
use conductor_git::{Scope, SensitivePatterns};
use conductor_run::recovery::{RecoveryConfig, RecoveryDecision, recover};
use conductor_store::{NewAttempt, Store};

const RUN: &str = "r-0041";
const NOW: i64 = 1_770_000_000_000;
const LEASE_MS: i64 = 60_000;

struct World {
    dir: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = common::agent::source_repo(dir.path());
        let mut store = Store::open_or_create(dir.path().join("conductor.db")).expect("store");
        seed(&mut store, &common::agent::head(&source));
        World { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn store(&self) -> Store {
        Store::open_or_create(self.dir.path().join("conductor.db")).expect("store")
    }

    fn config(&self) -> RecoveryConfig {
        RecoveryConfig {
            worker_id: "worker-recovered".to_string(),
            workspaces_root: self.dir.path().join("workspaces"),
            quarantine_root: self.dir.path().join("quarantine"),
            artifacts_root: self.dir.path().join("artifacts"),
            adopt_live_agents: false,
            lease_ms: LEASE_MS,
            scope: Scope::new(["src/**".to_string()]),
            sensitive: SensitivePatterns::default(),
        }
    }
}

fn seed(store: &mut Store, base_commit: &str) {
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
                              priority, lease_owner, lease_expires_at, lease_epoch, created_at)
             VALUES ('r-0041', 'T-0012', ?1, ?2, 'conductor/T-0012/r-0041', 'RUNNING',
                     100, 'worker-dead', ?3, 1, 0)",
            rusqlite::params![common::agent::POLICY_HASH, base_commit, NOW - 1],
        )?;
        Ok(())
    })
    .expect("seed");
}

/// Take the seeded run the way reality does: sweep the lapsed lease first.
///
/// A `RUNNING` run is deliberately **not** claimable — §4.7's predicate is
/// `READY`/`RECONCILING` — so a test that wants to plant an attempt on one has
/// to go through the sweep exactly as a restarting worker would.
fn claim_for_setup(store: &mut Store) -> conductor_core::Fence {
    store.expire_leases(NOW).expect("sweep the lapsed lease");
    store
        .claim_run(&RunId::new(RUN).expect("id"), "seed", NOW, LEASE_MS)
        .expect("claim")
        .expect("a swept run is claimable")
        .fence()
}

fn run_state(store: &Store) -> RunState {
    store
        .run(&RunId::new(RUN).expect("id"))
        .expect("run")
        .expect("a row")
        .state
}

/// Plant one `ACTIVE` attempt on the seeded run — what a supervisor that died
/// mid-run leaves behind for §4.7 step 3 to probe.
fn plant_active_attempt(store: &mut Store, pid: i32, pid_start_time: Option<i64>) {
    let fence = claim_for_setup(store);
    let attempt = store
        .create_attempt(
            &fence,
            NewAttempt {
                id: conductor_core::AttemptId::new("a-1").expect("id"),
                ordinal: 1,
                kind: "IMPLEMENT".to_string(),
                adapter: "fake".to_string(),
                launcher: "none".to_string(),
                caps_snapshot: "{}".to_string(),
                agent_session_id: None,
            },
            NOW,
        )
        .expect("create")
        .starting()
        .active(pid, pid_start_time);
    store
        .record_attempt_active(&fence, &attempt, NOW)
        .expect("active");
}

/// Erase a recorded start time the way a crash does — by writing the row, not
/// by calling a constructor.
///
/// Deliberately raw SQL: the point of the test that uses it is a row Conductor
/// has to survive *reading*, whoever wrote it, including a build older than this
/// one. Going through the typestate would only prove the typestate agrees with
/// itself.
fn forget_recorded_start_time(store: &mut Store, attempt_id: &str) {
    store
        .conn_mut()
        .execute(
            "UPDATE attempt SET pid_start_time = NULL WHERE id = ?1",
            rusqlite::params![attempt_id],
        )
        .expect("forget the start time");
}

/// The recorded start time as it sits in the database.
fn recorded_start_time(store: &Store, attempt_id: &str) -> Option<i64> {
    store
        .conn()
        .query_row(
            "SELECT pid_start_time FROM attempt WHERE id = ?1",
            rusqlite::params![attempt_id],
            |row| row.get(0),
        )
        .expect("row")
}

// ---------------------------------------------------------------------------

#[test]
fn step_2_forces_every_lease_bearing_state_to_reconciling() {
    let world = World::new();
    let mut store = world.store();
    let report = recover(&mut store, &world.config(), NOW).expect("recover");

    assert_eq!(report.expired_leases.len(), 1);
    assert_eq!(report.expired_leases[0].run_id.as_str(), RUN);
    assert_eq!(report.expired_leases[0].previous_state, RunState::Running);
    assert_eq!(
        report.expired_leases[0].previous_owner.as_deref(),
        Some("worker-dead")
    );
    // §5.2's restart rule, and the fencing epoch moved with it.
    assert!(report.expired_leases[0].lease_epoch > 1);
}

#[test]
fn step_1_reports_integrity_and_schema_version() {
    let world = World::new();
    let mut store = world.store();
    let report = recover(&mut store, &world.config(), NOW).expect("recover");
    assert_eq!(report.integrity_check, vec!["ok".to_string()]);
    assert_eq!(
        report.schema_version,
        Some(conductor_store::schema::SUPPORTED_SCHEMA_VERSION)
    );
}

#[test]
fn step_3_records_a_dead_pid_as_stale_never_as_crashed() {
    let world = World::new();
    let mut store = world.store();

    // An attempt that was ACTIVE with a pid that is now gone. 999999 is not a
    // live process on this host, and the probe says so rather than guessing.
    let fence = claim_for_setup(&mut store);
    let attempt = store
        .create_attempt(
            &fence,
            NewAttempt {
                id: conductor_core::AttemptId::new("a-1").expect("id"),
                ordinal: 1,
                kind: "IMPLEMENT".to_string(),
                adapter: "fake".to_string(),
                launcher: "none".to_string(),
                caps_snapshot: "{}".to_string(),
                agent_session_id: None,
            },
            NOW,
        )
        .expect("create")
        .starting()
        .active(999_999, Some(12_345));
    store
        .record_attempt_active(&fence, &attempt, NOW)
        .expect("active");

    let report = recover(&mut store, &world.config(), NOW + LEASE_MS * 2).expect("recover");

    let attempts = store
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts");
    assert_eq!(attempts[0].state, AttemptState::Reconciled);
    assert_eq!(
        attempts[0].outcome,
        Some(AttemptOutcome::Stale),
        "no exit was observed, so nothing may be recorded as an exit"
    );
    assert_eq!(attempts[0].exit_code, None);
    assert_eq!(attempts[0].signal, None);
    assert!(
        report
            .decisions
            .iter()
            .any(|d| matches!(d, RecoveryDecision::AttemptStale { .. })),
        "{:?}",
        report.decisions
    );
}

#[test]
fn step_3_refuses_to_adopt_a_recycled_pid() {
    // §4.7 step 3: "alive **and** start-time matches (a recycled pid is not your
    // process)". Here the pid is unambiguously alive — it is this test process
    // — but the recorded start time is somebody else's.
    let world = World::new();
    let mut store = world.store();

    let fence = claim_for_setup(&mut store);
    let me = std::process::id() as i32;
    let attempt = store
        .create_attempt(
            &fence,
            NewAttempt {
                id: conductor_core::AttemptId::new("a-1").expect("id"),
                ordinal: 1,
                kind: "IMPLEMENT".to_string(),
                adapter: "fake".to_string(),
                launcher: "none".to_string(),
                caps_snapshot: "{}".to_string(),
                agent_session_id: None,
            },
            NOW,
        )
        .expect("create")
        .starting()
        // A start time that is certainly not ours.
        .active(me, Some(1));
    store
        .record_attempt_active(&fence, &attempt, NOW)
        .expect("active");

    let report = recover(&mut store, &world.config(), NOW + LEASE_MS * 2).expect("recover");

    let decision = report
        .decisions
        .iter()
        .find(|d| matches!(d, RecoveryDecision::AttemptStale { .. }))
        .expect("a recycled pid must be STALE, never adopted or terminated");
    match decision {
        RecoveryDecision::AttemptStale { reason, .. } => assert!(
            reason.contains("different process"),
            "the reason must say why: {reason}"
        ),
        other => panic!("unexpected decision {other:?}"),
    }
    assert!(
        !report.decisions.iter().any(|d| matches!(
            d,
            RecoveryDecision::TerminatedLiveAgent { .. }
                | RecoveryDecision::AdoptedLiveAgent { .. }
        )),
        "recovery must not touch a process it does not own: {:?}",
        report.decisions
    );

    // The proof it did not act on it: this process is still here.
    assert!(conductor_run::supervise::start_time_us(me).is_some());
}

#[test]
fn step_3_refuses_to_adopt_a_pid_whose_recorded_start_time_is_absent() {
    // §4.7 step 3 asks "alive **and** start-time matches?". With no recorded
    // start time the second half of that question has no answer, and a question
    // that cannot be answered must not be answered in the affirmative: adopting
    // a stranger's process is worse than adopting nothing. The pid here is
    // unambiguously alive — it is this test process — which is exactly the case
    // a fail-open would swallow.
    let world = World::new();
    let mut store = world.store();
    let me = std::process::id() as i32;
    let real = conductor_run::supervise::start_time_us(me).expect("our own start time");

    plant_active_attempt(&mut store, me, Some(real));
    forget_recorded_start_time(&mut store, "a-1");

    let mut config = world.config();
    // Adoption *enabled*, so a refusal here is the identity check refusing and
    // not merely the conservative default declining.
    config.adopt_live_agents = true;
    let report = recover(&mut store, &config, NOW + LEASE_MS * 2).expect("recover");

    assert!(
        !report.decisions.iter().any(|d| matches!(
            d,
            RecoveryDecision::AdoptedLiveAgent { .. }
                | RecoveryDecision::TerminatedLiveAgent { .. }
        )),
        "an unidentifiable pid must be neither adopted nor killed: {:?}",
        report.decisions
    );
    let decision = report
        .decisions
        .iter()
        .find(|d| matches!(d, RecoveryDecision::AttemptStale { .. }))
        .expect("an attempt with no recorded identity is STALE");
    match decision {
        RecoveryDecision::AttemptStale { reason, .. } => {
            assert!(
                reason.contains(&me.to_string()),
                "the reason must name the pid it refused: {reason}"
            );
            assert!(
                reason.contains("start time"),
                "the reason must say the identity was missing, not that the pid was: {reason}"
            );
        }
        other => panic!("unexpected decision {other:?}"),
    }

    let attempts = store
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts");
    assert_eq!(attempts[0].outcome, Some(AttemptOutcome::Stale));
    assert_eq!(
        attempts[0].pid,
        Some(me),
        "the pid is still evidence even when the identity is not established"
    );
    assert_eq!(
        recorded_start_time(&store, "a-1"),
        None,
        "recovery must not invent the start time it could not read"
    );

    // The proof it did not act on it: this process is still here.
    assert!(conductor_run::supervise::start_time_us(me).is_some());
}

#[test]
fn step_3_refuses_to_adopt_a_pid_whose_recorded_start_time_is_the_zero_sentinel() {
    // A stored `0` is not a start time — no process began at the Unix epoch. It
    // is what a build that collapsed "the start time could not be read" into a
    // sentinel wrote, and those rows are still in databases. Believing one makes
    // §4.7 step 3 adopt whatever now happens to hold the pid, which is the
    // fail-open this test exists to close.
    let world = World::new();
    let mut store = world.store();
    let me = std::process::id() as i32;

    plant_active_attempt(&mut store, me, Some(0));

    let mut config = world.config();
    config.adopt_live_agents = true;
    let report = recover(&mut store, &config, NOW + LEASE_MS * 2).expect("recover");

    assert!(
        !report.decisions.iter().any(|d| matches!(
            d,
            RecoveryDecision::AdoptedLiveAgent { .. }
                | RecoveryDecision::TerminatedLiveAgent { .. }
        )),
        "a zero start time is an absent identity, not a wildcard: {:?}",
        report.decisions
    );
    let decision = report
        .decisions
        .iter()
        .find(|d| matches!(d, RecoveryDecision::AttemptStale { .. }))
        .expect("a zero start time is STALE, never adopted");
    match decision {
        RecoveryDecision::AttemptStale { reason, .. } => assert!(
            reason.contains("no start time"),
            "a `0` is an absent identity, not evidence that some *other* process \
             took the pid — saying so would send an operator hunting a recycled \
             pid that never existed: {reason}"
        ),
        other => panic!("unexpected decision {other:?}"),
    }

    // The proof it did not act on it: this process is still here.
    assert!(conductor_run::supervise::start_time_us(me).is_some());
}

#[test]
fn step_3_still_adopts_an_agent_whose_recorded_identity_matches() {
    // The positive control for the two refusals above. §4.7 step 3's "alive →
    // adopt or terminate (config)" has to stay reachable: a recovery that
    // refuses everything is not a stricter identity check, it is a broken one.
    // This control cannot fail before the fix — it pins behaviour the fix must
    // preserve, not behaviour it introduces.
    let world = World::new();
    let mut store = world.store();
    let me = std::process::id() as i32;
    let real = conductor_run::supervise::start_time_us(me).expect("our own start time");

    plant_active_attempt(&mut store, me, Some(real));

    let mut config = world.config();
    config.adopt_live_agents = true;
    let report = recover(&mut store, &config, NOW + LEASE_MS * 2).expect("recover");

    assert!(
        report
            .decisions
            .iter()
            .any(|d| matches!(d, RecoveryDecision::AdoptedLiveAgent { pid, .. } if *pid == me)),
        "a matching identity is still adoptable: {:?}",
        report.decisions
    );
}

#[test]
fn step_8_quarantines_orphan_workspaces_and_never_deletes_them() {
    // Acceptance row 18 and §4.1: "an orphan may hold the only copy of an hour
    // of work."
    let world = World::new();
    let workspaces = world.path().join("workspaces");
    std::fs::create_dir_all(workspaces.join("r-9999")).expect("mkdir");
    std::fs::write(
        workspaces.join("r-9999").join("precious.txt"),
        b"an hour of work",
    )
    .expect("write");

    let mut store = world.store();
    let report = recover(&mut store, &world.config(), NOW).expect("recover");

    assert_eq!(report.quarantined.len(), 1);
    let moved = Path::new(&report.quarantined[0]);
    assert!(moved.exists(), "the orphan must still exist");
    assert_eq!(
        std::fs::read_to_string(moved.join("precious.txt")).expect("read"),
        "an hour of work",
        "quarantine moves, it never deletes"
    );
    assert!(
        !workspaces.join("r-9999").exists(),
        "the orphan must have been moved out of the active root"
    );
}

#[test]
fn step_8_leaves_a_live_runs_workspace_alone() {
    let world = World::new();
    let workspaces = world.path().join("workspaces");
    std::fs::create_dir_all(&workspaces).expect("mkdir");
    // A workspace belonging to the active run, with a descriptor naming it.
    let source = world.path().join("source");
    let _ = common::agent::workspace(&source, &workspaces, RUN);

    let mut store = world.store();
    let report = recover(&mut store, &world.config(), NOW).expect("recover");

    assert!(
        report.quarantined.is_empty(),
        "an active run's workspace is not an orphan: {:?}",
        report.quarantined
    );
    assert!(workspaces.join(RUN).exists());
}

#[test]
fn step_9_expires_overdue_approvals_and_restores_the_rest() {
    let world = World::new();
    let mut store = world.store();
    store
        .conn()
        .execute(
            // `kind` is schema v6's (S8). The third row is a plan approval,
            // which §4.3 says does not expire: its `expires_at` is NULL, and
            // `NULL < now` is NULL in SQLite, so it is never selected. That
            // behaviour is load-bearing and invisible — nothing about the sweep
            // announces it — so step 9 is where it gets asserted.
            "INSERT INTO approval_request
               (id, kind, run_id, action, facts, facts_source, policy_hash, matched_rules,
                explanation, evidence_ref, state, requested_at, expires_at)
             VALUES ('ap-overdue', 'POLICY_APPROVAL', 'r-0041', 'dependency.add.runtime',
                     '{}', 'reconciliation', ?1, '[]', 'x', NULL, 'REQUESTED', 0, ?2),
                    ('ap-live', 'POLICY_APPROVAL', 'r-0041', 'dependency.add.runtime',
                     '{}', 'reconciliation', ?1, '[]', 'x', NULL, 'REQUESTED', 0, ?3),
                    ('ap-perpetual', 'PLAN_APPROVAL', NULL, 'architecture.change',
                     '{}', 'reconciliation', ?1, '[]', 'x', NULL, 'REQUESTED', 0, NULL)",
            rusqlite::params![common::agent::POLICY_HASH, NOW - 1, NOW + 3_600_000],
        )
        .expect("seed approvals");

    let report = recover(&mut store, &world.config(), NOW).expect("recover");

    assert_eq!(report.expired_approvals, vec!["ap-overdue".to_string()]);
    // Both the live one and the one that never expires are restored. A sweep
    // that treated a NULL TTL as "already past" would silently expire every
    // plan approval on the next daemon start.
    assert_eq!(
        report.restored_waits,
        vec!["ap-live".to_string(), "ap-perpetual".to_string()]
    );

    let states: Vec<(String, String)> = {
        let mut stmt = store
            .conn()
            .prepare("SELECT id, state FROM approval_request ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    };
    assert_eq!(
        states,
        vec![
            ("ap-live".to_string(), "REQUESTED".to_string()),
            ("ap-overdue".to_string(), "EXPIRED".to_string()),
            ("ap-perpetual".to_string(), "REQUESTED".to_string()),
        ],
        "a live approval's TTL must survive the restart (row 12)"
    );
}

#[test]
fn a_run_with_no_workspace_goes_back_to_the_queue_rather_than_blocking() {
    // Nothing was created, so nothing is at risk. Blocking here would require a
    // human to unstick a run that never started.
    let world = World::new();
    let mut store = world.store();
    let report = recover(&mut store, &world.config(), NOW).expect("recover");

    assert_eq!(run_state(&store), RunState::Repairing);
    assert!(
        report.decisions.iter().any(|d| matches!(
            d,
            RecoveryDecision::Reconciled {
                route: conductor_core::ReconciledRoute::Repairing,
                ..
            }
        )),
        "{:?}",
        report.decisions
    );
}

#[test]
fn recovery_leaves_a_run_another_worker_still_holds_alone() {
    // The complement of row 27: fencing is not only about rejecting a stale
    // worker's writes, it is about a *recovering* worker not stealing a live
    // one's run.
    let world = World::new();
    let mut store = world.store();
    store
        .conn()
        .execute(
            "UPDATE run SET state='RECONCILING', lease_owner='worker-live',
                            lease_expires_at=?1 WHERE id=?2",
            rusqlite::params![NOW + 30_000, RUN],
        )
        .expect("give it a live lease");

    let report = recover(&mut store, &world.config(), NOW).expect("recover");

    assert!(
        report.expired_leases.is_empty(),
        "a live lease must not be swept"
    );
    assert!(
        report
            .decisions
            .iter()
            .any(|d| matches!(d, RecoveryDecision::NotClaimable { .. })),
        "{:?}",
        report.decisions
    );
    let owner: String = store
        .conn()
        .query_row("SELECT lease_owner FROM run WHERE id=?1", [RUN], |r| {
            r.get(0)
        })
        .expect("owner");
    assert_eq!(owner, "worker-live");
}

#[test]
fn step_6_reports_whether_verification_is_still_owed() {
    // §4.7: "Re-run verification only if the tree hash has no cached valid
    // result." S3 asks; S4 runs. Asking is what decides whether the run can
    // advance, so it cannot wait for S4.
    let world = World::new();
    let workspaces = world.path().join("workspaces");
    std::fs::create_dir_all(&workspaces).expect("mkdir");
    let source = world.path().join("source");
    let ws = common::agent::workspace(&source, &workspaces, RUN);

    let mut store = world.store();
    // Attach the workspace and stash the baseline where recovery looks for it.
    let fence = claim_for_setup(&mut store);
    store
        .attach_workspace(
            &fence,
            "ws-r-0041",
            &ws.path.display().to_string(),
            &source.display().to_string(),
            NOW,
        )
        .expect("attach");
    let artifacts = conductor_run::ArtifactRoot::new(world.path().join("artifacts"));
    let owned = artifacts
        .claim_attempt_dir(
            &RunId::new(RUN).expect("id"),
            1,
            &conductor_run::Owner::new("seed", 1),
        )
        .expect("claim dir");
    owned
        .write_new(
            "baseline.json",
            &serde_json::to_vec(&ws.baseline).expect("serialize"),
        )
        .expect("write baseline");

    let report = recover(&mut store, &world.config(), NOW + LEASE_MS * 2).expect("recover");

    let decision = report
        .decisions
        .iter()
        .find_map(|d| match d {
            RecoveryDecision::Reconciled {
                verification_needed,
                ..
            } => Some(*verification_needed),
            _ => None,
        })
        .expect("the run must have been reconciled");
    assert!(
        decision,
        "nothing has verified this tree, so verification is owed"
    );

    // Record a cached PASS at that tree and ask again.
    store
        .conn()
        .execute(
            "INSERT INTO verification_check
               (id, run_id, tree_hash, commit_sha, toolchain_fingerprint, check_id,
                command_hash, exit_code, duration_ms, outcome)
             VALUES ('v-1', ?1, ?2, 'abc', 'tc', 'typecheck', 'ch', 0, 1, 'PASS')",
            rusqlite::params![RUN, ws.baseline.tree_hash],
        )
        .expect("cache a result");
    assert!(
        store
            .has_valid_verification(&ws.baseline.tree_hash)
            .expect("query"),
        "a cached PASS at this tree must be visible to recovery"
    );
}

#[test]
fn recovery_is_idempotent() {
    let world = World::new();
    let mut store = world.store();

    let first = recover(&mut store, &world.config(), NOW).expect("first");
    let state_after_first = run_state(&store);
    let second = recover(&mut store, &world.config(), NOW + 1).expect("second");

    assert_eq!(
        run_state(&store),
        state_after_first,
        "a second recovery pass must not move a run that is already settled"
    );
    assert!(
        second.expired_leases.is_empty(),
        "nothing was left holding a lapsed lease: {:?}",
        second.expired_leases
    );
    assert_eq!(first.integrity_check, second.integrity_check);
}

#[test]
fn active_runs_are_what_orphan_detection_compares_against() {
    // A guard against the quietest possible bug in step 8: if `active_runs`
    // ever stopped returning a live run, its workspace would be quarantined out
    // from under it.
    let world = World::new();
    let store = world.store();
    let active: BTreeSet<String> = store
        .active_runs()
        .expect("active")
        .into_iter()
        .map(|r| r.id.as_str().to_string())
        .collect();
    assert!(active.contains(RUN));

    store
        .conn()
        .execute("UPDATE run SET state='COMPLETE' WHERE id=?1", [RUN])
        .expect("complete it");
    let active: BTreeSet<String> = store
        .active_runs()
        .expect("active")
        .into_iter()
        .map(|r| r.id.as_str().to_string())
        .collect();
    assert!(!active.contains(RUN), "a terminal run is not active");
}
