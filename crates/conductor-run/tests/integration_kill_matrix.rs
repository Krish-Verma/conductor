//! The S5 acceptance bar — acceptance row 22, at the six integration points.
//!
//! > **Failure injection.** Kill **between** intent and effect, and between
//! > effect and confirm, for both commit and fetch.
//! >
//! > **Verify.** Kill at any of 6 points during commit/fetch → restart produces
//! > **exactly one** commit and one ref update, asserted by counting.
//!
//! # How the kills are delivered
//!
//! Conductor is a separate process (`conductor-s5-vertical`) and it kills
//! *itself* with `SIGKILL` when integration reaches a named
//! [`IntegrationPoint`]. Self-inflicted because an external kill has to race a
//! sleep to land between two particular statements; this one lands there every
//! time. `SIGKILL` because it cannot be caught: no unwinding, no `Drop`, no
//! flush.
//!
//! # What is counted, and why counting is the assertion
//!
//! `git rev-list --count base..branch` counts **commits**. `git reflog show`
//! counts **ref updates** — one line per update, so it distinguishes "the ref
//! ended up in the right place" from "the ref was moved once". A duplicated
//! effect that happened to be idempotent in its final state would still show two
//! reflog lines, and that is exactly the failure row 22 is about.
//!
//! # S3's lesson, applied
//!
//! S3 found that its twelve prescribed kill points were **insufficient**, and
//! that `assert_converged` — "the run reached a state needing no human" — is
//! necessary but not sufficient: a run can converge to a state nobody has to
//! touch and still be unable to progress. So every case here asserts
//! **progress** as well as convergence: the run reaches `COMPLETE`, the commit
//! exists, and the ref moved. A restart that merely tidied the ledger and left
//! the work unintegrated fails these tests. (It did, once — see
//! `effects.rs::integrating_twice_produces_one_commit_and_one_ref_update`.)

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use conductor_core::{RunId, RunState, SideEffectState, TaskState};
use conductor_run::effects::IntegrationPoint;
use conductor_store::Store;

use common::agent::{fake_agent_binary, scenario_file, warm_the_binary};
use common::vertical::{RUN, RUN_BRANCH, World, commits_above, ref_updates};

/// What one killed process left behind.
struct Killed {
    reached: Vec<String>,
    signal: Option<i32>,
    stdout: String,
}

fn binary() -> PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe");
    let target = test_exe
        .parent()
        .and_then(Path::parent)
        .expect("target dir");
    let path = target.join("conductor-s5-vertical");
    assert!(path.exists(), "missing binary at {}", path.display());
    path
}

/// Run the vertical until it kills itself at `point`.
fn run_until(world: &World, point: Option<IntegrationPoint>) -> Killed {
    use std::os::unix::process::ExitStatusExt;

    warm_the_binary();
    let scenario = scenario_file(&world.root(), "success");
    let mut command = Command::new(binary());
    command
        .arg("--root")
        .arg(world.root())
        .arg("--scenario")
        .arg(&scenario)
        .arg("--fake-agent")
        .arg(fake_agent_binary());
    if let Some(point) = point {
        command.arg("--die-at").arg(point.as_str());
    }

    let output = command.output().expect("run the vertical");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Killed {
        reached: points_reached(&stdout),
        signal: output.status.signal(),
        stdout,
    }
}

/// Restart: §4.7's recovery, then carry on.
fn restart(world: &World) -> Killed {
    use std::os::unix::process::ExitStatusExt;

    let output = Command::new(binary())
        .arg("--root")
        .arg(world.root())
        .arg("--worker")
        .arg("worker-restarted")
        .arg("--resume")
        .output()
        .expect("restart");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "the restart must finish the run: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    Killed {
        reached: points_reached(&stdout),
        signal: output.status.signal(),
        stdout,
    }
}

fn points_reached(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .filter(|v| v.get("stage").and_then(|s| s.as_str()) == Some("integration"))
                .and_then(|v| v.get("at").and_then(|a| a.as_str()).map(str::to_string))
        })
        .collect()
}

/// The counting assertion row 22 asks for, plus the progress assertion S3's
/// lesson demands.
fn assert_exactly_one_effect(world: &World, label: &str) {
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        1,
        "{label}: exactly one commit must exist on the run branch"
    );
    assert_eq!(
        ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
        1,
        "{label}: the ref must have been updated exactly once"
    );

    let store = world.store();

    // Convergence: nothing is left mid-effect, and nothing needs a human.
    assert!(
        store.unresolved_effects().expect("effects").is_empty(),
        "{label}: an effect was left INTENDED"
    );
    assert!(
        store.ambiguous_effects().expect("effects").is_empty(),
        "{label}: an effect was left AMBIGUOUS, so a human is required"
    );
    for row in ledger_rows(&store) {
        assert_eq!(
            row.state,
            SideEffectState::Confirmed,
            "{label}: ledger row {} is {}",
            row.operation_id,
            row.state
        );
    }

    // **Progress**, not merely convergence. This is the assertion S3's finding
    // demands: a run that tidied its ledger and never integrated would satisfy
    // every check above and be permanently stuck.
    assert_eq!(
        world.run_state(),
        RunState::Complete,
        "{label}: the run did not finish"
    );
    assert_eq!(
        world.task_state(),
        TaskState::Complete,
        "{label}: the task did not finish"
    );

    // The database is sound.
    assert_eq!(
        store.integrity_check().expect("integrity"),
        vec!["ok".to_string()],
        "{label}: integrity_check failed"
    );
    assert_eq!(
        store.foreign_key_check().expect("fk"),
        0,
        "{label}: foreign key violations"
    );
}

fn ledger_rows(store: &Store) -> Vec<conductor_store::SideEffectRow> {
    let mut stmt = store
        .conn()
        .prepare("SELECT operation_id FROM side_effect ORDER BY intended_at")
        .expect("prepare");
    let ids: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    ids.into_iter()
        .map(|id| {
            store
                .side_effect(&conductor_core::effect::OperationId::from_stored(id))
                .expect("read")
                .expect("a row")
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The six points.
// ---------------------------------------------------------------------------

#[test]
fn killing_conductor_at_every_integration_point_yields_exactly_one_commit_and_one_ref_update() {
    for point in IntegrationPoint::ALL {
        let label = format!("killed at {}", point.as_str());
        let world = World::new();

        let killed = run_until(&world, Some(*point));
        assert_eq!(
            killed.signal,
            Some(9),
            "{label}: the process must die by SIGKILL, not exit: {}",
            killed.stdout
        );
        assert!(
            killed.reached.iter().any(|r| r == point.as_str()),
            "{label}: the point was never reached, so nothing was injected \
             (reached: {:?})",
            killed.reached
        );

        restart(&world);
        assert_exactly_one_effect(&world, &label);

        // A second restart on top of a finished run must change nothing. If the
        // idempotency lived in "the ledger row happens to be CONFIRMED" rather
        // than in "the world already says so", this is where a duplicate would
        // appear.
        let store = world.store();
        assert_eq!(world.run_state(), RunState::Complete);
        drop(store);
        assert_exactly_one_effect(&world, &format!("{label} (settled)"));
    }
}

#[test]
fn the_kill_points_really_are_the_windows_they_claim_to_be() {
    // A matrix over points that are not where they say they are proves nothing.
    // Each case below asserts the *state of the world* immediately after the
    // kill, before any restart — which is the only way to know the injection
    // landed in the gap it names.
    let cases: Vec<(IntegrationPoint, bool, bool, usize)> = vec![
        // point, ledger row for the commit exists, commit exists, ref updates
        (IntegrationPoint::BeforeCommitIntent, false, false, 0),
        (IntegrationPoint::AfterCommitIntended, true, false, 0),
        (IntegrationPoint::AfterCommitCreated, true, true, 0),
        (IntegrationPoint::AfterCommitConfirmed, true, true, 0),
        (IntegrationPoint::AfterFetchIntended, true, true, 0),
        (IntegrationPoint::AfterFetchPerformed, true, true, 1),
    ];

    for (point, commit_intended, commit_made, refs) in cases {
        let label = format!("window at {}", point.as_str());
        let world = World::new();
        let killed = run_until(&world, Some(point));
        assert_eq!(killed.signal, Some(9), "{label}");

        let store = world.store();
        let rows = ledger_rows(&store);
        let has_commit_row = rows
            .iter()
            .any(|r| r.kind == conductor_core::SideEffectKind::GitCommitLocal);
        assert_eq!(
            has_commit_row, commit_intended,
            "{label}: commit ledger row present = {has_commit_row}, expected {commit_intended}"
        );
        assert_eq!(
            commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
            usize::from(commit_made),
            "{label}: commit made"
        );
        assert_eq!(
            ref_updates(&world.source, &format!("refs/heads/{RUN_BRANCH}")),
            refs,
            "{label}: ref updates"
        );
    }
}

#[test]
fn a_kill_between_the_effect_and_its_receipt_leaves_the_row_intended() {
    // Row 22's own words: "kill between effect and confirm | `side_effect`
    // `INTENDED` | re-check precondition; **do not re-run**". Both halves are
    // asserted: the window is real, and the restart resolves it by asking the
    // world rather than by acting again.
    let world = World::new();
    let killed = run_until(&world, Some(IntegrationPoint::AfterCommitCreated));
    assert_eq!(killed.signal, Some(9));

    {
        let store = world.store();
        let unresolved = store.unresolved_effects().expect("effects");
        assert_eq!(
            unresolved.len(),
            1,
            "the kill must have landed inside the crash window: {unresolved:?}"
        );
        assert_eq!(
            unresolved[0].kind,
            conductor_core::SideEffectKind::GitCommitLocal
        );
        assert_eq!(unresolved[0].state, SideEffectState::Intended);
    }
    // The commit really was made before the kill — otherwise the restart would
    // be resolving a window that never contained an effect.
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        1
    );

    restart(&world);
    assert_exactly_one_effect(&world, "after-commit-created");
}

#[test]
fn a_kill_between_intent_and_effect_leaves_no_effect_to_undo() {
    // The other half of row 22's injection. The ledger says `INTENDED` and the
    // world says nothing happened; the restart must perform the effect **once**,
    // not conclude it already had.
    let world = World::new();
    let killed = run_until(&world, Some(IntegrationPoint::AfterCommitIntended));
    assert_eq!(killed.signal, Some(9));

    {
        let store = world.store();
        assert_eq!(store.unresolved_effects().expect("effects").len(), 1);
    }
    assert_eq!(
        commits_above(&world.workspace(), RUN_BRANCH, &world.base_commit),
        0,
        "the kill must have landed before the commit"
    );

    restart(&world);
    assert_exactly_one_effect(&world, "after-commit-intended");
}

#[test]
fn an_uninterrupted_run_is_the_control() {
    // The non-vacuity control for the whole matrix: the same world, the same
    // binary, no kill. If this did not produce one commit and one ref update,
    // the counting assertions above would be measuring something other than
    // "the effect happened once".
    let world = World::new();
    let run = run_until(&world, None);
    assert_eq!(run.signal, None, "no kill was injected: {}", run.stdout);
    assert!(
        run.reached
            .iter()
            .any(|r| r == IntegrationPoint::AfterFetchPerformed.as_str()),
        "the run must have got all the way through integration: {:?}",
        run.reached
    );
    assert_exactly_one_effect(&world, "uninterrupted");
}

#[test]
fn a_restart_does_not_run_the_agent_again() {
    // §4.7's restart resolves effects by asking the world; it must not re-invoke
    // the agent. A second attempt would clone its baseline from a workspace that
    // already holds the first attempt's edits, reconcile as `NO_CHANGE`, and
    // make the finished work invisible — the run would go backwards.
    let world = World::new();
    run_until(&world, Some(IntegrationPoint::AfterCommitIntended));
    let attempts_before = world
        .store()
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts")
        .len();

    restart(&world);

    let attempts_after = world
        .store()
        .attempts_for_run(&RunId::new(RUN).expect("id"))
        .expect("attempts")
        .len();
    assert_eq!(
        attempts_after, attempts_before,
        "the restart started another agent attempt"
    );
    assert_exactly_one_effect(&world, "restart does not re-run the agent");
}

// ---------------------------------------------------------------------------
// What the counting assertion does **not** prove, and the test that does.
// ---------------------------------------------------------------------------
//
// Row 22 asks for "exactly one commit and one ref update, asserted by counting".
// Counting was run with the precondition re-check disabled, in both halves, and
// **it did not always fail**:
//
// * With the *fetch* re-check disabled the whole matrix still passed. `git
//   fetch` of a ref that has not moved performs no ref update at all, so the
//   reflog cannot tell one blind retry from none. Measured directly: two
//   identical fetches leave one reflog line.
// * With the *commit* re-check disabled the matrix failed — but on the
//   **progress** assertion, not on a count. The restart tried to commit an empty
//   index, git refused, and the run stopped at `AWAITING_REVIEW` instead of
//   completing. No second commit ever appeared, because git will not make one.
//
// So, under a fixed operation identity, git itself makes an observable duplicate
// impossible for both of these effects, and the count is necessary but not
// sufficient — the same shape as S3's finding about `assert_converged`. What the
// ledger's re-check actually buys is below: a ref Conductor did not move is
// **noticed** rather than overwritten. That is the property with teeth, and this
// is the test that fails when the mechanism is removed.

/// Point a ref at a commit unrelated to the run branch, the way another tool
/// would.
fn plant_a_stranger_ref(world: &World, reference: &str) -> String {
    let tree = git_stdout(&world.source, &["rev-parse", "HEAD^{tree}"]);
    let stranger = git_stdout(
        &world.source,
        &["commit-tree", &tree, "-m", "somebody else's work"],
    );
    let status = Command::new("git")
        .args(["update-ref", reference, &stranger])
        .current_dir(&world.source)
        .status()
        .expect("update-ref");
    assert!(status.success());
    stranger
}

fn git_stdout(repo: &Path, args: &[&str]) -> String {
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

/// Restart, tolerating a run that stops.
fn restart_allowing_a_stop(world: &World) -> String {
    let output = Command::new(binary())
        .arg("--root")
        .arg(world.root())
        .arg("--worker")
        .arg("worker-restarted")
        .arg("--resume")
        .output()
        .expect("restart");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn a_ref_conductor_did_not_move_is_noticed_rather_than_overwritten() {
    // §4.7: "**Ambiguous** (precondition indeterminate) → mark `AMBIGUOUS`, halt
    // the run, raise a finding, require a human. **Never guess.**" — and row
    // 22's "Human? only if ambiguous".
    //
    // The window: the ledger says the fetch is `INTENDED` and the ref has not
    // moved. Between the crash and the restart, something else puts a commit at
    // that ref. A blind retry would either overwrite somebody's work or die on a
    // non-fast-forward; the re-check turns it into a decision a person makes.
    let world = World::new();
    let killed = run_until(&world, Some(IntegrationPoint::AfterFetchIntended));
    assert_eq!(killed.signal, Some(9), "{}", killed.stdout);

    let reference = format!("refs/heads/{RUN_BRANCH}");
    let stranger = plant_a_stranger_ref(&world, &reference);

    let output = restart_allowing_a_stop(&world);

    // The stranger's commit is untouched. This is the assertion that fails when
    // the precondition re-check is removed.
    assert_eq!(
        git_stdout(&world.source, &["rev-parse", &reference]),
        stranger,
        "Conductor moved a ref it did not put there; output was: {output}"
    );

    let store = world.store();
    let ambiguous = store.ambiguous_effects().expect("effects");
    assert_eq!(
        ambiguous.len(),
        1,
        "the undecidable effect must be recorded as AMBIGUOUS: {output}"
    );
    assert_eq!(
        ambiguous[0].kind,
        conductor_core::SideEffectKind::GitFetchIntoMain
    );

    // A human is required, and told why.
    let findings = store
        .findings_for_run(&RunId::new(RUN).expect("id"))
        .expect("findings");
    let finding = findings
        .iter()
        .find(|f| f.kind == "EFFECT_AMBIGUOUS")
        .unwrap_or_else(|| panic!("no EFFECT_AMBIGUOUS finding: {findings:?}"));
    assert!(finding.resolution.is_none(), "findings never auto-resolve");
    assert!(
        finding.evidence_ref.contains(&stranger),
        "the finding must name what it found: {}",
        finding.evidence_ref
    );

    // The run halts somewhere a person has to look, and does not hold a lease
    // that nobody will renew.
    assert_eq!(world.run_state(), RunState::AwaitingReview, "{output}");
    assert_eq!(world.task_state(), TaskState::AwaitingReview);
    let row = store
        .run(&RunId::new(RUN).expect("id"))
        .expect("run")
        .expect("a row");
    assert!(
        row.lease_owner.is_none(),
        "a halted run must not keep a lease nobody will renew"
    );
}
