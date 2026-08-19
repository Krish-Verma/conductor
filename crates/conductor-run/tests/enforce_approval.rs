//! S9 — acceptance rows 13, 12 and 25, wired to a real run.
//!
//! | # | Scenario | Expected persisted state | Final |
//! |---|---|---|---|
//! | 13 | Dependency policy violation | `POLICY_SENSITIVE`, request created | `AWAITING_APPROVAL` |
//! | 12 | Crash during approval wait | request `REQUESTED`, TTL preserved | resumes on grant |
//! | 25 | Approval revoked mid-effect | grant `REVOKED`, effect recorded | `AWAITING_REVIEW` |
//!
//! S8 proved the approval *mechanism*: exact scoping, expiry, one-shot
//! consumption, revocation in four states, survival across fifty `SIGKILL`
//! cycles. The master plan was equally explicit about what it had not proved:
//!
//! > What it does **not** do is turn a `require_approval` decision into an
//! > `approval_request` from inside a run: nothing in the run path creates one
//! > … Scoring any of these `PASS` on the strength of the unit coverage would be
//! > exactly the "a similarly named test exists" error the sweep forbids.
//!
//! So nothing here calls `approval::request` to set up the thing under test.
//! The request must be created **by the run**, or the test fails. The only
//! direct approval calls are the ones standing in for a human at the socket —
//! `grant` and `revoke` — because a test cannot type at a socket.
//!
//! # Every gate has both halves
//!
//! Each refusal is paired with the granted case that must succeed. A suite that
//! only proved "the run stops" would pass against a build that stops every run,
//! and a suite that only proved "the grant resumes it" would pass against a
//! build that resumes on any grant at all — including one issued for a
//! different action, a different run, or one already revoked. Both are tested.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::agent::{fake_agent_binary, scenario_file, warm_the_binary};
use common::vertical::{RUN, TASK, World};
use conductor_core::{RunId, RunState, TaskId};
use conductor_run::approval::kind::{Expiry, Subject};
use conductor_run::approval::store::{
    ApprovalRequestRow, GrantOptions, GrantState, RequestState, grant, grant_row, request_row,
};
use conductor_run::policy::model::Action;
use conductor_run::vertical::{VerticalConfig, VerticalOutcome, resume_on_grant, run_task};

/// A policy that gates exactly the action the fixture performs.
const GATING_POLICY: &str = "policy:\n  rules:\n    - {id: p.dep, action: dependency.add.runtime, effect: require_approval}\n";

/// The same policy shape, but allowing the action. The negative control for
/// "does a run create a request when nothing requires one?"
const PERMISSIVE_POLICY: &str =
    "policy:\n  rules:\n    - {id: p.dep, action: dependency.add.runtime, effect: allow}\n";

/// A policy that refuses outright. §4.4's deny is not approvable.
const DENYING_POLICY: &str =
    "policy:\n  rules:\n    - {id: p.dep, action: dependency.add.runtime, effect: deny}\n";

fn now() -> i64 {
    1_800_000_000_000
}

fn run_id() -> RunId {
    RunId::new(RUN).expect("run id")
}

/// Replace the run's pinned policy with a real, decodable snapshot.
///
/// The default fixture seeds `canonical_blob = '{}'`, which cannot decode — a
/// state S9 treats as "undecidable, ask a human". These tests are about what
/// happens when the policy *can* be read, so they install one.
fn pin_policy(world: &World, yaml: &str) -> String {
    use conductor_run::policy::load;
    use conductor_run::policy::model::Origin;

    let document = load::parse_document(yaml, Origin::Global).expect("parse policy");
    let resolved = load::resolve_documents(Some(document), None, None).expect("resolve");
    let snapshot = load::snapshot(&resolved);
    let hash = snapshot.hash.clone();

    let mut store = world.store();
    load::persist(store.conn_mut(), &snapshot, now()).expect("persist snapshot");
    store
        .conn()
        .execute(
            "UPDATE run SET policy_hash = ?1 WHERE id = ?2",
            rusqlite::params![hash, RUN],
        )
        .expect("pin the run to it");
    hash
}

/// Put the dependency manifest inside the task's declared scope.
///
/// Without this the verdict would be `OUT_OF_SCOPE`, which outranks nothing and
/// routes to review — and the test would be proving something about scope
/// rather than about policy.
fn scope_includes_manifest(world: &World) {
    let store = world.store();
    store
        .conn()
        .execute(
            r#"UPDATE task SET scope_globs = '["src/**","Cargo.toml"]' WHERE id = ?1"#,
            rusqlite::params![TASK],
        )
        .expect("widen scope");
}

fn config(world: &World) -> VerticalConfig {
    VerticalConfig {
        task_id: TaskId::new(TASK).expect("task id"),
        worker_id: "w-1".to_string(),
        source_repo: world.source.clone(),
        workspaces_root: world.workspaces(),
        artifacts_root: world.artifacts(),
        quarantine_root: world.quarantine(),
        profile_path: world.profile(),
        scratch_index: world.root().join("scratch").join("index"),
        supervisor: conductor_run::SupervisorConfig {
            startup_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            wall_timeout: Duration::from_secs(120),
            terminate_grace: Duration::from_millis(300),
            poll_interval: Duration::from_millis(10),
        },
        lease_ms: conductor_store::LEASE_MS,
        heartbeat_interval: Duration::from_millis(200),
        startup_grace: Duration::from_secs(30),
        sensitive: Default::default(),
        agent_env_extra: BTreeMap::new(),
        // The fake agent authenticates against nothing.
        credential_home: None,
        probe_key: conductor_run::containment::cache::ProbeKey::new(
            "fake",
            "1.0.0",
            "none",
            "n/a",
            "test-os-1",
        ),
        // The worker derives §6.5's implementation packet — this is not a repair.
        instructions: None,
    }
}

/// Run the dependency-adding fixture to wherever it stops.
fn run_dependency_fixture(world: &World) -> conductor_run::vertical::Vertical {
    warm_the_binary();
    let scenario = scenario_file(&world.root(), "unexpected-dependency-change");
    let adapter = conductor_agent::fake::FakeAgent::new(fake_agent_binary(), scenario)
        .with_max_lifetime_ms(20_000);
    let mut store = world.store();
    run_task(&mut store, &adapter, &config(world), &mut ()).expect("the attempt runs")
}

/// Every approval request the database holds for this run.
fn requests(world: &World) -> Vec<ApprovalRequestRow> {
    let store = world.store();
    let mut stmt = store
        .conn()
        .prepare("SELECT id FROM approval_request WHERE run_id = ?1 ORDER BY id")
        .expect("prepare");
    let ids: Vec<String> = stmt
        .query_map([RUN], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    ids.iter()
        .map(|id| {
            request_row(store.conn(), id)
                .expect("request row")
                .expect("a row")
        })
        .collect()
}

/// Stand in for a human at the control socket.
fn human_grants(world: &World, request_id: &str, reuse: bool) -> String {
    let grant_id = format!("AG-{request_id}");
    let mut store = world.store();
    grant(
        store.conn_mut(),
        request_id,
        &GrantOptions {
            id: grant_id.clone(),
            scope: conductor_run::policy::model::Scope::from_pairs([(
                "run".to_string(),
                RUN.to_string(),
            )]),
            reuse,
            expires: Expiry::At(now() + 60 * 60 * 1000),
            granted_by: "operator".to_string(),
            channel: "socket".to_string(),
            nonce_hash: None,
        },
        now(),
    )
    .expect("grant");
    grant_id
}

// ---------------------------------------------------------------------------
// Row 13 — the request is created by the run
// ---------------------------------------------------------------------------

#[test]
fn a_policy_sensitive_run_creates_a_durable_approval_request() {
    // Acceptance row 13, end to end. Nothing in this test writes an approval
    // request; if the run does not create one, it fails.
    let world = World::new();
    scope_includes_manifest(&world);
    let policy_hash = pin_policy(&world, GATING_POLICY);

    let vertical = run_dependency_fixture(&world);

    assert_eq!(
        vertical.attempt.verdict,
        conductor_git::Verdict::PolicySensitive
    );
    assert_eq!(world.run_state(), RunState::AwaitingApproval);
    assert!(matches!(
        vertical.outcome,
        VerticalOutcome::Stopped {
            state: RunState::AwaitingApproval,
            ..
        }
    ));

    let requests = requests(&world);
    assert_eq!(
        requests.len(),
        1,
        "a run in AWAITING_APPROVAL must have exactly one pending request; \
         found {requests:?}"
    );
    let raised = &requests[0];
    assert_eq!(raised.state, RequestState::Requested);
    assert_eq!(raised.run_id.as_ref().map(|r| r.as_str()), Some(RUN));
    // The subject is the action §4.4 actually resolved, not a boolean.
    assert_eq!(
        raised.subject,
        Subject::PolicyAction {
            action: Action::DependencyAddRuntime
        }
    );
    // Bound to the snapshot the run is pinned to (row 23).
    assert_eq!(raised.policy_hash, policy_hash);
    assert!(
        raised.matched_rules.iter().any(|r| r == "p.dep"),
        "the rule that gated it is not recorded: {:?}",
        raised.matched_rules
    );
    // §4.3 makes a TTL mandatory for the policy kind.
    assert!(matches!(raised.expires, Expiry::At(_)));
}

#[test]
fn a_run_whose_policy_allows_the_change_creates_no_request() {
    // The negative control for row 13. Same fixture, same sensitive path, same
    // verdict — one rule changed. A build that raised a request for every
    // `POLICY_SENSITIVE` verdict would pass the test above and fail this one,
    // and it is the difference between "policy decided" and "something
    // happened".
    let world = World::new();
    scope_includes_manifest(&world);
    pin_policy(&world, PERMISSIVE_POLICY);

    let vertical = run_dependency_fixture(&world);

    assert_eq!(
        vertical.attempt.verdict,
        conductor_git::Verdict::PolicySensitive
    );
    assert!(
        requests(&world).is_empty(),
        "a policy that allows the action must not ask a human about it"
    );
    assert_ne!(
        world.run_state(),
        RunState::AwaitingApproval,
        "an allowed action left the run waiting for an approval nobody needs"
    );
}

#[test]
fn a_denied_action_halts_without_offering_a_button_that_must_not_exist() {
    // §4.4's `deny` is not approvable — that is what distinguishes it from
    // `require_approval` (ADR-0010). Creating a request would offer a human the
    // option of authorising something policy refuses.
    let world = World::new();
    scope_includes_manifest(&world);
    pin_policy(&world, DENYING_POLICY);

    run_dependency_fixture(&world);

    assert_eq!(world.run_state(), RunState::AwaitingReview);
    assert!(
        requests(&world).is_empty(),
        "a deny must not produce an approvable request"
    );
    let findings = world.store().findings_for_run(&run_id()).expect("findings");
    assert!(
        findings.iter().any(|f| f.kind == "POLICY_DENIED"),
        "the denial is not recorded: {findings:?}"
    );
}

// ---------------------------------------------------------------------------
// Row 12 — resume on grant
// ---------------------------------------------------------------------------

#[test]
fn a_granted_request_resumes_the_run_without_a_second_agent_attempt() {
    // Acceptance row 12's "resumes on grant", and the reason the destination is
    // `RECONCILING` rather than `READY`: a second agent attempt would re-capture
    // the baseline from a workspace that already holds the approved change,
    // reconcile it away as `NO_CHANGE`, and the approval would have authorised
    // nothing.
    let world = World::new();
    scope_includes_manifest(&world);
    pin_policy(&world, GATING_POLICY);

    run_dependency_fixture(&world);
    assert_eq!(world.run_state(), RunState::AwaitingApproval);
    let attempts_before = attempts(&world);

    let request = requests(&world).remove(0);
    let grant_id = human_grants(&world, &request.id, false);

    let mut store = world.store();
    let resumed = resume_on_grant(&mut store, &config(&world), now(), &mut ())
        .expect("a granted run must resume");
    drop(store);

    assert!(
        matches!(resumed.outcome, VerticalOutcome::Complete { .. }),
        "the granted run did not complete: {:?}",
        resumed.outcome
    );
    assert_eq!(world.run_state(), RunState::Complete);
    assert_eq!(
        attempts(&world),
        attempts_before,
        "resuming on a grant launched another agent"
    );

    // §4.3: one-shot unless a human said otherwise. The grant is spent.
    let spent = grant_row(world.store().conn(), &grant_id)
        .expect("grant row")
        .expect("a row");
    assert_eq!(
        spent.state,
        GrantState::Consumed,
        "the grant was not consumed, so it could authorise a second run"
    );
    // And the change the human approved is actually in the commit.
    assert!(
        integrated_paths(&world).iter().any(|p| p == "Cargo.toml"),
        "the approved change is not in the integrated commit — the resume path \
         discarded exactly what was authorised"
    );
}

#[test]
fn a_grant_for_a_different_action_does_not_resume_the_run() {
    // The binding is recomputed from the decision in hand and compared; the
    // stored value is never trusted (S8). A grant issued against a request for
    // some other action must not satisfy this run.
    let world = World::new();
    scope_includes_manifest(&world);
    pin_policy(&world, GATING_POLICY);
    run_dependency_fixture(&world);

    // A request for a different action entirely, granted.
    let other = "AR-unrelated".to_string();
    {
        let mut store = world.store();
        conductor_run::approval::store::request(
            store.conn_mut(),
            &conductor_run::approval::store::NewApprovalRequest {
                id: other.clone(),
                subject: Subject::PolicyAction {
                    action: Action::GitPush,
                },
                run_id: Some(run_id()),
                facts: Default::default(),
                policy_hash: "blake3:whatever".to_string(),
                matched_rules: Vec::new(),
                explanation: "a different operation".to_string(),
                evidence_ref: None,
                expires: Expiry::At(now() + 60 * 60 * 1000),
            },
            now(),
        )
        .expect("seed the unrelated request");
    }
    human_grants(&world, &other, false);

    let mut store = world.store();
    let resumed = resume_on_grant(&mut store, &config(&world), now(), &mut ())
        .expect("the resume itself runs");
    drop(store);

    assert!(
        !matches!(resumed.outcome, VerticalOutcome::Complete { .. }),
        "a grant for git.push completed a run gated on dependency.add.runtime"
    );
    assert_eq!(world.run_state(), RunState::AwaitingApproval);
}

#[test]
fn an_expired_grant_does_not_resume_the_run() {
    let world = World::new();
    scope_includes_manifest(&world);
    pin_policy(&world, GATING_POLICY);
    run_dependency_fixture(&world);

    let request = requests(&world).remove(0);
    {
        let mut store = world.store();
        grant(
            store.conn_mut(),
            &request.id,
            &GrantOptions {
                id: "AG-expired".to_string(),
                scope: conductor_run::policy::model::Scope::from_pairs([(
                    "run".to_string(),
                    RUN.to_string(),
                )]),
                reuse: false,
                // Already past by the time the run looks at it.
                expires: Expiry::At(now() - 1),
                granted_by: "operator".to_string(),
                channel: "socket".to_string(),
                nonce_hash: None,
            },
            now() - 2,
        )
        .expect("grant");
    }

    let mut store = world.store();
    let resumed =
        resume_on_grant(&mut store, &config(&world), now(), &mut ()).expect("resume runs");
    drop(store);

    assert!(!matches!(resumed.outcome, VerticalOutcome::Complete { .. }));
    assert_eq!(world.run_state(), RunState::AwaitingApproval);
}

// ---------------------------------------------------------------------------
// Row 25 — revocation
// ---------------------------------------------------------------------------

#[test]
fn a_grant_revoked_before_it_is_spent_does_not_resume_the_run() {
    // Row 25's first half: revocation *before* consumption stops the run. The
    // check that matters is the one adjacent to the effect, which is why the
    // grant is re-authorised at the policy gate rather than trusted because it
    // existed when the human walked away.
    let world = World::new();
    scope_includes_manifest(&world);
    pin_policy(&world, GATING_POLICY);
    run_dependency_fixture(&world);

    let request = requests(&world).remove(0);
    let grant_id = human_grants(&world, &request.id, false);

    // The human changes their mind before anything is spent.
    {
        let mut store = world.store();
        conductor_run::approval::revoke(
            store.conn_mut(),
            &current_fence(&world),
            &grant_id,
            None,
            conductor_run::approval::InFlight::No,
            now(),
            "changed my mind",
        )
        .expect("revoke");
    }

    let mut store = world.store();
    let resumed =
        resume_on_grant(&mut store, &config(&world), now(), &mut ()).expect("resume runs");
    drop(store);

    assert!(
        !matches!(resumed.outcome, VerticalOutcome::Complete { .. }),
        "a revoked grant completed the run"
    );
    assert_eq!(world.run_state(), RunState::AwaitingApproval);
    let revoked = grant_row(world.store().conn(), &grant_id)
        .expect("grant row")
        .expect("a row");
    assert_eq!(revoked.state, GrantState::Revoked);
}

#[test]
fn revoking_after_consumption_does_not_pretend_the_effect_was_undone() {
    // Row 25's second half, and the honest half. Conductor "cannot undo an
    // external side effect" (§4.9). Once the grant has been spent and the work
    // integrated, revocation records that the authorisation was withdrawn — it
    // does not, and must not, claim the commit was rolled back.
    let world = World::new();
    scope_includes_manifest(&world);
    pin_policy(&world, GATING_POLICY);
    run_dependency_fixture(&world);

    let request = requests(&world).remove(0);
    let grant_id = human_grants(&world, &request.id, false);

    let mut store = world.store();
    let resumed =
        resume_on_grant(&mut store, &config(&world), now(), &mut ()).expect("resume runs");
    drop(store);
    assert!(matches!(resumed.outcome, VerticalOutcome::Complete { .. }));

    let outcome = {
        let mut store = world.store();
        conductor_run::approval::revoke(
            store.conn_mut(),
            &current_fence(&world),
            &grant_id,
            None,
            conductor_run::approval::InFlight::No,
            now(),
            "too late",
        )
        .expect("revoke after consumption is defined, not an error")
    };

    // The grant does not go back to GRANTED, and the run does not un-complete.
    let after = grant_row(world.store().conn(), &grant_id)
        .expect("grant row")
        .expect("a row");
    assert_eq!(
        after.state,
        GrantState::Consumed,
        "revoking a spent grant rewrote history: {outcome:?}"
    );
    assert_eq!(
        world.run_state(),
        RunState::Complete,
        "revocation retroactively un-completed a run whose commit already exists"
    );
    assert!(
        integrated_paths(&world).iter().any(|p| p == "Cargo.toml"),
        "the effect that already happened is no longer visible"
    );
}

// ---------------------------------------------------------------------------
// §4.8's audit surface, at the call site
// ---------------------------------------------------------------------------

#[test]
fn a_credential_the_agent_committed_becomes_a_finding_on_the_real_run_path() {
    // `enforce_audit` proves the *scanner* works. This proves the **call site
    // exists**, which is a different claim and the one S9 owns — the same
    // distinction that kept rows 12, 13, 25 and 30 at `NOT RUN` through S8.
    //
    // A scanner nobody calls detects nothing, and a module-level test cannot
    // tell you whether anybody calls it. So this goes through `run_task` and
    // reads the finding back out of the database.
    let world = World::new();
    pin_policy(&world, PERMISSIVE_POLICY);

    warm_the_binary();
    let scenario = scenario_file(&world.root(), "secret-in-diff");
    let adapter = conductor_agent::fake::FakeAgent::new(fake_agent_binary(), scenario)
        .with_max_lifetime_ms(20_000);
    {
        let mut store = world.store();
        let _ = run_task(&mut store, &adapter, &config(&world), &mut ());
    }

    let findings = world.store().findings_for_run(&run_id()).expect("findings");
    let secret = findings
        .iter()
        .find(|f| f.kind.starts_with("SECRET_IN_DIFF"))
        .unwrap_or_else(|| {
            panic!("the committed credential raised no finding; findings were {findings:?}")
        });

    // Findings never auto-resolve (§4.8).
    assert!(secret.resolution.is_none());

    // And the finding must not have copied the secret into the database, from
    // where every report and packet generated afterwards would carry it.
    for finding in &findings {
        assert!(
            !finding.evidence_ref.contains("AKIAIOSFODNN7EXAMPLE"),
            "the finding echoed the credential it was reporting: {}",
            finding.evidence_ref
        );
    }
    assert!(
        secret.evidence_ref.contains("aws-access-key-id"),
        "the finding does not say what kind of secret it found: {}",
        secret.evidence_ref
    );
}

#[test]
fn an_ordinary_change_on_the_real_run_path_raises_no_secret_finding() {
    // The other half. Without this, a build that raised `SECRET_IN_DIFF` on
    // every run would pass the test above.
    let world = World::new();
    pin_policy(&world, PERMISSIVE_POLICY);

    warm_the_binary();
    let scenario = scenario_file(&world.root(), "success");
    let adapter = conductor_agent::fake::FakeAgent::new(fake_agent_binary(), scenario)
        .with_max_lifetime_ms(20_000);
    {
        let mut store = world.store();
        let _ = run_task(&mut store, &adapter, &config(&world), &mut ());
    }

    let findings = world.store().findings_for_run(&run_id()).expect("findings");
    assert!(
        !findings
            .iter()
            .any(|f| f.kind.starts_with("SECRET_IN_DIFF")),
        "an ordinary change was reported as containing a secret: {findings:?}"
    );
}

// ---------------------------------------------------------------------------
// helpers that read the repository rather than the return value
// ---------------------------------------------------------------------------

fn attempts(world: &World) -> usize {
    world
        .store()
        .attempts_for_run(&run_id())
        .expect("attempts")
        .len()
}

/// The run's current fencing token.
///
/// Revocation is a fenced write, and a run waiting for a human holds no lease —
/// so the epoch on the row is what a caller at the control socket would use.
/// Reading it rather than assuming `0` keeps the test honest about which
/// generation of the run is being written to.
fn current_fence(world: &World) -> conductor_core::Fence {
    let epoch: i64 = world
        .store()
        .conn()
        .query_row(
            "SELECT lease_epoch FROM run WHERE id = ?1",
            rusqlite::params![RUN],
            |row| row.get(0),
        )
        .expect("lease epoch");
    conductor_core::Fence::new(run_id(), epoch)
}

/// The paths in the run branch's tip commit, read from git.
fn integrated_paths(world: &World) -> Vec<String> {
    let out = std::process::Command::new("git")
        .current_dir(&world.source)
        .args([
            "show",
            "--name-only",
            "--pretty=format:",
            common::vertical::RUN_BRANCH,
        ])
        .output()
        .expect("git show");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}
