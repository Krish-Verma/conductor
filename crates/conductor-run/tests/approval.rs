//! S8 — approvals: the four kinds, `binding_hash`, TTL, one-shot, revocation.
//!
//! Master plan §4.3. Every refusal here is paired with a **positive control**
//! that passes under the mutation the refusal is supposed to catch — without
//! one, "a grant for A does not satisfy B" may be asserting nothing more than
//! that the fixture never had a matching grant at all. That is the exact trap
//! S7 hit and the reason S2, S5, S6 and S7 each shipped a vacuous test.

use conductor_core::effect::{OperationId, Precondition, SideEffectKind};
use conductor_core::{Fence, RunId, TaskId};
use conductor_run::approval::binding::Binding;
use conductor_run::approval::kind::{ApprovalKind, Expiry, ExpiryRule, Subject};
use conductor_run::approval::revoke::{InFlight, RevocationOutcome};
use conductor_run::approval::store::{
    ApprovalError, GrantOptions, GrantState, NewApprovalRequest, RequestState,
};
use conductor_run::approval::{Authorization, Consumption, Refusal};
use conductor_run::approval::{authorize, revoke, store as approvals};
use conductor_run::policy::evaluate::{Request, evaluate};
use conductor_run::policy::load;
use conductor_run::policy::model::{Action, Effect, Fact, FactSet, Origin, ResolvedPolicy, Scope};
use conductor_store::{NewRun, NewTask, Store};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A policy that gates `dependency.add.runtime` and `deployment.execute` behind
/// a human, and denies `git.force_push` outright.
const GATING_YAML: &str = r#"
policy:
  rules:
    - id: global.no-force-push
      action: git.force_push
      effect: deny
      locked: true
    - id: global.runtime-dependency
      action: dependency.add.runtime
      effect: require_approval
    - id: global.deploy
      action: deployment.execute
      effect: require_approval
"#;

/// The same policy with one rule tightened. Used to prove a grant stops
/// applying when the snapshot changes.
const GATING_YAML_EDITED: &str = r#"
policy:
  rules:
    - id: global.no-force-push
      action: git.force_push
      effect: deny
      locked: true
    - id: global.runtime-dependency
      action: dependency.add.runtime
      effect: require_approval
    - id: global.deploy
      action: deployment.execute
      effect: require_approval
    - id: global.lockfiles
      action: lockfile.modify
      effect: require_approval
"#;

const RUN: &str = "r-0041";

fn policy(yaml: &str) -> ResolvedPolicy {
    let document = load::parse_document(yaml, Origin::Global).expect("parse");
    load::resolve_documents(Some(document), None, None).expect("resolve")
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open_or_create(dir.path().join("conductor.db")).expect("open store");
    (dir, store)
}

/// A run pinned to `policy`, with the parent rows the schema requires.
fn seed_run(store: &mut Store, run_id: &str, policy: &ResolvedPolicy) -> String {
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

    let task_id = TaskId::new(format!("T-{run_id}")).expect("task id");
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

/// The facts §4.3's worked example carries.
fn dependency_facts(name: &str) -> FactSet {
    let mut facts = FactSet::new();
    facts.push(Fact::deterministic("dependency", name));
    facts.push(Fact::deterministic("manifest", "Cargo.toml"));
    facts
}

fn run_scope() -> Scope {
    Scope::from_pairs([("run".to_string(), RUN.to_string())])
}

/// Evaluate one gated action against `policy`, as the runtime would.
fn decision(
    policy: &ResolvedPolicy,
    action: &str,
    facts: FactSet,
) -> conductor_run::policy::evaluate::Decision {
    let request = Request::new(Action::parse(action), 1_000)
        .with_facts(facts)
        .with_context("run", RUN);
    evaluate(policy, &request)
}

/// A `REQUESTED` policy approval for one gated action, as §4.4's
/// `require_approval` produces.
fn policy_request(
    store: &mut Store,
    id: &str,
    decision: &conductor_run::policy::evaluate::Decision,
    expires_at_ms: i64,
) -> String {
    let request = NewApprovalRequest {
        id: id.to_string(),
        subject: Subject::PolicyAction {
            action: decision.action.clone(),
        },
        run_id: Some(RunId::new(RUN).expect("run id")),
        facts: decision.facts.iter().cloned().collect(),
        policy_hash: decision.policy_hash.clone(),
        matched_rules: decision.matched.iter().map(|m| m.rule_id.clone()).collect(),
        explanation: "adds a runtime dependency not present at base commit".to_string(),
        evidence_ref: None,
        expires: Expiry::At(expires_at_ms),
    };
    approvals::request(store.conn_mut(), &request, 500)
        .expect("record the request")
        .id
}

/// Grant a request at the run's scope.
fn grant(store: &mut Store, id: &str, request_id: &str, reuse: bool, expires: Expiry) -> String {
    approvals::grant(
        store.conn_mut(),
        request_id,
        &GrantOptions {
            id: id.to_string(),
            scope: run_scope(),
            reuse,
            expires,
            granted_by: "krish".to_string(),
            channel: "unix-socket".to_string(),
            nonce_hash: None,
        },
        600,
    )
    .expect("grant")
    .id
}

// ---------------------------------------------------------------------------
// the four kinds are structurally distinct (§4.3 lines 569-578)
// ---------------------------------------------------------------------------

#[test]
fn the_four_kinds_of_4_3_all_exist_and_are_distinct() {
    assert_eq!(ApprovalKind::ALL.len(), 4);
    let names: Vec<&str> = ApprovalKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "PLAN_APPROVAL",
            "POLICY_APPROVAL",
            "POLICY_EXCEPTION",
            "REVIEW_ACCEPTANCE"
        ]
    );
    for kind in ApprovalKind::ALL {
        assert_eq!(ApprovalKind::parse(kind.as_str()), Some(*kind));
    }
}

#[test]
fn expiry_is_a_property_of_the_kind_not_a_free_choice() {
    // §4.3's fourth column: plan approval and review acceptance do not expire;
    // a policy exception's expiry is mandatory.
    assert_eq!(ApprovalKind::Plan.expiry_rule(), ExpiryRule::Forbidden);
    assert_eq!(ApprovalKind::Policy.expiry_rule(), ExpiryRule::Mandatory);
    assert_eq!(
        ApprovalKind::PolicyException.expiry_rule(),
        ExpiryRule::Mandatory
    );
    assert_eq!(
        ApprovalKind::ReviewAcceptance.expiry_rule(),
        ExpiryRule::Forbidden
    );
}

#[test]
fn a_kind_cannot_disagree_with_what_it_authorizes() {
    // The kind is *derived* from the subject, so there is no way to build a
    // request whose kind and subject describe different things. Collapsing the
    // four into `approved: bool` is not representable.
    assert_eq!(
        Subject::PlanVersion {
            plan_version_id: "pv-4".to_string()
        }
        .kind(),
        ApprovalKind::Plan
    );
    assert_eq!(
        Subject::PolicyAction {
            action: Action::parse("deployment.execute")
        }
        .kind(),
        ApprovalKind::Policy
    );
    assert_eq!(
        Subject::Rule {
            rule_id: "project.deps".to_string(),
            requested: Effect::Allow
        }
        .kind(),
        ApprovalKind::PolicyException
    );
    assert_eq!(
        Subject::ReviewPacket {
            packet_id: "RP-9".to_string()
        }
        .kind(),
        ApprovalKind::ReviewAcceptance
    );
}

#[test]
fn a_plan_approval_does_not_satisfy_a_deployment_gate() {
    // §4.3: "Collapsing them would let a plan approval satisfy a deployment
    // gate."
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    // A plan approval, granted and unexpired.
    let plan_request = NewApprovalRequest {
        id: "AR-plan".to_string(),
        subject: Subject::PlanVersion {
            plan_version_id: "pv-1".to_string(),
        },
        run_id: None,
        facts: FactSet::new(),
        policy_hash: load::snapshot(&policy).hash,
        matched_rules: Vec::new(),
        explanation: "plan v1 becomes authoritative".to_string(),
        evidence_ref: None,
        expires: Expiry::Never,
    };
    approvals::request(store.conn_mut(), &plan_request, 500).expect("record");
    grant(&mut store, "AG-plan", "AR-plan", false, Expiry::Never);

    let deploy = decision(&policy, "deployment.execute", FactSet::new());
    assert_eq!(deploy.effect, Effect::RequireApproval);

    match authorize::authorize(store.conn(), &deploy, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(Refusal::NoMatchingGrant { .. }) => {}
        other => panic!("a plan approval must not satisfy a deployment gate: {other:?}"),
    }

    // POSITIVE CONTROL: the same call succeeds once a *policy* approval for
    // this exact action exists. Without this, the refusal above could simply be
    // a fixture with no usable grant at all.
    let deploy_request = policy_request(&mut store, "AR-deploy", &deploy, 10_000);
    grant(
        &mut store,
        "AG-deploy",
        &deploy_request,
        false,
        Expiry::At(10_000),
    );
    match authorize::authorize(store.conn(), &deploy, &run_scope(), 700).expect("authorize") {
        Authorization::Authorized { grant_id } => assert_eq!(grant_id, "AG-deploy"),
        other => panic!("positive control must authorize: {other:?}"),
    }
}

#[test]
fn the_binding_of_two_kinds_over_identical_material_still_differs() {
    // The anti-collapse property is in the hash, not only in a `kind` column
    // comparison: two approvals over the same policy hash and scope bind
    // differently because the kind is a domain separator in the preimage.
    let plan = Binding {
        subject: Subject::PlanVersion {
            plan_version_id: "deployment.execute".to_string(),
        },
        facts: FactSet::new(),
        policy_hash: "blake3:p".to_string(),
        scope: run_scope(),
    };
    let policy = Binding {
        subject: Subject::PolicyAction {
            action: Action::parse("deployment.execute"),
        },
        facts: FactSet::new(),
        policy_hash: "blake3:p".to_string(),
        scope: run_scope(),
    };
    assert_ne!(plan.hash(), policy.hash());
    // POSITIVE CONTROL: identical bindings hash identically, so the assertion
    // above is about the kind and not about the hash being unstable.
    assert_eq!(policy.hash(), policy.hash());
}

// ---------------------------------------------------------------------------
// binding_hash — the scoping mechanism (§4.3 line 558)
// ---------------------------------------------------------------------------

#[test]
fn a_grant_for_one_action_does_not_satisfy_another() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(
        &policy,
        "dependency.add.runtime",
        dependency_facts("serde_yaml"),
    );
    let request_id = policy_request(&mut store, "AR-0031", &dependency, 10_000);
    grant(
        &mut store,
        "AG-0019",
        &request_id,
        false,
        Expiry::At(10_000),
    );

    // §4.3: "cannot authorize `deployment.execute` (different action)".
    let deploy = decision(&policy, "deployment.execute", FactSet::new());
    match authorize::authorize(store.conn(), &deploy, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(Refusal::NoMatchingGrant { .. }) => {}
        other => panic!("a dependency grant must not authorize a deployment: {other:?}"),
    }

    // POSITIVE CONTROL: the same grant *does* satisfy the action it was issued
    // for. This is what proves the refusal above is the binding and not an
    // empty fixture.
    match authorize::authorize(store.conn(), &dependency, &run_scope(), 700).expect("authorize") {
        Authorization::Authorized { grant_id } => assert_eq!(grant_id, "AG-0019"),
        other => panic!("positive control must authorize: {other:?}"),
    }
}

#[test]
fn a_grant_for_dep_foo_does_not_satisfy_dep_bar() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let foo = decision(&policy, "dependency.add.runtime", dependency_facts("foo"));
    let request_id = policy_request(&mut store, "AR-foo", &foo, 10_000);
    grant(&mut store, "AG-foo", &request_id, false, Expiry::At(10_000));

    let bar = decision(&policy, "dependency.add.runtime", dependency_facts("bar"));
    match authorize::authorize(store.conn(), &bar, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(Refusal::NoMatchingGrant { .. }) => {}
        other => panic!("dep:foo must not authorize dep:bar: {other:?}"),
    }

    // POSITIVE CONTROL.
    match authorize::authorize(store.conn(), &foo, &run_scope(), 700).expect("authorize") {
        Authorization::Authorized { grant_id } => assert_eq!(grant_id, "AG-foo"),
        other => panic!("positive control must authorize: {other:?}"),
    }
}

#[test]
fn a_grant_stops_applying_when_the_policy_snapshot_changes() {
    // §4.3: "and stops applying if the policy snapshot changed — which is
    // correct, not inconvenient."
    let (_dir, mut store) = store();
    let before = policy(GATING_YAML);
    seed_run(&mut store, RUN, &before);

    let under_before = decision(
        &before,
        "dependency.add.runtime",
        dependency_facts("serde_yaml"),
    );
    let request_id = policy_request(&mut store, "AR-snap", &under_before, 10_000);
    grant(
        &mut store,
        "AG-snap",
        &request_id,
        false,
        Expiry::At(10_000),
    );

    let after = policy(GATING_YAML_EDITED);
    assert_ne!(
        load::snapshot(&before).hash,
        load::snapshot(&after).hash,
        "the fixture must actually change the snapshot, or this test is vacuous"
    );
    let under_after = decision(
        &after,
        "dependency.add.runtime",
        dependency_facts("serde_yaml"),
    );
    assert_eq!(
        under_after.effect,
        Effect::RequireApproval,
        "the edited policy must still gate this action, so the refusal is about \
         the snapshot and not about the action ceasing to be gated"
    );

    match authorize::authorize(store.conn(), &under_after, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(Refusal::NoMatchingGrant { .. }) => {}
        other => panic!("a grant must not survive a policy snapshot change: {other:?}"),
    }

    // POSITIVE CONTROL: under the snapshot it was issued against, it applies.
    match authorize::authorize(store.conn(), &under_before, &run_scope(), 700).expect("authorize") {
        Authorization::Authorized { grant_id } => assert_eq!(grant_id, "AG-snap"),
        other => panic!("positive control must authorize: {other:?}"),
    }
}

#[test]
fn a_grant_scoped_to_one_run_does_not_authorize_another() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let request_id = policy_request(&mut store, "AR-scope", &dependency, 10_000);
    grant(
        &mut store,
        "AG-scope",
        &request_id,
        false,
        Expiry::At(10_000),
    );

    let other = Scope::from_pairs([("run".to_string(), "r-9999".to_string())]);
    match authorize::authorize(store.conn(), &dependency, &other, 700).expect("authorize") {
        Authorization::Refused(Refusal::NoMatchingGrant { .. }) => {}
        other => panic!("a run-scoped grant must not cross runs: {other:?}"),
    }

    // POSITIVE CONTROL.
    match authorize::authorize(store.conn(), &dependency, &run_scope(), 700).expect("authorize") {
        Authorization::Authorized { .. } => {}
        other => panic!("positive control must authorize: {other:?}"),
    }
}

#[test]
fn the_binding_is_recomputed_at_use_time_and_not_read_back_from_the_row() {
    // A stored binding that disagrees with its own inputs — a hand edit, a
    // partial write — must not authorize anything. Same doctrine as schema v5's
    // `fingerprint`: the derived digest is evidence for a human, never an input
    // to a decision.
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let request_id = policy_request(&mut store, "AR-tamper", &dependency, 10_000);
    grant(
        &mut store,
        "AG-tamper",
        &request_id,
        false,
        Expiry::At(10_000),
    );

    // POSITIVE CONTROL, added at S8 review. Before tampering, the very same
    // grant must authorize the very same operation. Without this the test would
    // survive an `authorize` that refused *everything* — it asserts only a
    // refusal, and a function that never authorizes anything satisfies it
    // trivially. That is exactly what the S8 audit mutation demonstrated: with
    // `authorize` hard-wired to refuse, nine tests failed and this one passed.
    match authorize::authorize(store.conn(), &dependency, &run_scope(), 699).expect("authorize") {
        Authorization::Authorized { grant_id } => assert_eq!(grant_id, "AG-tamper"),
        other => panic!("the untampered grant must authorize its own operation: {other:?}"),
    }

    // Tamper: point the grant at a binding nothing will ever recompute.
    store
        .conn()
        .execute(
            "UPDATE approval_grant SET binding_hash = 'blake3:tampered' WHERE id = 'AG-tamper'",
            [],
        )
        .expect("tamper");

    match authorize::authorize(store.conn(), &dependency, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(Refusal::NoMatchingGrant { .. }) => {}
        other => panic!("a tampered binding must authorize nothing: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// ADR-0010 carry-forward — a grant is not a cheaper path around a capped deny
// ---------------------------------------------------------------------------

const CAPPABLE_DENY_YAML: &str = r#"
policy:
  rules:
    - id: global.no-arch-change
      action: architecture.change
      effect: deny
      when: [architecture_proxy]
"#;

#[test]
fn a_grant_can_never_satisfy_a_deny_so_the_uncapped_rule_is_not_the_expensive_one() {
    // ADR-0010's revisit trigger 3: "S8 granting approvals in a way that lets a
    // capped deny be satisfied more cheaply than an uncapped one."
    //
    // The capped path (model-assisted fact → `require_approval`) is satisfiable
    // by one human grant. The uncapped path (deterministic fact → `deny`) is
    // satisfiable by **no** grant at all. A grant that could clear a `deny`
    // would make the deterministic rule the weaker of the two, which is the
    // inversion the ADR forbids.
    let (_dir, mut store) = store();
    let policy = policy(CAPPABLE_DENY_YAML);
    seed_run(&mut store, RUN, &policy);

    let mut capped_facts = FactSet::new();
    capped_facts.push(Fact::model_assisted("architecture_proxy", "src/**"));
    let capped = decision(&policy, "architecture.change", capped_facts);
    assert_eq!(
        capped.effect,
        Effect::RequireApproval,
        "ADR-0010: a deny resting on a model-assisted fact is capped"
    );

    let mut uncapped_facts = FactSet::new();
    uncapped_facts.push(Fact::deterministic("architecture_proxy", "src/**"));
    let uncapped = decision(&policy, "architecture.change", uncapped_facts);
    assert_eq!(uncapped.effect, Effect::Deny);

    // The human grants the capped one. That is the whole point of the cap.
    let request_id = policy_request(&mut store, "AR-capped", &capped, 10_000);
    grant(
        &mut store,
        "AG-capped",
        &request_id,
        false,
        Expiry::At(10_000),
    );
    match authorize::authorize(store.conn(), &capped, &run_scope(), 700).expect("authorize") {
        Authorization::Authorized { grant_id } => assert_eq!(grant_id, "AG-capped"),
        other => panic!("the capped path is a human decision, and must work: {other:?}"),
    }

    // The same operation with a deterministic fact is a `deny`, and no grant
    // reaches it — not this one, and not one issued for it directly.
    match authorize::authorize(store.conn(), &uncapped, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(Refusal::DenyIsNotApprovable { .. }) => {}
        other => panic!("a deny must not be approvable: {other:?}"),
    }

    // And issuing a grant *for the deny itself* changes nothing: the refusal is
    // about the effect, not about the absence of a matching grant.
    let deny_request = policy_request(&mut store, "AR-uncapped", &uncapped, 10_000);
    grant(
        &mut store,
        "AG-uncapped",
        &deny_request,
        false,
        Expiry::At(10_000),
    );
    match authorize::authorize(store.conn(), &uncapped, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(Refusal::DenyIsNotApprovable { .. }) => {}
        other => panic!("a grant must not dissolve a deny: {other:?}"),
    }
}

#[test]
fn a_grant_issued_under_a_capped_deny_does_not_carry_over_to_the_deterministic_one() {
    // The second half of ADR-0010's carry-forward: the fact *sources* are in
    // the binding preimage, so a grant obtained while the evidence was
    // model-assisted cannot be replayed once the same evidence is
    // deterministic. Without this, a model-assisted observation would be a
    // cheap route to authorizing the deterministic case.
    let capped = Binding {
        subject: Subject::PolicyAction {
            action: Action::parse("architecture.change"),
        },
        facts: [Fact::model_assisted("architecture_proxy", "src/**")]
            .into_iter()
            .collect(),
        policy_hash: "blake3:p".to_string(),
        scope: run_scope(),
    };
    let deterministic = Binding {
        subject: Subject::PolicyAction {
            action: Action::parse("architecture.change"),
        },
        facts: [Fact::deterministic("architecture_proxy", "src/**")]
            .into_iter()
            .collect(),
        policy_hash: "blake3:p".to_string(),
        scope: run_scope(),
    };
    assert_ne!(capped.hash(), deterministic.hash());
    // POSITIVE CONTROL: the same source hashes the same, so the difference
    // above is the source and not hash instability.
    assert_eq!(capped.hash(), capped.hash());
}

#[test]
fn an_action_that_needs_no_approval_is_not_authorized_by_a_grant_either() {
    // A grant answers a `require_approval`. Offering it for an `allow` would
    // mean the code path that consumes grants runs where no gate exists, and a
    // consumed one-shot grant would then be gone when the real gate arrives.
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let ungated = decision(&policy, "git.commit.local", FactSet::new());
    assert_eq!(ungated.effect, Effect::Allow);
    match authorize::authorize(store.conn(), &ungated, &run_scope(), 700).expect("authorize") {
        Authorization::Refused(Refusal::NotGated { effect }) => assert_eq!(effect, Effect::Allow),
        other => panic!("an ungated action needs no grant: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// lifecycle: TTL, one-shot vs reuse, no double consume
// ---------------------------------------------------------------------------

#[test]
fn a_grant_past_its_ttl_authorizes_nothing() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let request_id = policy_request(&mut store, "AR-ttl", &dependency, 10_000);
    grant(&mut store, "AG-ttl", &request_id, false, Expiry::At(1_000));

    match authorize::authorize(store.conn(), &dependency, &run_scope(), 1_001).expect("authorize") {
        Authorization::Refused(Refusal::Expired { .. }) => {}
        other => panic!("an expired grant must authorize nothing: {other:?}"),
    }

    // POSITIVE CONTROL: one millisecond earlier the same grant is good.
    match authorize::authorize(store.conn(), &dependency, &run_scope(), 999).expect("authorize") {
        Authorization::Authorized { grant_id } => assert_eq!(grant_id, "AG-ttl"),
        other => panic!("positive control must authorize: {other:?}"),
    }
}

#[test]
fn expiry_sweeps_requests_and_grants_onto_their_terminal_states() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let waiting = policy_request(&mut store, "AR-waiting", &dependency, 1_000);
    let granted_request = policy_request(&mut store, "AR-granted", &dependency, 10_000);
    grant(
        &mut store,
        "AG-granted",
        &granted_request,
        false,
        Expiry::At(1_000),
    );

    let swept = approvals::expire(store.conn_mut(), 1_001).expect("expire");
    assert_eq!(swept.requests, [waiting]);
    assert_eq!(swept.grants, ["AG-granted".to_string()]);

    assert_eq!(
        approvals::request_row(store.conn(), "AR-waiting")
            .expect("read")
            .expect("row")
            .state,
        RequestState::Expired
    );
    assert_eq!(
        approvals::grant_row(store.conn(), "AG-granted")
            .expect("read")
            .expect("row")
            .state,
        GrantState::Expired
    );
    // The *request* behind a granted approval is not re-expired: it left
    // `REQUESTED` when it was granted, and a request is never `CONSUMED`.
    assert_eq!(
        approvals::request_row(store.conn(), "AR-granted")
            .expect("read")
            .expect("row")
            .state,
        RequestState::Granted
    );
}

#[test]
fn a_one_shot_grant_is_never_consumed_twice() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let request_id = policy_request(&mut store, "AR-once", &dependency, 10_000);
    grant(
        &mut store,
        "AG-once",
        &request_id,
        false,
        Expiry::At(10_000),
    );
    let binding = Binding::for_decision(&dependency, &run_scope()).hash();

    match approvals::consume(store.conn_mut(), "AG-once", &binding, 700).expect("consume") {
        Consumption::Consumed { grant_id } => assert_eq!(grant_id, "AG-once"),
        other => panic!("the first consumption must succeed: {other:?}"),
    }
    match approvals::consume(store.conn_mut(), "AG-once", &binding, 701).expect("consume") {
        Consumption::Refused(Refusal::AlreadyConsumed { grant_id }) => {
            assert_eq!(grant_id, "AG-once")
        }
        other => panic!("no grant is consumed twice: {other:?}"),
    }
    assert_eq!(
        approvals::grant_row(store.conn(), "AG-once")
            .expect("read")
            .expect("row")
            .state,
        GrantState::Consumed
    );
    // And it no longer authorizes anything.
    //
    // The refusal is `AlreadyConsumed`, not `NoMatchingGrant`: the grant *is*
    // found — its `binding_hash` still matches, which is the whole point of the
    // scoping — and is refused on its state. Reporting "no matching grant" here
    // would tell an operator they never had one, sending them to write a new
    // policy rule for an action that was in fact approved and spent. Same
    // distinction §4.6 draws between `IdenticalFingerprint` and
    // `BudgetExhausted`: both stop the work, and they send a person to different
    // places.
    match authorize::authorize(store.conn(), &dependency, &run_scope(), 702).expect("authorize") {
        Authorization::Refused(Refusal::AlreadyConsumed { grant_id }) => {
            assert_eq!(grant_id, "AG-once")
        }
        other => panic!("a consumed grant authorizes nothing: {other:?}"),
    }
}

#[test]
fn reuse_is_opt_in_and_survives_consumption() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let request_id = policy_request(&mut store, "AR-reuse", &dependency, 10_000);
    grant(
        &mut store,
        "AG-reuse",
        &request_id,
        true,
        Expiry::At(10_000),
    );
    let binding = Binding::for_decision(&dependency, &run_scope()).hash();

    for tick in 0..3 {
        match approvals::consume(store.conn_mut(), "AG-reuse", &binding, 700 + tick)
            .expect("consume")
        {
            Consumption::Reusable { grant_id } => assert_eq!(grant_id, "AG-reuse"),
            other => panic!("a reusable grant stays usable: {other:?}"),
        }
    }
    assert_eq!(
        approvals::grant_row(store.conn(), "AG-reuse")
            .expect("read")
            .expect("row")
            .state,
        GrantState::Granted
    );
    // Reuse does not outlive the TTL.
    match approvals::consume(store.conn_mut(), "AG-reuse", &binding, 10_001).expect("consume") {
        Consumption::Refused(Refusal::Expired { .. }) => {}
        other => panic!("reuse must not outlive the TTL: {other:?}"),
    }
}

#[test]
fn consumption_checks_the_binding_immediately_before_the_side_effect() {
    // §4.3: "consumption checked immediately before the side effect". Handing
    // `consume` a binding the grant was not issued for must refuse, or the
    // recomputation at authorize time would be advisory.
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let foo = decision(&policy, "dependency.add.runtime", dependency_facts("foo"));
    let bar = decision(&policy, "dependency.add.runtime", dependency_facts("bar"));
    let request_id = policy_request(&mut store, "AR-late", &foo, 10_000);
    grant(
        &mut store,
        "AG-late",
        &request_id,
        false,
        Expiry::At(10_000),
    );

    let wrong = Binding::for_decision(&bar, &run_scope()).hash();
    match approvals::consume(store.conn_mut(), "AG-late", &wrong, 700).expect("consume") {
        Consumption::Refused(Refusal::NoMatchingGrant { .. }) => {}
        other => panic!("consume must re-check the binding: {other:?}"),
    }
    // POSITIVE CONTROL: the right binding consumes.
    let right = Binding::for_decision(&foo, &run_scope()).hash();
    match approvals::consume(store.conn_mut(), "AG-late", &right, 700).expect("consume") {
        Consumption::Consumed { .. } => {}
        other => panic!("positive control must consume: {other:?}"),
    }
}

#[test]
fn a_request_is_never_consumed_and_a_grant_is_never_denied() {
    // §5.2 draws two machines, and they are not one chain: `REQUESTED |
    // GRANTED | DENIED | EXPIRED` for the request, `GRANTED | CONSUMED |
    // EXPIRED | REVOKED` for the grant. `GRANTED` is the join point, not a
    // transition.
    assert_eq!(RequestState::ALL.len(), 4);
    assert_eq!(GrantState::ALL.len(), 4);
    assert!(RequestState::parse("CONSUMED").is_none());
    assert!(RequestState::parse("REVOKED").is_none());
    assert!(GrantState::parse("DENIED").is_none());
    assert!(GrantState::parse("REQUESTED").is_none());
    // POSITIVE CONTROL: the states each machine does have parse.
    for state in RequestState::ALL {
        assert_eq!(RequestState::parse(state.as_str()), Some(*state));
    }
    for state in GrantState::ALL {
        assert_eq!(GrantState::parse(state.as_str()), Some(*state));
    }
}

#[test]
fn a_denied_request_produces_no_grant() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let request_id = policy_request(&mut store, "AR-deny", &dependency, 10_000);
    approvals::deny(store.conn_mut(), &request_id, "not this one", 700).expect("deny");

    assert_eq!(
        approvals::request_row(store.conn(), &request_id)
            .expect("read")
            .expect("row")
            .state,
        RequestState::Denied
    );
    // Granting a denied request is refused: the request machine has no
    // `DENIED → GRANTED` edge.
    let err = approvals::grant(
        store.conn_mut(),
        &request_id,
        &GrantOptions {
            id: "AG-deny".to_string(),
            scope: run_scope(),
            reuse: false,
            expires: Expiry::At(10_000),
            granted_by: "krish".to_string(),
            channel: "unix-socket".to_string(),
            nonce_hash: None,
        },
        800,
    )
    .expect_err("a denied request must not be grantable");
    assert!(
        matches!(err, ApprovalError::NotRequested { .. }),
        "unexpected error: {err:?}"
    );
    match authorize::authorize(store.conn(), &dependency, &run_scope(), 900).expect("authorize") {
        Authorization::Refused(Refusal::NoMatchingGrant { .. }) => {}
        other => panic!("a denied request authorizes nothing: {other:?}"),
    }
}

#[test]
fn a_kind_whose_expiry_is_forbidden_cannot_be_given_a_ttl() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let bad = NewApprovalRequest {
        id: "AR-badplan".to_string(),
        subject: Subject::PlanVersion {
            plan_version_id: "pv-1".to_string(),
        },
        run_id: None,
        facts: FactSet::new(),
        policy_hash: "blake3:p".to_string(),
        matched_rules: Vec::new(),
        explanation: "plan v1".to_string(),
        evidence_ref: None,
        expires: Expiry::At(10_000),
    };
    let err = approvals::request(store.conn_mut(), &bad, 500)
        .expect_err("§4.3: a plan approval does not expire");
    assert!(
        matches!(err, ApprovalError::Expiry { .. }),
        "unexpected error: {err:?}"
    );

    // POSITIVE CONTROL: the same request with no expiry is accepted, so the
    // refusal is about the TTL and not about plan approvals being unbuildable.
    let good = NewApprovalRequest {
        expires: Expiry::Never,
        ..bad
    };
    approvals::request(store.conn_mut(), &good, 500).expect("a plan approval with no TTL");
}

#[test]
fn a_kind_whose_expiry_is_mandatory_cannot_be_perpetual() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    seed_run(&mut store, RUN, &policy);

    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let bad = NewApprovalRequest {
        id: "AR-perpetual".to_string(),
        subject: Subject::PolicyAction {
            action: dependency.action.clone(),
        },
        run_id: Some(RunId::new(RUN).expect("run id")),
        facts: dependency.facts.iter().cloned().collect(),
        policy_hash: dependency.policy_hash.clone(),
        matched_rules: Vec::new(),
        explanation: "adds a dependency".to_string(),
        evidence_ref: None,
        expires: Expiry::Never,
    };
    let err = approvals::request(store.conn_mut(), &bad, 500)
        .expect_err("§4.3: a policy approval expires");
    assert!(
        matches!(err, ApprovalError::Expiry { .. }),
        "unexpected error: {err:?}"
    );

    // POSITIVE CONTROL.
    let good = NewApprovalRequest {
        expires: Expiry::At(10_000),
        ..bad
    };
    approvals::request(store.conn_mut(), &good, 500).expect("a policy approval with a TTL");
}

// ---------------------------------------------------------------------------
// persistence across restart (acceptance row 12)
// ---------------------------------------------------------------------------

#[test]
fn a_wait_and_its_ttl_survive_reopening_the_store() {
    // Acceptance row 12: "Crash during approval wait → request `REQUESTED`,
    // wait restored, TTL preserved."
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    let policy = policy(GATING_YAML);
    let expires_at = 1_786_629_780_000;

    {
        let mut store = Store::open_or_create(&path).expect("open");
        seed_run(&mut store, RUN, &policy);
        let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
        policy_request(&mut store, "AR-restart", &dependency, expires_at);
    }

    let store = Store::open_existing(&path).expect("reopen");
    let row = approvals::request_row(store.conn(), "AR-restart")
        .expect("read")
        .expect("row");
    assert_eq!(row.state, RequestState::Requested);
    assert_eq!(row.expires, Expiry::At(expires_at));
    assert_eq!(row.kind, ApprovalKind::Policy);
    assert_eq!(
        approvals::pending_requests(store.conn())
            .expect("pending")
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>(),
        ["AR-restart".to_string()]
    );
}

#[test]
fn a_grant_survives_reopening_the_store_and_still_authorizes_exactly_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("conductor.db");
    let policy = policy(GATING_YAML);
    let dependency = decision(&policy, "dependency.add.runtime", dependency_facts("serde"));
    let binding = Binding::for_decision(&dependency, &run_scope()).hash();

    {
        let mut store = Store::open_or_create(&path).expect("open");
        seed_run(&mut store, RUN, &policy);
        let request_id = policy_request(&mut store, "AR-durable", &dependency, 10_000);
        grant(
            &mut store,
            "AG-durable",
            &request_id,
            false,
            Expiry::At(10_000),
        );
    }
    {
        let mut store = Store::open_existing(&path).expect("reopen");
        match approvals::consume(store.conn_mut(), "AG-durable", &binding, 700).expect("consume") {
            Consumption::Consumed { .. } => {}
            other => panic!("the grant must survive the restart: {other:?}"),
        }
    }
    {
        let mut store = Store::open_existing(&path).expect("reopen");
        match approvals::consume(store.conn_mut(), "AG-durable", &binding, 701).expect("consume") {
            Consumption::Refused(Refusal::AlreadyConsumed { .. }) => {}
            other => panic!("consumption must survive the restart too: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// revocation — §4.3's four outcomes (Scenario S, acceptance row 25)
// ---------------------------------------------------------------------------

/// The precondition the run's commit effect carries. Its content is irrelevant
/// here — revocation reasons about the ledger *state*, not about the world.
fn commit_precondition() -> Precondition {
    Precondition::CommitOnBranch {
        path: "/repo".to_string(),
        branch: "conductor/r-0041".to_string(),
        tree: "tree-1".to_string(),
        message_marker: "Conductor-Run: r-0041".to_string(),
    }
}

/// A run with a granted approval and, optionally, a ledger row for the effect
/// the grant authorizes.
fn revocation_world(
    store: &mut Store,
    policy: &ResolvedPolicy,
    grant_id: &str,
    request_id: &str,
) -> (
    conductor_run::policy::evaluate::Decision,
    Fence,
    OperationId,
) {
    seed_run(store, RUN, policy);
    let dependency = decision(policy, "dependency.add.runtime", dependency_facts("serde"));
    let recorded = policy_request(store, request_id, &dependency, 10_000);
    grant(store, grant_id, &recorded, false, Expiry::At(10_000));
    let fence = Fence::new(RunId::new(RUN).expect("run id"), 0);
    let operation = OperationId::compute(
        SideEffectKind::GitCommitLocal,
        &RunId::new(RUN).expect("run id"),
        1,
        "tree-1",
    );
    (dependency, fence, operation)
}

#[test]
fn revoking_a_grant_that_was_never_consumed_stops_the_effect_happening() {
    // §4.3 row 1: "Not yet consumed → effect never happens; run →
    // `AWAITING_APPROVAL`."
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    let (dependency, fence, _) = revocation_world(&mut store, &policy, "AG-r1", "AR-r1");

    let outcome = revoke::revoke(
        store.conn_mut(),
        &fence,
        "AG-r1",
        None,
        InFlight::No,
        800,
        "operator changed their mind",
    )
    .expect("revoke");
    assert!(
        matches!(outcome, RevocationOutcome::NotYetConsumed { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        approvals::grant_row(store.conn(), "AG-r1")
            .expect("read")
            .expect("row")
            .state,
        GrantState::Revoked
    );
    // `Revoked`, not `NoMatchingGrant` — and the assertion below on `consume`
    // already expects `Revoked`, so anything else here would have the same test
    // demanding two different names for one fact.
    match authorize::authorize(store.conn(), &dependency, &run_scope(), 900).expect("authorize") {
        Authorization::Refused(Refusal::Revoked { grant_id }) => assert_eq!(grant_id, "AG-r1"),
        other => panic!("a revoked grant authorizes nothing: {other:?}"),
    }
    // The effect never happens: consuming it is refused too.
    let binding = Binding::for_decision(&dependency, &run_scope()).hash();
    match approvals::consume(store.conn_mut(), "AG-r1", &binding, 901).expect("consume") {
        Consumption::Refused(Refusal::Revoked { .. }) => {}
        other => panic!("a revoked grant must not be consumable: {other:?}"),
    }
}

#[test]
fn revoking_an_intended_effect_that_has_not_started_aborts_it_before_it_starts() {
    // §4.3 row 2: "`INTENDED`, effect not started → aborted before starting."
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    let (_, fence, operation) = revocation_world(&mut store, &policy, "AG-r2", "AR-r2");
    conductor_store::side_effect::intend_effect(
        store.conn_mut(),
        &fence,
        &operation,
        SideEffectKind::GitCommitLocal,
        &commit_precondition(),
        750,
    )
    .expect("intend");

    let outcome = revoke::revoke(
        store.conn_mut(),
        &fence,
        "AG-r2",
        Some(&operation),
        InFlight::No,
        800,
        "operator changed their mind",
    )
    .expect("revoke");
    assert!(
        matches!(outcome, RevocationOutcome::AbortedBeforeStarting { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        approvals::grant_row(store.conn(), "AG-r2")
            .expect("read")
            .expect("row")
            .state,
        GrantState::Revoked
    );
    // The ledger row is resolved as FAILED — the effect did not happen and is
    // known not to have happened. It is never left INTENDED, which restart
    // would otherwise re-check and possibly perform.
    let row = conductor_store::side_effect::side_effect(store.conn(), &operation)
        .expect("read")
        .expect("row");
    assert_eq!(row.state, conductor_core::SideEffectState::Failed);
}

#[test]
fn revoking_an_effect_in_flight_cannot_cancel_it_and_halts_with_a_finding() {
    // §4.3 row 3: "`INTENDED`, effect in flight → **cannot be cancelled**.
    // Complete or fail it, record the receipt, halt with a finding."
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    let (_, fence, operation) = revocation_world(&mut store, &policy, "AG-r3", "AR-r3");
    conductor_store::side_effect::intend_effect(
        store.conn_mut(),
        &fence,
        &operation,
        SideEffectKind::GitCommitLocal,
        &commit_precondition(),
        750,
    )
    .expect("intend");

    let outcome = revoke::revoke(
        store.conn_mut(),
        &fence,
        "AG-r3",
        Some(&operation),
        InFlight::Yes,
        800,
        "operator changed their mind",
    )
    .expect("revoke");
    match &outcome {
        RevocationOutcome::CannotCancelInFlight { finding_id, .. } => {
            assert!(!finding_id.is_empty())
        }
        other => panic!("an in-flight effect cannot be cancelled: {other:?}"),
    }
    // The ledger row is left INTENDED: the effect is still running, and
    // recording an outcome Conductor has not observed would be a guess.
    let row = conductor_store::side_effect::side_effect(store.conn(), &operation)
        .expect("read")
        .expect("row");
    assert_eq!(row.state, conductor_core::SideEffectState::Intended);

    let findings = store
        .findings_for_run(&RunId::new(RUN).expect("run id"))
        .expect("findings");
    assert!(
        findings
            .iter()
            .any(|f| f.kind == revoke::REVOKED_WHILE_IN_FLIGHT),
        "expected a {} finding, got {findings:?}",
        revoke::REVOKED_WHILE_IN_FLIGHT
    );
}

#[test]
fn revoking_after_the_effect_is_confirmed_raises_post_hoc_revocation() {
    // §4.3 row 4: "`CONFIRMED` → cannot be undone. Record revocation, raise
    // `POST_HOC_REVOCATION` finding."
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    let (dependency, fence, operation) = revocation_world(&mut store, &policy, "AG-r4", "AR-r4");
    let binding = Binding::for_decision(&dependency, &run_scope()).hash();
    approvals::consume(store.conn_mut(), "AG-r4", &binding, 760).expect("consume");
    conductor_store::side_effect::intend_effect(
        store.conn_mut(),
        &fence,
        &operation,
        SideEffectKind::GitCommitLocal,
        &commit_precondition(),
        765,
    )
    .expect("intend");
    conductor_store::side_effect::confirm_effect(
        store.conn_mut(),
        &fence,
        &operation,
        "sha:deadbeef",
        770,
    )
    .expect("confirm");

    let outcome = revoke::revoke(
        store.conn_mut(),
        &fence,
        "AG-r4",
        Some(&operation),
        InFlight::No,
        800,
        "operator changed their mind",
    )
    .expect("revoke");
    match &outcome {
        RevocationOutcome::PostHocRevocation { finding_id, .. } => assert!(!finding_id.is_empty()),
        other => panic!("a confirmed effect cannot be undone: {other:?}"),
    }
    let findings = store
        .findings_for_run(&RunId::new(RUN).expect("run id"))
        .expect("findings");
    assert!(
        findings
            .iter()
            .any(|f| f.kind == revoke::POST_HOC_REVOCATION),
        "expected a {} finding, got {findings:?}",
        revoke::POST_HOC_REVOCATION
    );
    // §5.2 makes `CONSUMED` terminal, so the grant does not move to `REVOKED`.
    // The revocation is recorded as a finding, which never auto-resolves (§4.8)
    // — losing it in a state change is what a terminal state would do.
    assert_eq!(
        approvals::grant_row(store.conn(), "AG-r4")
            .expect("read")
            .expect("row")
            .state,
        GrantState::Consumed
    );
}

#[test]
fn the_four_revocation_outcomes_are_reached_by_four_different_worlds() {
    // The guard against a revocation implementation that returns one outcome
    // for everything: each of §4.3's four rows must be produced by a world that
    // differs only in how far the effect got.
    let outcomes = ["AG-a", "AG-b", "AG-c", "AG-d"]
        .iter()
        .zip(["AR-a", "AR-b", "AR-c", "AR-d"])
        .enumerate()
        .map(|(index, (grant_id, request_id))| {
            let (_dir, mut store) = store();
            let policy = policy(GATING_YAML);
            let (dependency, fence, operation) =
                revocation_world(&mut store, &policy, grant_id, request_id);
            let precondition = commit_precondition();
            let (operation_arg, in_flight) = match index {
                0 => (None, InFlight::No),
                1 => {
                    conductor_store::side_effect::intend_effect(
                        store.conn_mut(),
                        &fence,
                        &operation,
                        SideEffectKind::GitCommitLocal,
                        &precondition,
                        750,
                    )
                    .expect("intend");
                    (Some(&operation), InFlight::No)
                }
                2 => {
                    conductor_store::side_effect::intend_effect(
                        store.conn_mut(),
                        &fence,
                        &operation,
                        SideEffectKind::GitCommitLocal,
                        &precondition,
                        750,
                    )
                    .expect("intend");
                    (Some(&operation), InFlight::Yes)
                }
                _ => {
                    let binding = Binding::for_decision(&dependency, &run_scope()).hash();
                    approvals::consume(store.conn_mut(), grant_id, &binding, 755).expect("consume");
                    conductor_store::side_effect::intend_effect(
                        store.conn_mut(),
                        &fence,
                        &operation,
                        SideEffectKind::GitCommitLocal,
                        &precondition,
                        750,
                    )
                    .expect("intend");
                    conductor_store::side_effect::confirm_effect(
                        store.conn_mut(),
                        &fence,
                        &operation,
                        "sha:deadbeef",
                        760,
                    )
                    .expect("confirm");
                    (Some(&operation), InFlight::No)
                }
            };
            let outcome = revoke::revoke(
                store.conn_mut(),
                &fence,
                grant_id,
                operation_arg,
                in_flight,
                800,
                "revoked",
            )
            .expect("revoke");
            std::mem::discriminant(&outcome)
        })
        .collect::<Vec<_>>();

    for (left, right) in outcomes.iter().enumerate().flat_map(|(i, left)| {
        outcomes
            .iter()
            .enumerate()
            .filter(move |(j, _)| *j > i)
            .map(move |(_, right)| (left, right))
    }) {
        assert_ne!(
            left, right,
            "§4.3's four revocation rows must not collapse into one outcome"
        );
    }
}

#[test]
fn revoking_a_grant_twice_is_not_an_error_and_does_not_change_the_answer() {
    let (_dir, mut store) = store();
    let policy = policy(GATING_YAML);
    let (_, fence, _) = revocation_world(&mut store, &policy, "AG-twice", "AR-twice");

    revoke::revoke(
        store.conn_mut(),
        &fence,
        "AG-twice",
        None,
        InFlight::No,
        800,
        "first",
    )
    .expect("revoke");
    let second = revoke::revoke(
        store.conn_mut(),
        &fence,
        "AG-twice",
        None,
        InFlight::No,
        801,
        "second",
    )
    .expect("revoke twice");
    assert!(
        matches!(second, RevocationOutcome::AlreadyRevoked { .. }),
        "{second:?}"
    );
}
