//! S8's durability bar — 50 kill-restart cycles, and no grant consumed twice.
//!
//! > **Failure injection.** Kill during an approval wait · kill between grant
//! > and consume · socket file deleted.
//! >
//! > **Verify.** Approval state survives 50 kill-restart cycles · **no grant
//! > consumed twice**.
//!
//! # What "kill-restart" means here
//!
//! A real `SIGKILL` to a real process, fifty times, following the crash matrix's
//! technique: the victim kills *itself* at the point that matters, because an
//! external kill has to race a sleep to land between two particular statements
//! and a self-inflicted one lands there every time. The existing
//! `a_grant_survives_reopening_the_store_and_still_authorizes_exactly_once`
//! reopens the store once, inside one process, and that is a different and much
//! weaker claim: it shows `open_existing` reads back what `open_or_create`
//! wrote. It cannot show that an uncommitted approval never becomes a real one,
//! because nothing ever dies.
//!
//! # The one assertion the slice names, and the control that makes it mean
//! something
//!
//! Fifty processes each try to consume the same one-shot grant. **Exactly one**
//! may report `consumed`. A run in which none did would satisfy "no grant
//! consumed twice" perfectly and prove nothing at all — it is the shape of every
//! vacuous test the previous five slices shipped. So
//! [`fifty_kill_restart_cycles_never_consume_a_grant_twice`] asserts the count
//! is exactly one, names which cycle won, and
//! [`the_fifty_cycles_did_not_break_consumption_itself`] issues a **fresh**
//! grant after all fifty kills and consumes it, so the forty-nine refusals are
//! demonstrably about the grant being spent rather than about consumption having
//! stopped working.

use std::path::{Path, PathBuf};
use std::process::Command;

use conductor_core::{RunId, TaskId};
use conductor_run::approval::binding::Binding;
use conductor_run::approval::kind::{Expiry, Subject};
use conductor_run::approval::store as approvals;
use conductor_run::approval::store::{
    Consumption, GrantOptions, GrantState, NewApprovalRequest, RequestState,
};
use conductor_run::policy::evaluate::{Decision, Request, evaluate};
use conductor_run::policy::load;
use conductor_run::policy::model::{Action, Fact, FactSet, Origin, ResolvedPolicy, Scope};
use conductor_store::{NewRun, NewTask, Store};

/// How many times the process is killed and restarted.
const CYCLES: i64 = 50;

/// Must match the victim binary.
const RUN: &str = "r-0041";
const REQUEST_TTL_BASE_MS: i64 = 1_800_000_000_000;
const GRANT_TTL_MS: i64 = 1_900_000_000_000;

const SHARED_REQUEST: &str = "AR-shared";
const SHARED_GRANT: &str = "AG-shared";

const GATING_YAML: &str = r#"
policy:
  rules:
    - id: global.runtime-dependency
      action: dependency.add.runtime
      effect: require_approval
"#;

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

fn policy() -> ResolvedPolicy {
    let document = load::parse_document(GATING_YAML, Origin::Global).expect("parse");
    load::resolve_documents(Some(document), None, None).expect("resolve")
}

fn dependency_facts(name: &str) -> FactSet {
    let mut facts = FactSet::new();
    facts.push(Fact::deterministic("dependency", name));
    facts.push(Fact::deterministic("manifest", "Cargo.toml"));
    facts
}

fn decision(policy: &ResolvedPolicy, name: &str) -> Decision {
    let request = Request::new(Action::parse("dependency.add.runtime"), 1_000)
        .with_facts(dependency_facts(name))
        .with_context("run", RUN);
    evaluate(policy, &request)
}

fn run_scope() -> Scope {
    Scope::from_pairs([("run".to_string(), RUN.to_string())])
}

/// The store the cycles run against: one run, one gated decision, one one-shot
/// grant.
fn seed(path: &Path, policy: &ResolvedPolicy) -> Decision {
    let mut store = Store::open_or_create(path).expect("create store");
    let snapshot = load::snapshot(policy);
    conductor_store::with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT OR IGNORE INTO project
               (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES ('p-1', '/repo', 'blake3:repo', 'main', 'blake3:cfg', 0)",
            [],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO plan_version
               (id, project_id, version, content_hash, state, source_path)
             VALUES ('pv-1', 'p-1', 1, 'blake3:plan', 'DRAFT', '.conductor/plans/v1/plan.yaml')",
            [],
        )?;
        Ok(())
    })
    .expect("seed parents");
    load::persist(store.conn_mut(), &snapshot, 0).expect("persist snapshot");

    let task_id = TaskId::new("T-0041").expect("task id");
    store
        .create_task(
            &NewTask {
                id: task_id.clone(),
                plan_version_id: "pv-1".to_string(),
                slice_id: "S8".to_string(),
                scope_globs: vec!["crates/**".to_string()],
                verification_profile: "default".to_string(),
                attempt_budget: 3,
            },
            0,
        )
        .expect("create task");
    store
        .create_run(
            &NewRun {
                id: RunId::new(RUN).expect("run id"),
                task_id,
                policy_hash: snapshot.hash.clone(),
                base_commit: "abc123".to_string(),
                run_branch: format!("conductor/{RUN}"),
                target_branch: "main".to_string(),
            },
            0,
        )
        .expect("create run");

    let shared = decision(policy, "the-shared-one");
    approvals::request(
        store.conn_mut(),
        &NewApprovalRequest {
            id: SHARED_REQUEST.to_string(),
            subject: Subject::PolicyAction {
                action: shared.action.clone(),
            },
            run_id: Some(RunId::new(RUN).expect("run id")),
            facts: shared.facts.iter().cloned().collect(),
            policy_hash: shared.policy_hash.clone(),
            matched_rules: vec!["global.runtime-dependency".to_string()],
            explanation: "the grant fifty processes will fight over".to_string(),
            evidence_ref: None,
            expires: Expiry::At(GRANT_TTL_MS),
        },
        0,
    )
    .expect("seed the shared request");
    approvals::grant(
        store.conn_mut(),
        SHARED_REQUEST,
        &GrantOptions {
            id: SHARED_GRANT.to_string(),
            scope: run_scope(),
            reuse: false,
            expires: Expiry::At(GRANT_TTL_MS),
            granted_by: "krish".to_string(),
            channel: "unix-socket".to_string(),
            nonce_hash: None,
        },
        0,
    )
    .expect("seed the shared grant");
    shared
}

fn victim_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_conductor-s8-approval-victim"))
}

/// What one cycle claimed to have done before it died.
#[derive(Debug, Default)]
struct Cycle {
    consume: String,
    committed: Option<String>,
    granted: Option<String>,
    doomed: Option<String>,
    signalled: bool,
}

/// Run one cycle to its self-inflicted `SIGKILL` and read back what it said.
fn run_cycle(db: &Path, cycle: i64, binding: &str) -> Cycle {
    let output = Command::new(victim_binary())
        .arg(db)
        .arg(cycle.to_string())
        .arg(SHARED_GRANT)
        .arg(binding)
        .output()
        .unwrap_or_else(|err| panic!("spawn the victim: {err}"));

    let mut parsed = Cycle {
        // `signal()` is `Some(9)` only when the process really died of SIGKILL.
        // A clean exit here would mean the kill never landed and the cycle was
        // a normal shutdown wearing a crash's name.
        signalled: {
            use std::os::unix::process::ExitStatusExt;
            output.status.signal() == Some(9)
        },
        ..Cycle::default()
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        match line.split_once(' ') {
            Some(("CONSUME", rest)) => parsed.consume = rest.to_string(),
            Some(("COMMITTED", rest)) => parsed.committed = Some(rest.to_string()),
            Some(("GRANTED", rest)) => parsed.granted = Some(rest.to_string()),
            Some(("DOOMED", rest)) => parsed.doomed = Some(rest.to_string()),
            _ => {}
        }
    }
    assert!(
        parsed.signalled,
        "cycle {cycle} did not die of SIGKILL (status {:?}); stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !parsed.consume.is_empty(),
        "cycle {cycle} said nothing about the shared grant; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    parsed
}

// ---------------------------------------------------------------------------
// the slice's Verify line
// ---------------------------------------------------------------------------

#[test]
fn fifty_kill_restart_cycles_never_consume_a_grant_twice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");
    let policy = policy();
    let shared = seed(&db, &policy);
    let binding = Binding::for_decision(&shared, &run_scope()).hash();

    let mut cycles = Vec::new();
    for cycle in 1..=CYCLES {
        cycles.push(run_cycle(&db, cycle, binding.as_str()));

        // The store must be usable again *immediately* after every kill. A
        // database that only recovers once, at the end, would let 49 of these
        // cycles run against a file nothing could open.
        let store = Store::open_existing(&db).expect("reopen after the kill");
        assert_eq!(
            store.integrity_check().expect("integrity check"),
            vec!["ok".to_string()],
            "the database is damaged after cycle {cycle}"
        );
    }

    // ---- no grant consumed twice -----------------------------------------
    let consumed: Vec<usize> = cycles
        .iter()
        .enumerate()
        .filter(|(_, cycle)| cycle.consume == "consumed")
        .map(|(index, _)| index + 1)
        .collect();
    assert_eq!(
        consumed.len(),
        1,
        "exactly one of {CYCLES} cycles may consume the one-shot grant; these \
         did: {consumed:?}. Zero would satisfy \"never twice\" while proving \
         nothing."
    );
    assert_eq!(
        consumed[0], 1,
        "the first cycle is the one that finds the grant live"
    );
    for (index, cycle) in cycles.iter().enumerate().skip(1) {
        assert!(
            cycle.consume.starts_with("refused:"),
            "cycle {} must be refused, not {:?}",
            index + 1,
            cycle.consume
        );
        assert!(
            cycle.consume.contains("already been consumed"),
            "cycle {} must be refused *because it was spent*, not for some \
             other reason: {:?}",
            index + 1,
            cycle.consume
        );
    }

    // ---- approval state survives -----------------------------------------
    let store = Store::open_existing(&db).expect("reopen");
    assert_eq!(
        approvals::grant_row(store.conn(), SHARED_GRANT)
            .expect("read")
            .expect("row")
            .state,
        GrantState::Consumed,
        "the terminal state must have outlived 50 kills"
    );

    for (index, cycle) in cycles.iter().enumerate() {
        let number = index as i64 + 1;

        // Committed requests survive, with the exact TTL they were written
        // with. A TTL that came back as a default would look identical to a
        // preserved one if every cycle used the same number, which is why each
        // cycle uses its own.
        let id = cycle
            .committed
            .as_ref()
            .unwrap_or_else(|| panic!("cycle {number} committed no request"));
        let row = approvals::request_row(store.conn(), id)
            .expect("read")
            .unwrap_or_else(|| panic!("{id} did not survive"));
        assert_eq!(
            row.expires,
            Expiry::At(REQUEST_TTL_BASE_MS + number),
            "{id} came back with the wrong TTL"
        );
        let expected_state = if cycle.granted.is_some() {
            RequestState::Granted
        } else {
            RequestState::Requested
        };
        assert_eq!(
            row.state, expected_state,
            "{id} came back in the wrong state"
        );

        // Committed grants survive.
        if let Some(grant_id) = &cycle.granted {
            let grant = approvals::grant_row(store.conn(), grant_id)
                .expect("read")
                .unwrap_or_else(|| panic!("{grant_id} did not survive"));
            assert_eq!(grant.state, GrantState::Granted);
            assert_eq!(grant.expires, Expiry::At(GRANT_TTL_MS));
        }

        // And the write that was never committed is **absent**. This is the
        // half that matters: a half-finished approval must never become a real
        // one, and the process that started it did not live to roll it back.
        if let Some(doomed) = &cycle.doomed {
            assert!(
                approvals::request_row(store.conn(), doomed)
                    .expect("read")
                    .is_none(),
                "{doomed} was never committed and must not exist"
            );
        }
    }

    // The fixture must have exercised all three shapes, or the loop above is
    // asserting less than it appears to.
    assert!(
        cycles.iter().filter(|c| c.granted.is_some()).count() >= CYCLES as usize / 2 - 1,
        "too few cycles granted"
    );
    assert!(
        cycles.iter().filter(|c| c.doomed.is_some()).count() >= CYCLES as usize / 3 - 1,
        "too few cycles wrote a doomed row"
    );
}

#[test]
fn the_fifty_cycles_did_not_break_consumption_itself() {
    // POSITIVE CONTROL for the test above. Forty-nine refusals are only evidence
    // of "no grant consumed twice" if consumption still *works* at the end. If
    // fifty `SIGKILL`s had left the store in a state where nothing could be
    // consumed, the assertions above would read exactly the same.
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("conductor.db");
    let policy = policy();
    let shared = seed(&db, &policy);
    let binding = Binding::for_decision(&shared, &run_scope()).hash();

    for cycle in 1..=CYCLES {
        run_cycle(&db, cycle, binding.as_str());
    }

    let mut store = Store::open_existing(&db).expect("reopen");
    let fresh = decision(&policy, "issued-after-the-kills");
    approvals::request(
        store.conn_mut(),
        &NewApprovalRequest {
            id: "AR-after".to_string(),
            subject: Subject::PolicyAction {
                action: fresh.action.clone(),
            },
            run_id: Some(RunId::new(RUN).expect("run id")),
            facts: fresh.facts.iter().cloned().collect(),
            policy_hash: fresh.policy_hash.clone(),
            matched_rules: vec!["global.runtime-dependency".to_string()],
            explanation: "issued after fifty kills".to_string(),
            evidence_ref: None,
            expires: Expiry::At(GRANT_TTL_MS),
        },
        0,
    )
    .expect("a request must still be recordable");
    approvals::grant(
        store.conn_mut(),
        "AR-after",
        &GrantOptions {
            id: "AG-after".to_string(),
            scope: run_scope(),
            reuse: false,
            expires: Expiry::At(GRANT_TTL_MS),
            granted_by: "krish".to_string(),
            channel: "unix-socket".to_string(),
            nonce_hash: None,
        },
        0,
    )
    .expect("a grant must still be issuable");

    let fresh_binding = Binding::for_decision(&fresh, &run_scope()).hash();
    match approvals::consume(store.conn_mut(), "AG-after", &fresh_binding, 1_000).expect("consume")
    {
        Consumption::Consumed { grant_id } => assert_eq!(grant_id, "AG-after"),
        other => panic!("consumption must still work after fifty kills: {other:?}"),
    }
    // …and is still one-shot.
    match approvals::consume(store.conn_mut(), "AG-after", &fresh_binding, 1_001).expect("consume")
    {
        Consumption::Refused(_) => {}
        other => panic!("and must still be one-shot: {other:?}"),
    }
}
