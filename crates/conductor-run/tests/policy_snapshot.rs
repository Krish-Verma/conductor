//! S7 — loading, canonical snapshots, run-lifetime pinning and the §4.2
//! eligibility gate.
//!
//! Master plan §4.4: *"At run creation Conductor canonically serializes the
//! resolved policy (sorted keys, no timestamps), hashes it BLAKE3, stores it
//! content-addressed, and pins `policy_hash` on the run. **A run evaluates
//! against its snapshot for its entire life**."*
//!
//! As in `policy.rs`, every refusal here is paired with a positive control that
//! proves the assertion is capable of failing.

use conductor_core::containment::{
    Enforcement, ExecutionCapabilities, GatingDimension, Informational,
};
use conductor_core::{RunId, TaskId};
use conductor_run::containment::cache::{self, CacheLookup, ProbeKey};
use conductor_run::policy::eligibility::{self, Eligibility, ExecutionRequirements, ProbeStatus};
use conductor_run::policy::evaluate::{DriftDecision, Request, drift, evaluate};
use conductor_run::policy::load::{self, PolicyError};
use conductor_run::policy::model::{Action, Effect, Origin, ResolvedPolicy};
use conductor_store::{NewRun, NewTask, Store};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

const GLOBAL_YAML: &str = r#"
policy:
  rules:
    - id: global.no-force-push
      action: git.force_push
      effect: deny
      locked: true
    - id: global.deploy
      action: deployment.execute
      effect: require_approval
  exceptions:
    - id: global.temporary-ship
      action: deployment.execute
      effect: allow
      scope: {run: r-0001}
      expires_at: "2026-08-13T14:03:00Z"
"#;

/// The same policy, written by a different person: keys reordered, comments
/// added, quoting and indentation changed, blank lines inserted.
const GLOBAL_YAML_REFORMATTED: &str = r#"
# our house policy
policy:

  exceptions:
    -   expires_at: '2026-08-13T14:03:00Z'
        scope:
          run: "r-0001"
        effect: allow
        action: "deployment.execute"
        id: global.temporary-ship

  rules:

    -   effect: "deny"       # nobody force-pushes
        locked:   true
        action: git.force_push
        id: "global.no-force-push"

    -   action: deployment.execute
        id: global.deploy
        effect: require_approval
"#;

const PROJECT_PERMISSIVE: &str = r#"
policy:
  rules:
    - id: project.deps
      action: dependency.add.runtime
      effect: allow
"#;

const PROJECT_TIGHTENED: &str = r#"
policy:
  rules:
    - id: project.deps
      action: dependency.add.runtime
      effect: deny
"#;

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fixture");
    path
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open_or_create(dir.path().join("conductor.db")).expect("open store");
    (dir, store)
}

/// A run pinned to `policy`, with the parent rows the schema requires.
///
/// `created_at` is explicit because the pinning tests seed **two** runs pinned to
/// **two** snapshots. Without a second, newer snapshot in the store, a
/// `pinned_for_run` that ignored `run.policy_hash` and simply took whatever
/// policy it found would still return the right answer, and the pinning test
/// would be vacuous.
fn seed_run(store: &mut Store, run_id: &str, policy: &ResolvedPolicy, created_at: i64) -> String {
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

    load::persist(store.conn_mut(), &snapshot, created_at).expect("persist snapshot");

    let task_id = TaskId::new(format!("T-{run_id}")).expect("task id");
    store
        .create_task(
            &NewTask {
                id: task_id.clone(),
                plan_version_id: "pv-1".to_string(),
                slice_id: "S7".to_string(),
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
                id: RunId::new(run_id).expect("run id"),
                task_id,
                policy_hash: snapshot.hash.clone(),
                base_commit: "abc123".to_string(),
                run_branch: format!("conductor/{run_id}"),
                target_branch: "main".to_string(),
            },
            0,
        )
        .expect("create run");
    snapshot.hash
}

// ---------------------------------------------------------------------------
// loading
// ---------------------------------------------------------------------------

#[test]
fn the_specification_shape_loads_into_the_model_it_describes() {
    let doc = load::parse_document(GLOBAL_YAML, Origin::Global).expect("load");
    assert_eq!(doc.rules().len(), 2);
    assert_eq!(doc.rules()[0].id, "global.no-force-push");
    assert!(doc.rules()[0].locked);
    assert_eq!(doc.rules()[0].effect, Effect::Deny);
    assert!(!doc.rules()[1].locked);
    assert_eq!(doc.rules()[1].effect, Effect::RequireApproval);

    assert_eq!(doc.exceptions().len(), 1);
    let exception = &doc.exceptions()[0];
    assert_eq!(exception.action, Action::parse("deployment.execute"));
    assert_eq!(exception.effect, Effect::Allow);
    // 2026-08-13T14:03:00Z — 20678 days after the epoch, plus 14h03m.
    assert_eq!(exception.expires_at_ms, 1_786_629_780_000);
}

#[test]
fn malformed_policy_is_a_named_error_and_never_a_permissive_default() {
    let cases: &[(&str, &str)] = &[
        ("policy:\n  rules: [", "YAML"),
        ("rules: []\n", "policy"),
        (
            "policy:\n  rules:\n    - {action: git.push, effect: deny}\n",
            "id",
        ),
        ("policy:\n  rules:\n    - {id: a, effect: deny}\n", "action"),
        (
            "policy:\n  rules:\n    - {id: a, action: git.push}\n",
            "effect",
        ),
        (
            "policy:\n  rules:\n    - {id: a, action: git.push, effect: maybe}\n",
            "effect",
        ),
        (
            "policy:\n  rules:\n    - {id: a, action: git.push, effect: deny}\n    - {id: a, action: git.pull, effect: deny}\n",
            "twice",
        ),
        (
            "policy:\n  exceptions:\n    - {id: x, action: deployment.execute, effect: allow, scope: {run: r}}\n",
            "expires_at",
        ),
        (
            "policy:\n  exceptions:\n    - {id: x, action: \"dependency.*\", effect: allow, scope: {run: r}, expires_at: \"2026-08-13T14:03:00Z\"}\n",
            "exactly one action",
        ),
        (
            "policy:\n  exceptions:\n    - {id: x, action: deployment.execute, effect: allow, scope: {run: r}, expires_at: \"whenever\"}\n",
            "RFC 3339",
        ),
    ];

    for (yaml, needle) in cases {
        let err = load::parse_document(yaml, Origin::Global)
            .expect_err("malformed policy must be refused, not defaulted");
        assert!(
            err.to_string().contains(needle),
            "error for {yaml:?} did not mention {needle:?}: {err}"
        );
    }

    // Positive control: the shape these all deviate from does load.
    assert!(load::parse_document(GLOBAL_YAML, Origin::Global).is_ok());
}

#[test]
fn a_project_document_may_not_declare_a_locked_rule() {
    let yaml = "policy:\n  rules:\n    - {id: p, action: git.push, effect: deny, locked: true}\n";
    let err = load::parse_document(yaml, Origin::Project).expect_err("must be refused");
    assert!(err.to_string().contains("locked"), "{err}");

    // Positive control: the identical text loads as a global document.
    assert!(load::parse_document(yaml, Origin::Global).is_ok());
}

#[test]
fn a_built_in_invariant_cannot_be_declared_loosened_or_excepted_from_yaml() {
    // §4.4: "built-in invariants are not configurable at all". The loader has no
    // syntax for declaring one, and refuses any attempt to name one in an
    // exception — which is the only construct that could lower an effect.
    let attempts = [
        "policy:\n  builtin_invariants:\n    - never_push_to_a_remote\n",
        "policy:\n  invariants:\n    - {id: i, action: git.push, effect: allow}\n",
        "policy:\n  exceptions:\n    - {id: x, action: git.push, effect: allow, scope: {run: r}, expires_at: \"2026-08-13T14:03:00Z\"}\n",
        "policy:\n  exceptions:\n    - {id: x, action: filesystem.write.outside_workspace, effect: allow, scope: {run: r}, expires_at: \"2026-08-13T14:03:00Z\"}\n",
    ];
    for yaml in attempts {
        let err = load::parse_document(yaml, Origin::Global)
            .expect_err("configuring a built-in invariant must be refused");
        assert!(
            matches!(err, PolicyError::BuiltinNotConfigurable { .. })
                || err.to_string().contains("built-in"),
            "{yaml:?} produced the wrong error: {err}"
        );
    }

    // Positive control: an exception naming an action no invariant governs is
    // accepted, so the refusals above are about the invariants and not about
    // exceptions being broken.
    assert!(
        load::parse_document(
            "policy:\n  exceptions:\n    - {id: x, action: deployment.execute, effect: allow, scope: {run: r}, expires_at: \"2026-08-13T14:03:00Z\"}\n",
            Origin::Global,
        )
        .is_ok()
    );
}

// ---------------------------------------------------------------------------
// canonical serialization and the hash
// ---------------------------------------------------------------------------

#[test]
fn the_policy_hash_is_byte_identical_across_two_serializations() {
    let a = ResolvedPolicy::new(
        Some(load::parse_document(GLOBAL_YAML, Origin::Global).expect("a")),
        None,
        None,
    )
    .expect("policy");
    let b = ResolvedPolicy::new(
        Some(load::parse_document(GLOBAL_YAML_REFORMATTED, Origin::Global).expect("b")),
        None,
        None,
    )
    .expect("policy");

    let first = load::snapshot(&a);
    let second = load::snapshot(&a);
    assert_eq!(
        first.canonical_blob, second.canonical_blob,
        "two serializations of one policy must be byte-identical"
    );
    assert_eq!(first.hash, second.hash);

    let reordered = load::snapshot(&b);
    assert_eq!(
        first.canonical_blob, reordered.canonical_blob,
        "key order, whitespace, quoting and comments must not change the blob"
    );
    assert_eq!(first.hash, reordered.hash);
    assert!(first.hash.starts_with("blake3:"), "{}", first.hash);

    // Positive control: a policy that differs by one effect hashes differently.
    // Without this, a `snapshot` that returned a constant would pass.
    let different = ResolvedPolicy::new(
        Some(
            load::parse_document(
                &GLOBAL_YAML.replace("effect: require_approval", "effect: deny"),
                Origin::Global,
            )
            .expect("c"),
        ),
        None,
        None,
    )
    .expect("policy");
    assert_ne!(first.hash, load::snapshot(&different).hash);
}

#[test]
fn the_canonical_blob_round_trips_back_into_the_same_policy() {
    // The blob is not a digest input — it is what a pinned run is evaluated
    // against after the file on disk is gone. It therefore has to be complete.
    let policy = load::resolve_documents(
        Some(load::parse_document(GLOBAL_YAML, Origin::Global).expect("g")),
        Some(load::parse_document(PROJECT_PERMISSIVE, Origin::Project).expect("p")),
        None,
    )
    .expect("policy");

    let blob = load::snapshot(&policy);
    let back = load::from_canonical(&blob.canonical_blob).expect("round trip");
    let again = load::snapshot(&back);

    assert_eq!(blob.canonical_blob, again.canonical_blob);
    assert_eq!(blob.hash, again.hash);
}

#[test]
fn the_canonical_blob_has_sorted_keys_and_carries_no_timestamp_of_its_own() {
    let policy = ResolvedPolicy::new(
        Some(load::parse_document(GLOBAL_YAML, Origin::Global).expect("g")),
        None,
        None,
    )
    .expect("policy");
    let blob = load::snapshot(&policy).canonical_blob;

    let value: serde_json::Value = serde_json::from_str(&blob).expect("canonical blob is JSON");
    let keys: Vec<&String> = value
        .as_object()
        .expect("an object")
        .keys()
        .collect::<Vec<_>>();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "top-level keys must be sorted");

    for forbidden in ["created_at", "serialized_at", "loaded_at", "now"] {
        assert!(
            !blob.contains(forbidden),
            "the blob carries a timestamp ({forbidden}), so two snapshots of one \
             policy would not hash equal: {blob}"
        );
    }
}

// ---------------------------------------------------------------------------
// mechanism 4 — run-lifetime pinning
// ---------------------------------------------------------------------------

#[test]
fn a_run_evaluates_against_its_snapshot_after_the_policy_on_disk_is_tightened() {
    let (_dir, mut store) = store();
    let files = tempfile::tempdir().expect("tempdir");
    let project = write(files.path(), "policy.yaml", PROJECT_PERMISSIVE);

    let at_creation = load::resolve(None, Some(&project), None).expect("resolve");
    let pinned_hash = seed_run(&mut store, "r-0001", &at_creation, 1_000);

    // The operator edits the policy mid-run, and a *second* run is created
    // afterwards — so the store now holds two snapshots and the newer one is
    // the tightened one. This is what stops the test being vacuous: a
    // `pinned_for_run` that ignored `run.policy_hash` and reached for whatever
    // policy it could find would now find the wrong one.
    std::fs::write(&project, PROJECT_TIGHTENED).expect("tighten");
    let after_edit = load::resolve(None, Some(&project), None).expect("resolve");
    let tightened_hash = seed_run(&mut store, "r-0002", &after_edit, 2_000);
    assert_ne!(pinned_hash, tightened_hash);

    let request = Request::new(Action::parse("dependency.add.runtime"), 0);

    let pinned = load::pinned_for_run(store.conn(), &RunId::new("r-0001").expect("id"))
        .expect("the pinned snapshot must still resolve");
    assert_eq!(pinned.hash, pinned_hash, "the run keeps its policy_hash");
    assert_eq!(
        evaluate(&pinned.policy, &request).effect,
        Effect::Allow,
        "a run evaluates against its snapshot for its entire life (§4.4)"
    );

    // Positive control 1: the run created *after* the edit resolves to the new
    // policy. So `pinned_for_run` reads a per-run pin rather than returning a
    // constant, and the assertion above can fail.
    let newer =
        load::pinned_for_run(store.conn(), &RunId::new("r-0002").expect("id")).expect("resolve");
    assert_eq!(newer.hash, tightened_hash);
    assert_eq!(evaluate(&newer.policy, &request).effect, Effect::Deny);

    // Positive control 2: the file really did change on disk.
    let now_on_disk = load::resolve(None, Some(&project), None).expect("resolve");
    assert_eq!(evaluate(&now_on_disk, &request).effect, Effect::Deny);
    assert_ne!(load::snapshot(&now_on_disk).hash, pinned_hash);
}

#[test]
fn a_policy_file_deleted_mid_run_does_not_stop_the_snapshot_resolving() {
    let (_dir, mut store) = store();
    let files = tempfile::tempdir().expect("tempdir");
    let project = write(files.path(), "policy.yaml", PROJECT_TIGHTENED);

    let at_creation = load::resolve(None, Some(&project), None).expect("resolve");
    seed_run(&mut store, "r-0004", &at_creation, 1_000);

    std::fs::remove_file(&project).expect("delete the policy file");

    let pinned = load::pinned_for_run(store.conn(), &RunId::new("r-0004").expect("id"))
        .expect("the snapshot is in the store, not on disk");
    assert_eq!(
        evaluate(
            &pinned.policy,
            &Request::new(Action::parse("dependency.add.runtime"), 0)
        )
        .effect,
        Effect::Deny
    );

    // Positive control: the file is genuinely gone, so the pinned resolution
    // above cannot have come from disk.
    let err = load::resolve(None, Some(&project), None)
        .expect_err("a named-but-absent policy file is an error, not an empty policy");
    assert!(matches!(err, PolicyError::Io { .. }), "{err}");
}

#[test]
fn a_run_pinned_to_a_missing_snapshot_is_an_error_and_not_an_empty_policy() {
    // Fail closed: an unresolvable pin must never degrade into "no rules".
    let (_dir, mut store) = store();
    let at_creation = ResolvedPolicy::new(None, None, None).expect("policy");
    seed_run(&mut store, "r-0003", &at_creation, 1_000);
    // The foreign key normally makes this impossible, which is the point: the
    // only way to reach a dangling pin is a database that lost the row — a
    // partial restore, a hand-edited store, a `.dump` reloaded without the
    // table. Recreate that state rather than pretend it cannot happen.
    store
        .conn()
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DELETE FROM policy_snapshot;
             PRAGMA foreign_keys = ON;",
        )
        .expect("simulate a store that lost the snapshot row");

    let err = load::pinned_for_run(store.conn(), &RunId::new("r-0003").expect("id"))
        .expect_err("a dangling pin must be an error");
    assert!(matches!(err, PolicyError::SnapshotMissing { .. }), "{err}");
}

#[test]
fn a_mid_run_tightening_of_a_pending_action_pauses_the_run() {
    // Acceptance row 23: "edit policy mid-run → run keeps `policy_hash`; old
    // snapshot; **pause if strictly tighter**".
    let pinned = load::resolve_documents(
        None,
        Some(load::parse_document(PROJECT_PERMISSIVE, Origin::Project).expect("p")),
        None,
    )
    .expect("policy");
    let tightened = load::resolve_documents(
        None,
        Some(load::parse_document(PROJECT_TIGHTENED, Origin::Project).expect("p")),
        None,
    )
    .expect("policy");

    let pending = [Action::parse("dependency.add.runtime")];
    let pinned = load::Pinned {
        hash: load::snapshot(&pinned).hash,
        policy: pinned,
    };

    match drift(&pinned, &tightened, &pending, &Default::default(), 0) {
        DriftDecision::Pause { tightened } => {
            assert_eq!(tightened.len(), 1);
            assert_eq!(tightened[0].pinned, Effect::Allow);
            assert_eq!(tightened[0].current, Effect::Deny);
        }
        other => panic!("a strictly tighter policy must pause the run, got {other:?}"),
    }
}

#[test]
fn positive_control_a_loosened_or_unchanged_policy_does_not_pause() {
    let permissive = load::resolve_documents(
        None,
        Some(load::parse_document(PROJECT_PERMISSIVE, Origin::Project).expect("p")),
        None,
    )
    .expect("policy");
    let tightened = load::resolve_documents(
        None,
        Some(load::parse_document(PROJECT_TIGHTENED, Origin::Project).expect("p")),
        None,
    )
    .expect("policy");
    let pending = [Action::parse("dependency.add.runtime")];

    // Loosened: the pinned policy denies, the new one allows. A run must not
    // pause for being given *more* room — and it must not adopt the loosening
    // either, which the pinning test above already covers.
    let pinned = load::Pinned {
        hash: load::snapshot(&tightened).hash,
        policy: tightened.clone(),
    };
    assert!(matches!(
        drift(&pinned, &permissive, &pending, &Default::default(), 0),
        DriftDecision::Proceed
    ));

    // Unchanged.
    assert!(matches!(
        drift(&pinned, &tightened, &pending, &Default::default(), 0),
        DriftDecision::Proceed
    ));

    // And a tightening of an action that is not pending does not pause either —
    // §4.4 scopes the exception to "a pending action".
    assert!(matches!(
        drift(
            &load::Pinned {
                hash: load::snapshot(&permissive).hash,
                policy: permissive,
            },
            &tightened,
            &[Action::parse("git.commit.local")],
            &Default::default(),
            0
        ),
        DriftDecision::Proceed
    ));
}

// ---------------------------------------------------------------------------
// mechanism 5 — the §4.2 eligibility gate
// ---------------------------------------------------------------------------

fn requirements() -> ExecutionRequirements {
    // §4.2's own example.
    ExecutionRequirements::parse_yaml(
        "execution_requirements:\n  filesystem_write: restricted\n  control_surface:  hard\n",
    )
    .expect("§4.2's example must parse")
}

fn measured_codex() -> ExecutionCapabilities {
    ExecutionCapabilities {
        filesystem_write: Enforcement::Restricted,
        network_egress: Enforcement::Hard,
        control_surface: Enforcement::Hard,
        credential_read: Enforcement::None,
        tool_interception: Informational::new(Enforcement::None),
        exceptions: vec!["/tmp".into()],
    }
}

fn probe_key() -> ProbeKey {
    ProbeKey::new("codex", "0.142.0", "codex-sandbox", "0.142.0", "macOS 26.6")
}

#[test]
fn a_measured_host_that_meets_every_requirement_is_eligible() {
    let (_dir, mut store) = store();
    cache::upsert(store.conn_mut(), &probe_key(), &measured_codex(), 1).expect("upsert");
    let lookup = cache::lookup(store.conn(), &probe_key()).expect("lookup");

    match eligibility::check(&lookup, &requirements()) {
        Eligibility::Eligible { measured, probe } => {
            assert_eq!(measured, measured_codex());
            assert!(matches!(probe, ProbeStatus::Measured { .. }));
        }
        other => panic!("expected eligible, got {other:?}"),
    }
}

#[test]
fn measured_capabilities_below_a_requirement_refuse_and_name_the_dimension() {
    let (_dir, mut store) = store();
    let bare = ExecutionCapabilities {
        control_surface: Enforcement::None,
        ..measured_codex()
    };
    cache::upsert(store.conn_mut(), &probe_key(), &bare, 1).expect("upsert");
    let lookup = cache::lookup(store.conn(), &probe_key()).expect("lookup");

    match eligibility::check(&lookup, &requirements()) {
        Eligibility::Refused {
            shortfalls, offers, ..
        } => {
            assert_eq!(shortfalls.len(), 1);
            assert_eq!(shortfalls[0].dimension, GatingDimension::ControlSurface);
            assert_eq!(shortfalls[0].required, Enforcement::Hard);
            assert_eq!(shortfalls[0].measured, Enforcement::None);
            // §4.2: "offer: attended mode | different adapter | a sandbox launcher".
            assert_eq!(offers.len(), 3);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_stale_or_absent_probe_refuses_every_declared_requirement() {
    // §4.2 and CLAUDE.md: "measured per (adapter × launcher) on the host, cached
    // by version, and **fail closed when stale**".
    let (_dir, mut store) = store();
    cache::upsert(store.conn_mut(), &probe_key(), &measured_codex(), 1).expect("upsert");

    // The launcher is upgraded. Same host, same adapter, different version — the
    // row no longer describes what will run.
    let stale = ProbeKey::new("codex", "0.142.0", "codex-sandbox", "0.143.0", "macOS 26.6");
    let lookup = cache::lookup(store.conn(), &stale).expect("lookup");
    assert!(matches!(lookup, CacheLookup::Miss), "{lookup:?}");

    match eligibility::check(&lookup, &requirements()) {
        Eligibility::Refused {
            shortfalls, probe, ..
        } => {
            assert!(matches!(probe, ProbeStatus::Absent));
            assert_eq!(
                shortfalls.len(),
                2,
                "an unmeasured host satisfies nothing: {shortfalls:?}"
            );
            assert!(
                shortfalls.iter().all(|s| s.measured == Enforcement::None),
                "{shortfalls:?}"
            );
        }
        other => panic!("a stale probe must refuse, got {other:?}"),
    }
}

#[test]
fn positive_control_the_same_requirements_pass_against_the_fresh_row() {
    // The stale test above would pass if `check` refused unconditionally. This
    // one proves the requirements are satisfiable and that the version triple is
    // the only thing that changed.
    let (_dir, mut store) = store();
    cache::upsert(store.conn_mut(), &probe_key(), &measured_codex(), 1).expect("upsert");
    let fresh = cache::lookup(store.conn(), &probe_key()).expect("lookup");
    assert!(matches!(
        eligibility::check(&fresh, &requirements()),
        Eligibility::Eligible { .. }
    ));
}

#[test]
fn an_unreadable_probe_row_refuses_exactly_as_an_absent_one_does() {
    let (_dir, mut store) = store();
    cache::upsert(store.conn_mut(), &probe_key(), &measured_codex(), 1).expect("upsert");
    store
        .conn()
        .execute(
            "UPDATE containment_probe SET capabilities = '{\"nope\":1}'",
            [],
        )
        .expect("corrupt the row");

    let lookup = cache::lookup(store.conn(), &probe_key()).expect("lookup");
    match eligibility::check(&lookup, &requirements()) {
        Eligibility::Refused {
            probe, shortfalls, ..
        } => {
            assert!(matches!(probe, ProbeStatus::Unusable { .. }));
            assert_eq!(shortfalls.len(), 2);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn tool_interception_can_never_be_named_as_a_requirement() {
    // §4.2/§6.3: hooks are reported, never gating. The requirement map is keyed
    // by `GatingDimension`, which has no variant for it — so the only way to try
    // is through the YAML, and the loader refuses by name.
    let err = ExecutionRequirements::parse_yaml(
        "execution_requirements:\n  tool_interception: restricted\n",
    )
    .expect_err("tool_interception must never satisfy a gating requirement");
    let message = err.to_string();
    assert!(message.contains("tool_interception"), "{message}");
    assert!(
        message.contains("never gates") || message.contains("informational"),
        "the refusal must say why: {message}"
    );

    // And an unrecognised dimension is refused too, rather than ignored — an
    // ignored requirement is a requirement that silently does not apply.
    assert!(
        ExecutionRequirements::parse_yaml("execution_requirements:\n  filesystem_writes: hard\n")
            .is_err()
    );

    // Positive control: the four real dimensions all parse.
    for dimension in GatingDimension::ALL {
        let yaml = format!("execution_requirements:\n  {dimension}: hard\n");
        let parsed = ExecutionRequirements::parse_yaml(&yaml)
            .unwrap_or_else(|e| panic!("{dimension} must parse: {e}"));
        assert_eq!(parsed.get(*dimension), Some(Enforcement::Hard));
    }
}

#[test]
fn a_requirement_of_none_is_satisfied_by_anything_including_an_unmeasured_host() {
    // §4.2's rule is `required > measured`, not `not measured`. A requirement of
    // `none` requires nothing, and pretending otherwise would be inventing
    // policy the plan does not state.
    let requirements =
        ExecutionRequirements::parse_yaml("execution_requirements:\n  network_egress: none\n")
            .expect("parse");
    assert!(matches!(
        eligibility::check(&CacheLookup::Miss, &requirements),
        Eligibility::Eligible { .. }
    ));
}
