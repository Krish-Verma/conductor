//! The S5 vertical: one fake-agent task from `PENDING` to `COMPLETE`, end to
//! end, with a real commit.
//!
//! > **Objective.** One fake-agent task from `PENDING` to `COMPLETE`, end to
//! > end, with a real commit. (Part 8, S5)
//!
//! Every assertion here reads the **database and the repository**, never the
//! return value alone: §4.8's whole thesis is that the report is evidence and
//! git is authority, and a test that believed a function's own summary would be
//! making the mistake the design exists to prevent.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use conductor_core::completion::Criterion;
use conductor_core::{PlanVersionId, RunState, TaskId, TaskState};
use conductor_git::Verdict;
use conductor_run::WorkerError;
use conductor_run::vertical::{VerticalConfig, VerticalOutcome, run_task};

use common::agent::{fake_agent_binary, scenario_file, warm_the_binary, write_scenario};
use common::vertical::{FAILING_PROFILE, RUN, RUN_BRANCH, TASK, World, commits_above, ref_updates};

/// Run the vertical over a catalogued scenario.
fn run_scenario(world: &World, scenario: &str) -> conductor_run::vertical::Vertical {
    try_scenario(world, scenario).expect("the vertical must not error")
}

/// The same, without insisting it succeeded.
///
/// Separate from [`run_scenario`] because a refusal *before the claim* — an
/// unmet dependency, an ineligible host — is reported as an error rather than
/// as a [`VerticalOutcome`]: there is no attempt to describe, so there is no
/// `Vertical` to return. A test about that refusal has to be able to see it.
fn try_scenario(
    world: &World,
    scenario: &str,
) -> Result<conductor_run::vertical::Vertical, WorkerError> {
    warm_the_binary();
    let path = scenario_file(&world.root(), scenario);
    drive(world, &path)
}

/// Run the vertical over a hand-written scenario.
fn run_json(world: &World, json: &str) -> conductor_run::vertical::Vertical {
    warm_the_binary();
    let path = write_scenario(&world.root(), json);
    drive(world, &path).expect("the vertical must not error")
}

fn drive(world: &World, scenario: &Path) -> Result<conductor_run::vertical::Vertical, WorkerError> {
    let adapter =
        conductor_agent::fake::FakeAgent::new(fake_agent_binary(), scenario.to_path_buf())
            .with_max_lifetime_ms(20_000);
    let mut store = world.store();
    let config = VerticalConfig {
        task_id: conductor_core::TaskId::new(TASK).expect("task id"),
        worker_id: "worker-1".to_string(),
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
        sensitive: conductor_git::SensitivePatterns::default(),
        agent_env_extra: Default::default(),
        // The fake agent authenticates against nothing.
        credential_home: None,
        // No `execution_requirements` on these fixtures' tasks, so §4.2's
        // gate compares an empty vector and proceeds without a probe.
        probe_key: conductor_run::containment::cache::ProbeKey::new(
            "fake", "test", "none", "n/a", "unprobed",
        ),
        // The worker derives §6.5's implementation packet — this is not a repair.
        instructions: None,
    };
    run_task(&mut store, &adapter, &config, &mut ())
}

/// Write the fixture task's `acceptance_criteria` column, in the exact bytes
/// `plan::materialize::canonical_criteria` produces.
///
/// The payload is written directly rather than by running the materializer,
/// because what is under test here is the **completion gate's** reading of the
/// column, not the materializer's writing of it — `plan_materialize.rs` already
/// pins the bytes, and coupling these tests to a whole plan document would make
/// a failure here ambiguous between the two.
fn declares_criteria(world: &World, json: &str) {
    world
        .store()
        .set_acceptance_criteria(&TaskId::new(TASK).expect("task id"), Some(json))
        .expect("write acceptance_criteria");
}

/// Write the fixture task's `depends_on` column, in the materializer's bytes.
fn declares_dependency(world: &World, json: &str) {
    world
        .store()
        .set_depends_on(&TaskId::new(TASK).expect("task id"), Some(json))
        .expect("write depends_on");
}

/// Add a second task to the store, so a dependency has something to point at.
fn seed_task(world: &World, id: &str) {
    world
        .store()
        .create_task(
            &conductor_store::NewTask {
                id: TaskId::new(id).expect("task id"),
                plan_version_id: "pv-1".to_string(),
                slice_id: "S11".to_string(),
                scope_globs: vec!["src/**".to_string()],
                verification_profile: "verification.yaml".to_string(),
                attempt_budget: 3,
            },
            0,
        )
        .expect("create task");
}

/// Walk a task to `COMPLETE` along §5.2's edges.
///
/// One `set_task_state` per edge rather than a single jump, because the store
/// enforces §5.2's legality table — `PENDING → COMPLETE` is refused, and
/// rightly. A fixture that could shortcut the machine would be testing a task
/// state the product cannot produce.
fn complete_task(world: &World, id: &str) {
    let task_id = TaskId::new(id).expect("task id");
    let mut store = world.store();
    for state in [
        TaskState::Ready,
        TaskState::Running,
        TaskState::Reconciling,
        TaskState::Verifying,
        TaskState::Complete,
    ] {
        store
            .set_task_state(&task_id, state)
            .unwrap_or_else(|e| panic!("{id} → {state}: {e}"));
    }
}

/// Every `run.state` the event journal recorded, in order.
///
/// The journal is how "RECONCILING is mandatory and unskippable" is checked
/// against what actually happened rather than against what a function returned.
fn state_history(world: &World) -> Vec<String> {
    let store = world.store();
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT payload FROM event
              WHERE run_id = ?1 AND kind = 'RUN_STATE_CHANGED' ORDER BY seq",
        )
        .expect("prepare");
    let rows: Vec<String> = stmt
        .query_map([RUN], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    rows.iter()
        .filter_map(|p| {
            serde_json::from_str::<serde_json::Value>(p)
                .ok()
                .and_then(|v| v.get("to").and_then(|t| t.as_str()).map(str::to_string))
        })
        .collect()
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

// ---------------------------------------------------------------------------
// The happy path.
// ---------------------------------------------------------------------------

#[test]
fn a_task_goes_from_pending_to_complete_with_a_real_commit() {
    // Acceptance row 1: "Success | — | `EXITED`, `CLEAN_COMPLETE`, all `PASS` |
    // commit, fetch, advance | no | no | `COMPLETE`".
    let world = World::new();
    assert_eq!(world.task_state(), TaskState::Pending);

    let result = run_scenario(&world, "success");

    let VerticalOutcome::Complete {
        commit, fetched, ..
    } = &result.outcome
    else {
        panic!("expected COMPLETE, got {:?}", result.outcome);
    };

    // The database.
    assert_eq!(world.run_state(), RunState::Complete);
    assert_eq!(world.task_state(), TaskState::Complete);
    assert_eq!(result.attempt.verdict, Verdict::CleanComplete);

    // The repository: one real commit on the run branch, carrying the work.
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        1
    );
    let files = git_out(
        &world.workspace(),
        &["show", "--name-only", "--format=", &commit.sha],
    );
    assert!(
        files.contains("src/added.rs"),
        "the commit is empty: {files}"
    );

    // …and the source repository has the branch, by one ref update.
    assert_eq!(
        git_out(&world.source, &["rev-parse", &fetched.reference]),
        commit.sha
    );
    assert_eq!(
        ref_updates(&world.source, &fetched.reference),
        1,
        "exactly one ref update"
    );
}

#[test]
fn the_completed_run_passed_through_reconciling_and_verifying() {
    // §5.2: "**`RECONCILING` is mandatory and unskippable**", and `COMPLETE`
    // sits downstream of `VERIFYING`. Asserted against the event journal, which
    // is what actually happened, rather than against the vertical's own report.
    let world = World::new();
    run_scenario(&world, "success");

    let history = state_history(&world);
    let reconciling = history
        .iter()
        .position(|s| s == "RECONCILING")
        .expect("the run must have passed through RECONCILING");
    let verifying = history
        .iter()
        .position(|s| s == "VERIFYING")
        .expect("the run must have passed through VERIFYING");
    let complete = history
        .iter()
        .position(|s| s == "COMPLETE")
        .expect("the run must have reached COMPLETE");
    assert!(
        reconciling < verifying && verifying < complete,
        "states arrived out of order: {history:?}"
    );
}

#[test]
fn the_user_repository_keeps_its_own_branch_and_checkout() {
    // §4.1: "Never pushed. Never auto-merged into the default branch." The
    // integration creates one ref and touches nothing else — including the
    // user's working tree, which they may well be editing.
    let world = World::new();
    let main_before = git_out(&world.source, &["rev-parse", "main"]);
    let branch_before = git_out(&world.source, &["rev-parse", "--abbrev-ref", "HEAD"]);

    run_scenario(&world, "success");

    assert_eq!(git_out(&world.source, &["rev-parse", "main"]), main_before);
    assert_eq!(
        git_out(&world.source, &["rev-parse", "--abbrev-ref", "HEAD"]),
        branch_before
    );
    assert_eq!(git_out(&world.source, &["status", "--porcelain"]), "");
}

#[test]
fn the_commit_carries_only_the_trailers_that_have_a_real_source() {
    // §3.4's five trailers exist so the audit trail "survives total local state
    // loss and travels with the repository". A fabricated hash is **worse**
    // than an absent trailer, because a reader recovering from total loss
    // cannot tell a made-up value from a real one.
    //
    // **Changed at S11.** This test used to assert `Conductor-Plan` was absent,
    // on the S5-era ground that "plan versioning is S11; emitting a plan
    // trailer now would fabricate one". S11 built the plan ledger, so the
    // trailer now has exactly the real source the old assertion was waiting
    // for: `task.plan_version_id` → `plan_version.version` and
    // `.content_hash`. Nothing about the *rule* changed — only whether this
    // trailer can satisfy it.
    let world = World::new();
    let result = run_scenario(&world, "success");
    let VerticalOutcome::Complete { commit, .. } = &result.outcome else {
        panic!("expected COMPLETE, got {:?}", result.outcome);
    };

    let trailers = git_out(
        &world.workspace(),
        &["log", "-1", "--format=%(trailers)", &commit.sha],
    );

    // Present, and real.
    assert!(
        trailers.contains(&format!("Conductor-Run: {RUN}")),
        "{trailers}"
    );
    assert!(trailers.contains("Conductor-Plan: v1@"), "{trailers}");
    assert!(trailers.contains("Conductor-Policy: blake3:"), "{trailers}");
    assert!(
        trailers.contains("Conductor-Verification: blake3:"),
        "{trailers}"
    );

    // Still deferred, and therefore absent rather than invented. S8 built the
    // grant machinery; what has no source yet is the *binding* value §3.4
    // shows — `AG-0019 binding=blake3:7d31…` — on a commit whose run consumed
    // no grant at all, which this one did not.
    assert!(
        !trailers.contains("Conductor-Approval"),
        "this run consumed no grant; an approval trailer here would name one \
         that does not exist: {trailers}"
    );
}

#[test]
fn the_plan_trailer_names_the_version_and_content_hash_of_the_plan_the_task_came_from() {
    // §3.4's format is `Conductor-Plan: v3@blake3:9ac2…`, and §3.5 makes it
    // load-bearing: recovery from total local loss reconstructs "which run,
    // plan version, policy snapshot and approval produced every
    // Conductor-authored commit" from the trailers alone. So the value has to
    // be the two facts a reader needs to find the document again — which
    // version, and what it hashed to — read from the row the task was
    // materialized under rather than restated from the row's id.
    let world = World::new();
    let result = run_scenario(&world, "success");
    let VerticalOutcome::Complete { commit, .. } = &result.outcome else {
        panic!("expected COMPLETE, got {:?}", result.outcome);
    };

    let plan = world
        .store()
        .plan_version(&PlanVersionId::new("pv-1").expect("plan version id"))
        .expect("query")
        .expect("the fixture registers pv-1");
    let value = git_out(
        &world.workspace(),
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Conductor-Plan,valueonly)",
            &commit.sha,
        ],
    );

    assert_eq!(
        value.trim(),
        format!("v{}@{}", plan.version, plan.content_hash)
    );
    assert_eq!(value.trim(), "v1@blake3:plan", "§3.4's literal shape");
}

#[test]
fn the_verification_trailer_is_a_digest_of_the_evidence_that_was_actually_used() {
    // A trailer whose value is not reproducible from the evidence is decoration.
    // This one is `blake3` over the checks the completion gate read, so a reader
    // with the artifacts can recompute it — which is what makes it audit.
    let world = World::new();
    let result = run_scenario(&world, "success");
    let VerticalOutcome::Complete { commit, .. } = &result.outcome else {
        panic!("expected COMPLETE, got {:?}", result.outcome);
    };
    let report = result
        .verification
        .as_ref()
        .expect("the happy path verified");

    let value = git_out(
        &world.workspace(),
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Conductor-Verification,valueonly)",
            &commit.sha,
        ],
    );
    assert_eq!(value.trim(), report.evidence_digest());
    assert!(value.starts_with("blake3:"), "{value}");
}

// ---------------------------------------------------------------------------
// Acceptance rows 2–6.
// ---------------------------------------------------------------------------

#[test]
fn row_2_a_crash_before_any_edit_leaves_nothing_committed() {
    // Row 2: "Crash before edits | kill at t=1s | `CRASHED`, `NO_CHANGE` | new
    // attempt, same packet". The new attempt is S6's — `REPAIRING → READY` is
    // the edge S6 owns, and S3's report already recorded that a run routed to
    // `REPAIRING` cannot be re-claimed before then. What S5 owns is the half
    // that must be true either way: **no effect happened**.
    let world = World::new();
    let result = run_scenario(&world, "crash-before-edits");

    assert_eq!(result.attempt.verdict, Verdict::NoChange);
    assert_eq!(world.run_state(), RunState::Repairing);
    assert_eq!(world.task_state(), TaskState::Repairing);
    assert!(matches!(result.outcome, VerticalOutcome::Stopped { .. }));
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        0
    );
}

#[test]
fn row_3_a_crash_after_edits_still_reaches_complete() {
    // Row 3: "Crash after edits | kill after writes | `CRASHED`,
    // `CLEAN_NO_REPORT` | verify current tree | … | `COMPLETE`". The agent died
    // without reporting; git says what happened, and verification decides.
    let world = World::new();
    let result = run_scenario(&world, "crash-after-edits");

    assert_eq!(result.attempt.verdict, Verdict::CleanNoReport);
    assert_eq!(world.run_state(), RunState::Complete);
    assert_eq!(world.task_state(), TaskState::Complete);
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        1,
        "the work the agent finished before dying must be committed"
    );
    let VerticalOutcome::Complete { commit, .. } = &result.outcome else {
        panic!("expected COMPLETE, got {:?}", result.outcome);
    };
    let files = git_out(
        &world.workspace(),
        &["show", "--name-only", "--format=", &commit.sha],
    );
    assert!(files.contains("src/added.rs"), "{files}");
}

#[test]
fn row_4_a_missing_report_does_not_stop_completion() {
    // Row 4: "Missing report | exit 0, no report | `EXITED`, `CLEAN_NO_REPORT` |
    // verification decides | no | no | `COMPLETE`". §4.5: "Note what is absent:
    // **the agent's report.**"
    let world = World::new();
    let result = run_scenario(&world, "missing-report");

    assert_eq!(result.attempt.verdict, Verdict::CleanNoReport);
    assert_eq!(world.task_state(), TaskState::Complete);
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        1
    );
}

#[test]
fn row_5_a_malformed_report_leaves_a_finding_and_still_completes() {
    // Row 5: "Malformed report | invalid JSON | finding `REPORT_UNPARSEABLE` |
    // verification decides; **finding stays** | no | no | `COMPLETE` + finding".
    let world = World::new();
    run_scenario(&world, "malformed-report");

    assert_eq!(world.task_state(), TaskState::Complete);
    let findings = world
        .store()
        .findings_for_run(&conductor_core::RunId::new(RUN).expect("id"))
        .expect("findings");
    let unparseable = findings
        .iter()
        .find(|f| f.kind == "REPORT_UNPARSEABLE")
        .expect("row 5 requires the finding");
    assert!(
        unparseable.resolution.is_none(),
        "§4.8: findings never auto-resolve"
    );
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        1
    );
}

#[test]
fn row_6_a_false_success_report_is_contradicted_and_stops_for_a_human() {
    // Row 6: "False success | 'complete', tree unchanged | `CONTRADICTED` |
    // halt | no | **yes** | `AWAITING_REVIEW`". §4.8: "**git wins**".
    let world = World::new();
    let result = run_json(
        &world,
        r#"{"id":"false-success","steps":[
             {"step":"emit","kind":"agent.started","detail":"lying"},
             {"step":"report_on_stdout","claim":"COMPLETE",
              "files_touched":["src/added.rs"],"summary":"all done (it is not)"},
             {"step":"exit","code":0}]}"#,
    );

    assert_eq!(result.attempt.verdict, Verdict::Contradicted);
    assert_eq!(world.run_state(), RunState::AwaitingReview);
    assert_eq!(world.task_state(), TaskState::AwaitingReview);
    assert!(matches!(result.outcome, VerticalOutcome::Stopped { .. }));

    // Nothing was committed on the strength of the claim.
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        0
    );
    // And it never reached verification: the report was refuted by the
    // repository before any check could be spent on it.
    assert!(result.verification.is_none());
    assert!(!state_history(&world).iter().any(|s| s == "VERIFYING"));
}

// ---------------------------------------------------------------------------
// Acceptance row 16 — the target branch moved.
// ---------------------------------------------------------------------------

#[test]
fn row_16_a_moved_target_branch_stops_at_awaiting_review_with_no_rebase() {
    // Row 16: "Target branch moved | user commits to `main` | divergence at
    // integration | **no rebase, no merge** | no | **yes** | `AWAITING_REVIEW`".
    let world = World::new();
    let moved = world.user_commits_to_main();

    let result = run_scenario(&world, "success");

    assert_eq!(world.run_state(), RunState::AwaitingReview);
    assert_eq!(world.task_state(), TaskState::AwaitingReview);
    let VerticalOutcome::Stopped { reason, .. } = &result.outcome else {
        panic!("expected a stop, got {:?}", result.outcome);
    };
    assert!(
        reason.contains(&moved) && reason.contains(&world.base_commit),
        "the divergence must name both shas: {reason}"
    );

    // Verification still ran — the work is verified, it just cannot be
    // integrated without a person. That distinction is what makes the review
    // packet worth reading.
    assert!(result.verification.is_some());

    // No rebase, no merge, no commit, no fetch.
    assert_eq!(git_out(&world.source, &["rev-parse", "main"]), moved);
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        0
    );

    // A human is told what happened, and the finding does not auto-resolve.
    let findings = world
        .store()
        .findings_for_run(&conductor_core::RunId::new(RUN).expect("id"))
        .expect("findings");
    let divergence = findings
        .iter()
        .find(|f| f.kind == "TARGET_BRANCH_MOVED")
        .expect("the divergence must be attached as a finding");
    assert!(divergence.resolution.is_none());
}

// ---------------------------------------------------------------------------
// The completion gate is not a formality.
// ---------------------------------------------------------------------------

#[test]
fn a_failing_check_stops_the_run_short_of_complete_and_commits_nothing() {
    // §4.5: "Verification is authoritative. The agent's report is not." The
    // agent reports COMPLETE and the check says otherwise; the check wins, and
    // no Conductor-owned effect happens on the strength of a green report.
    let world = World::new().with_profile(FAILING_PROFILE);
    let result = run_scenario(&world, "success");

    assert_eq!(result.attempt.verdict, Verdict::CleanComplete);
    assert_ne!(world.run_state(), RunState::Complete);
    assert_ne!(world.task_state(), TaskState::Complete);
    assert!(matches!(result.outcome, VerticalOutcome::Stopped { .. }));
    assert!(
        !result.refusals.is_empty(),
        "the gate must say which criterion it refused on"
    );
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        0
    );
    assert!(
        world
            .store()
            .unresolved_effects()
            .expect("effects")
            .is_empty(),
        "a refused run must not have opened an effect"
    );
}

// ---------------------------------------------------------------------------
// §4.5's criterion 5 — every acceptance criterion binds to ≥1 *passing* check.
// ---------------------------------------------------------------------------

/// One criterion, bound to the passing profile's required check.
const BOUND_CRITERION: &str = r#"[{"id":"AC-1","manual":false,"statement":"The unit tests prove it.","verified_by":["unit-tests"]}]"#;

#[test]
fn a_criterion_bound_to_a_check_that_failed_refuses_completion_in_its_own_name() {
    // §4.5 criterion 5 is "every acceptance criterion binds to ≥1 **passing**
    // check". The word doing the work is *passing*: a criterion bound to a
    // check that ran and failed is bound to nothing that proves it.
    //
    // The assertion is on the criterion, not merely on the outcome. A failing
    // required check already refuses under criterion 1, so "the task did not
    // complete" would be true even if criterion 5 had never looked — which is
    // exactly the state this task exists to end.
    let world = World::new().with_profile(FAILING_PROFILE);
    declares_criteria(&world, BOUND_CRITERION);

    let result = run_scenario(&world, "success");

    assert_ne!(world.task_state(), TaskState::Complete);
    let acceptance = result
        .refusals
        .iter()
        .find(|r| r.criterion == Criterion::AcceptanceBindings)
        .unwrap_or_else(|| {
            panic!(
                "criterion 5 must refuse in its own name: {:?}",
                result.refusals
            )
        });
    assert!(
        acceptance.detail.contains("AC-1") && acceptance.detail.contains("unit-tests"),
        "the refusal must name the criterion and the check a human has to look \
         at: {acceptance:?}"
    );
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0
    );
}

#[test]
fn a_manual_criterion_cannot_be_satisfied_mechanically_even_by_a_passing_check() {
    // §3.7: "Escape hatch: mark it `manual: true`, which forces a review
    // boundary." Bound here to a check that *does* pass, which is the strongest
    // form of the invariant: `manual` is not a statement about how much
    // evidence there is, it is a statement that no amount of mechanical
    // evidence settles the question.
    let world = World::new();
    declares_criteria(
        &world,
        r#"[{"id":"AC-2","manual":true,"statement":"A human has read the migration.","verified_by":["unit-tests"]}]"#,
    );

    let result = run_scenario(&world, "success");

    // Everything mechanical went right — that is the point.
    assert_eq!(result.attempt.verdict, Verdict::CleanComplete);
    assert!(
        result
            .verification
            .as_ref()
            .expect("the run verified")
            .all_passed(),
        "every check passed; only the manual criterion stands"
    );

    assert_ne!(world.task_state(), TaskState::Complete);
    assert!(
        result
            .refusals
            .iter()
            .any(|r| r.criterion == Criterion::AcceptanceBindings && r.detail.contains("AC-2")),
        "{:?}",
        result.refusals
    );
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0
    );
}

#[test]
fn a_criterion_bound_to_a_passing_check_reaches_complete_and_is_no_longer_deferred() {
    // **Positive control.** A gate that refused everything would satisfy both
    // tests above and be useless, so the same machinery has to let a task whose
    // criteria are genuinely satisfied through.
    //
    // The second assertion is what makes this more than a repeat of the happy
    // path: criterion 5 must be *evaluated* rather than deferred. Before S11
    // this task completed too — with `AcceptanceBindings` on the deferred list,
    // meaning nobody had looked.
    let world = World::new();
    declares_criteria(&world, BOUND_CRITERION);

    let result = run_scenario(&world, "success");

    let VerticalOutcome::Complete { deferred, .. } = &result.outcome else {
        panic!("expected COMPLETE, got {:?}", result.outcome);
    };
    assert_eq!(world.task_state(), TaskState::Complete);
    assert!(
        !deferred.contains(&Criterion::AcceptanceBindings),
        "criterion 5 was answered, not deferred: {deferred:?}"
    );
}

// ---------------------------------------------------------------------------
// §5.2's `PENDING ──deps met──► READY`.
// ---------------------------------------------------------------------------

#[test]
fn a_task_whose_dependency_has_not_completed_is_not_claimed() {
    // §5.2 draws exactly one edge out of `PENDING`, and it is conditional.
    // Before S11 there was no dependency graph, so "deps met" was vacuously
    // true; the materializer writes `task.depends_on`, so it is now a question
    // with an answer.
    let world = World::new();
    seed_task(&world, "T-0011");
    declares_dependency(&world, r#"["T-0011"]"#);

    let error = try_scenario(&world, "success").expect_err("the dependency is PENDING");

    assert!(
        error.to_string().contains("T-0011"),
        "the refusal must name the dependency a human has to chase: {error}"
    );
    // Still `PENDING`, deliberately. `BLOCKED` would be wrong: §5.2's fifth
    // correction gives it only `→ CANCELLED` and `→ SUPERSEDED`, so a task
    // blocked for waiting could never start once its dependency finished.
    assert_eq!(world.task_state(), TaskState::Pending);
    // Nothing was spent on it either: the gate is on `PENDING → READY`, which
    // is upstream of the claim, so there is no workspace, no agent and no ref.
    assert!(
        !world.workspace().exists(),
        "a task that may not be claimed must not have been cloned for"
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        0
    );
}

#[test]
fn a_task_whose_dependency_has_completed_is_claimed() {
    // **Positive control** for the gate above: a dependency gate that refused
    // unconditionally would pass that test and stall every plan with an edge in
    // it. `COMPLETE` is the state §5.2 makes terminal, so "met" is a fact that
    // cannot later become false.
    let world = World::new();
    seed_task(&world, "T-0011");
    complete_task(&world, "T-0011");
    declares_dependency(&world, r#"["T-0011"]"#);

    let result = run_scenario(&world, "success");

    assert!(result.is_complete(), "{:?}", result.outcome);
    assert_eq!(world.task_state(), TaskState::Complete);
}

// ---------------------------------------------------------------------------
// Acceptance row 29 — `.conductor/` mutated on a run branch.
// ---------------------------------------------------------------------------

#[test]
fn row_29_a_conductor_change_on_a_run_branch_is_rejected_and_never_fetched() {
    // Row 29: "`.conductor/` mutated on a run branch | agent writes `APPROVED` |
    // change rejected at reconciliation | **never fetched**; finding | no |
    // **yes** | `AWAITING_REVIEW`".
    //
    // §3.3 control 1 calls this rejection *unconditional*, so this test asserts
    // all three layers that make it so, not just the verdict: the verdict, the
    // CRITICAL finding (§4.5's criterion 4 blocks COMPLETE on one), and the ref
    // in the *registered* repository never moving.
    let world = World::new();
    let result = run_json(
        &world,
        r#"{"id":"forge-approval","steps":[
             {"step":"emit","kind":"agent.started","detail":"forging"},
             {"step":"write_file","path":"src/added.rs","contents":"pub fn added() {}\n"},
             {"step":"write_file","path":".conductor/plans/v1/APPROVED",
              "contents":"content_hash: blake3:0000\napprover: not-a-human\n"},
             {"step":"exit","code":0}]}"#,
    );

    assert_eq!(result.attempt.verdict, Verdict::GovernanceViolation);
    assert_eq!(world.run_state(), RunState::AwaitingReview);
    assert_eq!(world.task_state(), TaskState::AwaitingReview);

    // Layer 2: a CRITICAL finding naming the path. §4.8's findings never
    // auto-resolve, so this alone keeps the run off COMPLETE.
    let findings = world
        .store()
        .findings_for_run(&conductor_core::RunId::new(RUN).expect("id"))
        .expect("findings");
    let governance = findings
        .iter()
        .find(|f| f.kind == "GOVERNANCEPATH")
        .expect("row 29 requires the finding");
    assert_eq!(
        governance.severity, "CRITICAL",
        "§4.5 criterion 4 only blocks on a blocking severity: {governance:?}"
    );
    assert!(
        governance
            .evidence_ref
            .contains(".conductor/plans/v1/APPROVED"),
        "the finding must name the forged path: {governance:?}"
    );
    assert!(
        governance.resolution.is_none(),
        "§4.8: findings never auto-resolve"
    );

    // Layer 3: "Conductor never fetches such a change." The registered
    // repository's ref must not have moved.
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        0,
        "the run branch reached the registered repository"
    );

    // And it never spent a check on a tree it had already rejected.
    assert!(result.verification.is_none());
    assert!(!state_history(&world).iter().any(|s| s == "VERIFYING"));
}
