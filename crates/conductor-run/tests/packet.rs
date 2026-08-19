//! Packets — master plan §6.5, §6.6 (slice S12).
//!
//! # The two sentences this file exists to enforce
//!
//! §6.5 opens with the rule that decides the whole design:
//!
//! > **Every packet is generated from durable state, content-hashed, and stored
//! > as an artifact.** No packet is assembled from conversation history. A packet
//! > that cannot be regenerated from the store plus the repository is a bug.
//!
//! and §6.6 says what "generated" has to mean to be worth anything:
//!
//! > Packets and policy snapshots must serialize **byte-identically** for
//! > identical state. Not a stylistic preference: `binding_hash` and
//! > `policy_hash` are worthless if serialization is nondeterministic.
//!
//! # Why determinism is tested against a *second store*, not a second call
//!
//! Calling one function twice in one process and getting the same bytes proves
//! almost nothing: it is satisfied by any pure function of values already in
//! memory, including ones that captured a timestamp or a `HashMap` iteration
//! order once. The test that can fail is the one that rebuilds the state — a
//! second `Store`, opened separately, holding rows written in a different order
//! — and demands the same bytes out. That is the property §6.5 actually claims,
//! because it is the property a *restart* depends on.
//!
//! # Context minimization is a security property, not a cost optimization
//!
//! §6.5: decisions are *"selected by touching the task's scope globs or explicit
//! refs — **never** 'all accepted decisions'"*. A packet that dumps every
//! decision is a packet that grows without bound as a project ages, and the
//! first thing that breaks is the size budget the same section imposes. So the
//! selection is asserted by including a decision that must **not** appear.

use std::collections::BTreeSet;
use std::path::Path;

use conductor_core::{PlanVersionState, ProjectId, RunId, TaskId};
use conductor_git::run_git_ok;
use conductor_run::approval::{
    self, Authorization, Expiry, GrantOptions, NewApprovalRequest, Subject,
};
use conductor_run::packet::{self, continuation, implementation};
use conductor_run::plan::{self, ledger, materialize};
use conductor_run::policy::load as policy_load;
use conductor_run::policy::model::{FactSet, Scope};
use conductor_run::verify::profile;
use conductor_store::{NewRun, Store};

// ---------------------------------------------------------------------------
// Fixture — a project with two decisions, one in scope and one not.
// ---------------------------------------------------------------------------

const PROJECT_YAML: &str = "\
project:
  id: p-packet
  default_branch: main
  adapter: fake
  scope_defaults:
    allowed_globs: [\"src/**\"]
    forbidden_globs: [\".conductor/**\"]
";

const VERIFICATION_YAML: &str = "\
verification:
  required:
    - id: typecheck
      command: cargo check --all-targets
  invariants:
    - id: unit-tests
      command: cargo test
";

const POLICY_YAML: &str = "\
policy:
  rules:
    - id: project.no-force-push
      action: git.force_push
      effect: deny
    - id: project.dependency
      action: dependency.add
      effect: require_approval
";

/// Referenced by T-0012, so §6.5 selects it.
const DECISION_IN_SCOPE: &str = "\
---
id: D-0001
status: ACCEPTED
date: 2026-08-15
---
Policy evaluation happens before the agent launches, not after it reports.
";

/// Referenced by nothing. §6.5's \"never all accepted decisions\" is exactly
/// the rule that must keep this out of T-0012's packet.
const DECISION_OUT_OF_SCOPE: &str = "\
---
id: D-0002
status: ACCEPTED
date: 2026-08-15
---
The changelog is generated, never hand-edited.
";

fn plan_yaml() -> String {
    "plan:\n  id: p-packet\n  version: 1\n  objective: \"Make packets deterministic.\"\n  \
     milestones:\n    - id: M-02\n      title: \"Packets\"\n      slices:\n        \
     - id: S-12\n          title: \"Packets and reports\"\n          tasks:\n            \
     - id: T-0012\n              objective: \"Generate the implementation packet.\"\n              \
     scope:\n                allowed_globs: [\"src/**\"]\n              \
     decisions: [D-0001]\n              \
     acceptance_criteria:\n                - id: AC-1\n                  \
     statement: \"The same state produces the same bytes.\"\n                  \
     verified_by: [unit-tests]\n                - id: AC-2\n                  \
     statement: \"The packet stays inside its budget.\"\n                  \
     verified_by: [typecheck]\n"
        .to_string()
}

fn catalogue() -> BTreeSet<String> {
    let loaded = profile::parse(VERIFICATION_YAML).expect("verification fixture parses");
    plan::check_ids(&loaded.profile)
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create parent");
    std::fs::write(&path, text).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    run_git_ok(root, &["init", "-q", "-b", "main"]).expect("git init");
    run_git_ok(root, &["config", "user.email", "packet@example.invalid"]).expect("email");
    run_git_ok(root, &["config", "user.name", "Packet Test"]).expect("name");
    write(root, ".conductor/project.yaml", PROJECT_YAML);
    write(root, ".conductor/verification.yaml", VERIFICATION_YAML);
    write(root, ".conductor/policy.yaml", POLICY_YAML);
    write(
        root,
        ".conductor/decisions/D-0001-policy-first.md",
        DECISION_IN_SCOPE,
    );
    write(
        root,
        ".conductor/decisions/D-0002-changelog.md",
        DECISION_OUT_OF_SCOPE,
    );
    write(root, ".conductor/plans/v1/plan.yaml", &plan_yaml());
    std::fs::create_dir_all(root.join("src")).expect("src");
    write(root, "src/lib.rs", "pub fn base() -> u32 { 0 }\n");
    run_git_ok(root, &["add", "-A"]).expect("add");
    run_git_ok(root, &["commit", "-q", "-m", "initial"]).expect("commit");
    dir
}

/// A project registered, v1 approved and materialised, and one run created.
///
/// `now_ms` is threaded through every write so a caller can build the *same*
/// state at a different wall clock — which is how the "no timestamps inside
/// hashed content" rule is falsifiable rather than merely asserted.
fn live(repo: &Path, db: &Path, now_ms: i64) -> (ProjectId, RunId) {
    let mut store = Store::open_or_create(db).expect("open");
    let project = ledger::register_project(&mut store, repo, now_ms).expect("register project");
    let project_id = project.id.clone();
    let registered = ledger::register_plan_version(&mut store, &project_id, 1, &catalogue())
        .expect("register v1");

    let request_id = "AR-1".to_string();
    approval::request(
        store.conn_mut(),
        &NewApprovalRequest {
            id: request_id.clone(),
            subject: Subject::PlanVersion {
                plan_version_id: registered.row.id.as_str().to_string(),
            },
            run_id: None,
            facts: FactSet::new(),
            policy_hash: "blake3:policy".to_string(),
            matched_rules: Vec::new(),
            explanation: "authoritative".to_string(),
            evidence_ref: None,
            expires: Expiry::Never,
        },
        now_ms,
    )
    .expect("request");
    let grant = approval::grant(
        store.conn_mut(),
        &request_id,
        &GrantOptions {
            id: "AG-1".to_string(),
            scope: Scope::from_pairs([(
                "plan_version".to_string(),
                registered.row.id.as_str().to_string(),
            )]),
            reuse: false,
            expires: Expiry::Never,
            granted_by: "alice".to_string(),
            channel: "socket".to_string(),
            nonce_hash: None,
        },
        now_ms,
    )
    .expect("grant");
    let id = ledger::plan_version_id(&project_id, 1);
    store
        .set_plan_state(&id, PlanVersionState::AwaitingApproval)
        .expect("await");
    ledger::approve(
        &mut store,
        &project_id,
        1,
        &Authorization::Authorized { grant_id: grant.id },
        now_ms,
    )
    .expect("approve");
    conductor_run::decision::register_decisions(&mut store, &project_id).expect("decisions");
    materialize::materialize(&mut store, &project_id, 1, &registered.plan, now_ms)
        .expect("materialise");

    let resolved = policy_load::resolve(None, Some(&repo.join(".conductor/policy.yaml")), None)
        .expect("policy");
    let snapshot = policy_load::snapshot(&resolved);
    policy_load::persist(store.conn_mut(), &snapshot, now_ms).expect("persist policy");

    let run_id = RunId::new("r-0041").expect("run id");
    store
        .create_run(
            &NewRun {
                id: run_id.clone(),
                task_id: TaskId::new("T-0012").expect("task id"),
                policy_hash: snapshot.hash.clone(),
                base_commit: "3f2a1c".to_string() + &"0".repeat(34),
                run_branch: "conductor/T-0012/r-0041".to_string(),
                target_branch: "main".to_string(),
            },
            now_ms,
        )
        .expect("create run");
    (project_id, run_id)
}

// ---------------------------------------------------------------------------
// §6.6 — determinism
// ---------------------------------------------------------------------------

#[test]
fn the_same_durable_state_produces_a_byte_identical_packet_from_a_separate_store() {
    // §6.6's actual claim. Two stores, built independently, in two different
    // directories, at two different wall clocks. A packet that captured a
    // timestamp, a path that varies, or a map iteration order fails here and
    // passes a same-process double call.
    let repo_dir = repo();
    let a = tempfile::tempdir().expect("a");
    let b = tempfile::tempdir().expect("b");

    let (_p1, run1) = live(repo_dir.path(), &a.path().join("conductor.db"), 1_000);
    let (_p2, run2) = live(repo_dir.path(), &b.path().join("conductor.db"), 9_999_000);

    let mut store_a = Store::open_existing(a.path().join("conductor.db")).expect("a");
    let mut store_b = Store::open_existing(b.path().join("conductor.db")).expect("b");

    let one = implementation::build(&mut store_a, &run1).expect("packet a");
    let two = implementation::build(&mut store_b, &run2).expect("packet b");

    assert_eq!(
        one.canonical_bytes(),
        two.canonical_bytes(),
        "identical durable state must serialize byte-identically (§6.6)"
    );
    assert_eq!(one.hash(), two.hash());
}

#[test]
fn a_packet_carries_no_wall_clock_inside_the_bytes_it_is_hashed_over() {
    // §6.6: "no timestamps inside hashed content". Stated separately from the
    // test above because that one would still pass if *both* builds captured
    // the same constant; this one names the failure mode.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");
    let rendered = implementation::build(&mut store, &run)
        .expect("packet")
        .to_yaml();

    for needle in ["1970-", "2026-", "created_at", "emitted_at", "timestamp"] {
        assert!(
            !rendered.contains(needle),
            "hashed packet content must not contain {needle:?}:\n{rendered}"
        );
    }
}

// ---------------------------------------------------------------------------
// §6.5 — what the packet must actually say
// ---------------------------------------------------------------------------

#[test]
fn the_packet_carries_every_field_section_6_5_names() {
    // POSITIVE CONTROL for the whole file. A builder that emitted an empty
    // document would satisfy determinism, the size budget and context
    // minimization all at once.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");
    let built = implementation::build(&mut store, &run).expect("packet");
    let y = built.to_yaml();

    for needle in [
        "packet: implementation",
        "packet_version:",
        "run_id: r-0041",
        "task_id: T-0012",
        "plan_version: 1",
        "plan_hash: blake3:",
        "policy_hash: blake3:",
        "objective:",
        "acceptance_criteria:",
        "AC-1",
        "scope:",
        "allowed_globs:",
        "forbidden_globs:",
        "repository:",
        "base_commit:",
        "verification:",
        "boundaries:",
        "report_schema:",
    ] {
        assert!(y.contains(needle), "§6.5 requires {needle:?} in:\n{y}");
    }

    // Every acceptance criterion must arrive bound to a check — §3.7 makes an
    // unbound criterion "the mechanism by which a task reaches COMPLETE on an
    // agent's word", and a packet that dropped the binding would hand the agent
    // exactly that.
    assert!(
        y.contains("verified_by"),
        "criteria must arrive bound to checks:\n{y}"
    );
    // §3.3: the always-forbidden write scope has to reach the agent.
    assert!(
        y.contains(".conductor/**"),
        "forbidden scope must travel:\n{y}"
    );
}

#[test]
fn only_decisions_the_task_explicitly_references_travel() {
    // §6.5: "decisions selected by touching the task's scope globs or explicit
    // refs — never 'all accepted decisions'". Of those two mechanisms only
    // explicit refs is implementable: §3.6 fixes a decision's frontmatter at
    // four fields, so a decision cannot declare a scope to be matched against.
    // See ADR-0016.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");
    let y = implementation::build(&mut store, &run)
        .expect("packet")
        .to_yaml();

    assert!(
        y.contains("D-0001"),
        "a decision the task references must travel — without this the assertion below is vacuous:\n{y}"
    );
    assert!(
        !y.contains("D-0002"),
        "a decision nothing references must NOT travel (§6.5):\n{y}"
    );
}

#[test]
fn the_governance_path_is_forbidden_even_when_no_document_says_so() {
    // §3.3: `.conductor/**` is an always-forbidden write scope "regardless of
    // what any plan says, precisely because the plan is a file the agent can
    // edit". The fixture used everywhere else declares it in `scope_defaults`,
    // which makes the assertion in
    // [`the_packet_carries_every_field_section_6_5_names`] pass whether or not
    // Conductor adds it — a mutation removing the union survived that test.
    //
    // This one removes the declaration, so the only thing that can put
    // `.conductor/**` in the packet is Conductor itself.
    let repo_dir = repo();
    write(
        repo_dir.path(),
        ".conductor/project.yaml",
        "project:\n  id: p-packet\n  default_branch: main\n  adapter: fake\n  \
         scope_defaults:\n    allowed_globs: [\"src/**\"]\n",
    );
    run_git_ok(repo_dir.path(), &["add", "-A"]).expect("add");
    run_git_ok(
        repo_dir.path(),
        &["commit", "-q", "-m", "drop the declaration"],
    )
    .expect("commit");

    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");
    let y = implementation::build(&mut store, &run)
        .expect("packet")
        .to_yaml();

    assert!(
        y.contains(".conductor/**"),
        "§3.3's always-forbidden scope must reach the agent even when neither \
         the project nor the plan declares it:\n{y}"
    );
}

// ---------------------------------------------------------------------------
// §6.5 — the size budget
// ---------------------------------------------------------------------------

#[test]
fn a_packet_stays_within_its_budget_when_the_evidence_does_not() {
    // §6.5 targets <4 KB and links evidence rather than embedding it. The test
    // that can fail is the one where the evidence is genuinely large: a builder
    // that embedded it would blow the budget here and pass on a small fixture.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);

    // A prior diff no packet should ever inline.
    let big = repo_dir.path().join("huge.patch");
    std::fs::write(&big, "x".repeat(600_000)).expect("write a large artifact");

    let mut store = Store::open_existing(&db).expect("store");
    let built = implementation::build(&mut store, &run)
        .expect("packet")
        .with_evidence(packet::Evidence::linked("prior_diff", &big).expect("link the artifact"));

    let bytes = built.canonical_bytes().len();
    assert!(
        bytes <= packet::MAX_PACKET_BYTES,
        "packet is {bytes} bytes, over the {} byte ceiling",
        packet::MAX_PACKET_BYTES
    );
    let y = built.to_yaml();
    assert!(
        !y.contains(&"x".repeat(200)),
        "large evidence must be linked, never embedded:\n{}",
        &y[..y.len().min(600)]
    );
    // Linked "never embedded" is only honest if the link is usable: a path the
    // reader can open and a digest they can check it against.
    assert!(
        y.contains("huge.patch"),
        "the link must name the artifact:\n{y}"
    );
    assert!(
        y.contains("blake3:"),
        "a link without a digest is a path that can change under the reader:\n{y}"
    );
}

#[test]
fn an_oversized_packet_is_refused_rather_than_silently_truncated() {
    // The rule the budget must not break: "a packet that drops the reason for a
    // policy finding merely to fit the limit is not correct." So the failure
    // mode is a refusal naming the overflow, never a quietly shortened
    // load-bearing field.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");
    let mut built = implementation::build(&mut store, &run).expect("packet");

    // Far more linked evidence than the ceiling can hold. Links are small, so
    // this takes many — which is the point: the bound is on the packet, not on
    // any one field.
    for i in 0..20_000 {
        built = built.with_evidence(packet::Evidence::inline(
            "note",
            format!("finding-{i}-{}", "y".repeat(60)),
        ));
    }
    let err = built
        .try_canonical_bytes()
        .expect_err("an over-budget packet must refuse");
    let rendered = err.to_string();
    assert!(
        rendered.contains(&packet::MAX_PACKET_BYTES.to_string()),
        "the refusal must name the ceiling it hit: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// §6.5 — the continuation packet, and S12's stop point
// ---------------------------------------------------------------------------

#[test]
fn a_continuation_packet_is_the_implementation_packet_plus_observed_reality() {
    // §6.5: "Continuation packet = implementation packet **plus observed
    // reality**: reconciliation_verdict, current tree hash vs base, the actual
    // diff so far, which criteria already verify green at the current tree,
    // commits in the run clone, the partial report if any".
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    let observed = continuation::Observed {
        reconciliation_verdict: "CLEAN_NO_REPORT".to_string(),
        tree_hash: "blake3:aaaa".to_string(),
        diff: continuation::Diff::summary(2, 31, 4),
        changed_paths: Vec::new(),
        criteria_green: vec!["AC-1".to_string()],
        commits: vec!["c0ffee1".to_string()],
        partial_report: None,
    };
    let built = continuation::build(&mut store, &run, &observed).expect("continuation");
    let y = built.to_yaml();

    // It is still an implementation packet — everything the agent needed before
    // it is still what it needs now.
    for needle in [
        "task_id: T-0012",
        "acceptance_criteria",
        "scope:",
        "boundaries:",
    ] {
        assert!(
            y.contains(needle),
            "{needle:?} must survive into the continuation:\n{y}"
        );
    }
    assert!(
        y.contains("packet: continuation"),
        "and it must say which it is:\n{y}"
    );

    // Plus observed reality.
    for needle in [
        "reconciliation_verdict: CLEAN_NO_REPORT",
        "tree_hash:",
        "base_commit:",
        "criteria_already_green",
        "AC-1",
        "commits",
        "c0ffee1",
    ] {
        assert!(y.contains(needle), "§6.5 requires {needle:?}:\n{y}");
    }
}

#[test]
fn a_continuation_packet_says_the_previous_agents_reasoning_is_gone() {
    // §6.5 quotes the sentence verbatim, and it is the whole point of S12's stop
    // point ("recovery does not depend on hidden state"): a fresh agent that
    // believed it could recover the prior chain of thought would wait for
    // something that does not exist.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");
    let y = continuation::build(&mut store, &run, &continuation::Observed::none())
        .expect("continuation")
        .to_yaml();

    assert!(
        y.contains("The previous agent's reasoning is not available"),
        "§6.5's sentence must travel verbatim:\n{y}"
    );
    assert!(
        y.contains("inferable only from the diff"),
        "including the half that says what to do instead:\n{y}"
    );
}

#[test]
fn a_continuation_packet_is_regenerable_after_the_process_that_made_it_is_gone() {
    // S12's test line: "continuation packet regenerable after total process
    // restart". Same durable state plus the same observation must give the same
    // bytes from a store this process opened fresh — the property a restart
    // depends on, and the one a packet assembled from conversation history
    // could not have.
    let repo_dir = repo();
    let a = tempfile::tempdir().expect("a");
    let b = tempfile::tempdir().expect("b");
    let (_p1, run1) = live(repo_dir.path(), &a.path().join("conductor.db"), 1_000);
    let (_p2, run2) = live(repo_dir.path(), &b.path().join("conductor.db"), 9_999_000);

    let observed = continuation::Observed {
        reconciliation_verdict: "CLEAN_NO_REPORT".to_string(),
        tree_hash: "blake3:aaaa".to_string(),
        diff: continuation::Diff::summary(2, 31, 4),
        changed_paths: Vec::new(),
        criteria_green: vec!["AC-1".to_string()],
        commits: vec!["c0ffee1".to_string()],
        partial_report: None,
    };

    let mut store_a = Store::open_existing(a.path().join("conductor.db")).expect("a");
    let mut store_b = Store::open_existing(b.path().join("conductor.db")).expect("b");
    let one = continuation::build(&mut store_a, &run1, &observed).expect("a");
    let two = continuation::build(&mut store_b, &run2, &observed).expect("b");

    assert_eq!(one.canonical_bytes(), two.canonical_bytes());
}

#[test]
fn a_continuation_packet_links_a_large_diff_rather_than_carrying_it() {
    // The diff is the one field that grows without bound, and §6.5's budget does
    // not get to drop it — so it is linked, exactly like prior diffs.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let big = repo_dir.path().join("run.diff");
    std::fs::write(&big, "+".repeat(500_000)).expect("write");

    let mut store = Store::open_existing(&db).expect("store");
    let observed = continuation::Observed {
        reconciliation_verdict: "CLEAN_NO_REPORT".to_string(),
        tree_hash: "blake3:aaaa".to_string(),
        diff: continuation::Diff::linked(&big).expect("link"),
        changed_paths: Vec::new(),
        criteria_green: Vec::new(),
        commits: Vec::new(),
        partial_report: None,
    };
    let built = continuation::build(&mut store, &run, &observed).expect("continuation");
    let bytes = built.canonical_bytes().len();
    assert!(
        bytes <= packet::MAX_PACKET_BYTES,
        "continuation packet is {bytes} bytes, over the ceiling"
    );
    let y = built.to_yaml();
    assert!(
        !y.contains(&"+".repeat(200)),
        "the diff must be linked, not carried"
    );
    assert!(
        y.contains("run.diff") && y.contains("blake3:"),
        "with a path and a digest:\n{y}"
    );
}

// ---------------------------------------------------------------------------
// Secret safety — §6.5's surfaces that carry text nobody vetted
// ---------------------------------------------------------------------------

/// Disposable canaries. **Every value here is fake**, and shaped only so that
/// `verify::secrets` recognises the *form* — the point is to prove the packet
/// redacts, and a real credential would prove it by doing the thing this test
/// exists to prevent.
mod canary {
    /// `AKIA` + 16 upper-case alphanumerics. AWS's own documentation example.
    pub const AWS: &str = "AKIAIOSFODNN7EXAMPLE";
    /// `ghp_` + 36. Not a token: a repeated pattern of the right length.
    pub const GITHUB: &str = "ghp_0123456789abcdef0123456789abcdef0123";
    /// §4.9's shape: credentials inline in a URL.
    ///
    /// The password is deliberately **8+ characters**. `secrets::is_placeholder`
    /// suppresses shorter values on purpose — a 6-character "password" is far more
    /// often a variable name or an example than a credential — so a canary like
    /// `hunter2` tests the placeholder rule rather than the detector, and reads as
    /// a scanner gap when it is the fixture that is wrong.
    pub const DB_PASSWORD: &str = "n0t-a-real-password";
    pub const DB_URL: &str = "postgres://conductor:n0t-a-real-password@db.invalid:5432/app";
}

#[test]
fn a_partial_report_that_leaked_a_credential_does_not_reach_the_next_agent() {
    // The continuation packet's `partial_report` is **agent-produced text**, and
    // §6.5 hands the packet to a *different* agent. Nothing else in the packet has
    // that property: the objective, the criteria and the decisions are documents a
    // human wrote and committed. So this is the surface where a secret arrives
    // from outside and is then re-published, and the one worth testing hardest.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    let leaked = format!(
        "I exported the key {} and set DATABASE_URL={} before running the tests.",
        canary::AWS,
        canary::DB_URL
    );
    let observed = continuation::Observed {
        reconciliation_verdict: "CLEAN_NO_REPORT".to_string(),
        tree_hash: "blake3:aaaa".to_string(),
        diff: continuation::Diff::summary(1, 2, 0),
        changed_paths: Vec::new(),
        criteria_green: Vec::new(),
        commits: Vec::new(),
        partial_report: Some(leaked),
    };

    let built = continuation::build(&mut store, &run, &observed).expect("continuation");
    let rendered = built.to_yaml();

    // Not "the packet was refused" — refusing would discard the reason the run
    // stopped, which §6.5's size rule already forbids for the same reason. The
    // packet is delivered with the value removed and the fact recorded.
    assert!(
        !rendered.contains(canary::AWS),
        "an AWS-shaped canary survived into the packet:\n{rendered}"
    );
    assert!(
        !rendered.contains(canary::DB_PASSWORD),
        "a URL password survived into the packet:\n{rendered}"
    );
    assert!(
        rendered.contains("REDACTED"),
        "the redaction must be visible, or a reader cannot tell text was removed \
         from text that never existed:\n{rendered}"
    );
    // The surrounding prose is still there: a redaction that ate the sentence
    // would lose the observation the next agent needs.
    assert!(
        rendered.contains("before running the tests"),
        "only the secret is removed:\n{rendered}"
    );

    // And the hash covers what was delivered, not the unredacted original —
    // otherwise the digest names a document nobody has.
    let bytes = String::from_utf8(built.canonical_bytes()).expect("utf8");
    assert!(
        !bytes.contains(canary::AWS) && !bytes.contains(canary::DB_PASSWORD),
        "the hashed bytes still carry the secret"
    );
}

#[test]
fn redaction_is_deterministic_and_reports_what_it_found() {
    // §6.6 is not weakened by redaction: it is a pure function of the text, so
    // the same state still produces the same bytes. Asserted because a redactor
    // that inserted a counter, an offset or a random marker would break the one
    // property every packet hash depends on.
    let repo_dir = repo();
    let a = tempfile::tempdir().expect("a");
    let b = tempfile::tempdir().expect("b");
    let (_p1, run1) = live(repo_dir.path(), &a.path().join("conductor.db"), 1_000);
    let (_p2, run2) = live(repo_dir.path(), &b.path().join("conductor.db"), 9_999_000);

    let leaked = format!("token {} and key {}", canary::GITHUB, canary::AWS);
    let observed = |report: &str| continuation::Observed {
        reconciliation_verdict: "CLEAN_NO_REPORT".to_string(),
        tree_hash: "blake3:aaaa".to_string(),
        diff: continuation::Diff::summary(1, 2, 0),
        changed_paths: Vec::new(),
        criteria_green: Vec::new(),
        commits: Vec::new(),
        partial_report: Some(report.to_string()),
    };

    let mut store_a = Store::open_existing(a.path().join("conductor.db")).expect("a");
    let mut store_b = Store::open_existing(b.path().join("conductor.db")).expect("b");
    let one = continuation::build(&mut store_a, &run1, &observed(&leaked)).expect("a");
    let two = continuation::build(&mut store_b, &run2, &observed(&leaked)).expect("b");

    assert_eq!(
        one.canonical_bytes(),
        two.canonical_bytes(),
        "redaction must be deterministic, or §6.6's byte-identical claim is false"
    );
    assert_eq!(one.hash(), two.hash());

    // Both kinds are named. A redaction that collapsed everything to one marker
    // would hide that two different classes of credential were present, which is
    // the difference between "rotate a token" and "rotate a token and a key".
    let rendered = one.to_yaml();
    assert!(rendered.contains("github-token"), "{rendered}");
    assert!(rendered.contains("aws-access-key-id"), "{rendered}");
}

#[test]
fn a_clean_packet_is_not_marked_redacted() {
    // POSITIVE CONTROL. Without it, every assertion above is satisfied by a
    // packet that stamps "REDACTED" over everything — which would pass the
    // secret tests and destroy the packet.
    let repo_dir = repo();
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("conductor.db");
    let (_p, run) = live(repo_dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    let rendered = implementation::build(&mut store, &run)
        .expect("packet")
        .to_yaml();

    assert!(
        !rendered.contains("REDACTED"),
        "nothing in this fixture is a secret:\n{rendered}"
    );
    // …and the fixture's real content is intact.
    assert!(
        rendered.contains("Generate the implementation packet"),
        "{rendered}"
    );
    assert!(rendered.contains("AC-1"), "{rendered}");
}

// ---------------------------------------------------------------------------
// §6.5's review packet (S13) — the one packet whose reader is a person
// ---------------------------------------------------------------------------

use conductor_run::packet::review;

/// A review half with one of everything §6.5 names, so a conformance test can
/// assert the fields rather than the plumbing.
fn review_fields() -> review::ReviewFields {
    review::ReviewFields {
        tasks: vec!["T-0001".to_string()],
        boundary: "policy_violation".to_string(),
        end_state: "AWAITING_REVIEW".to_string(),
        proposed_next_state: "COMPLETE".to_string(),
        claims: review::Claims {
            claim: Some("COMPLETE".to_string()),
            task_id: Some("T-0001".to_string()),
            files_touched: vec!["src/added.rs".to_string()],
            summary: "added a function".to_string(),
            commands_run: vec!["cargo test".to_string()],
            acceptance_criteria: vec!["AC-1".to_string()],
            deviations: vec!["used a different helper name".to_string()],
            blockers: vec!["none".to_string()],
            unverified_claims: vec!["it is faster".to_string()],
        },
        measured: review::Measured {
            reconciliation_verdict: "CONTRADICTED".to_string(),
            tree_hash: "tree-abc".to_string(),
            changed_paths: vec!["src/other.rs".to_string()],
            commits: vec!["abcdef123456".to_string()],
        },
        diff: continuation::Diff::summary(1, 4, 0),
        checks: vec![review::CheckLine {
            check_id: "unit-tests".to_string(),
            command: "/bin/sh -c echo ok".to_string(),
            outcome: "PASS".to_string(),
            exit_code: Some(0),
            duration_ms: Some(12),
            tree_hash: "tree-abc".to_string(),
        }],
        policy: vec![review::PolicyLine {
            action: "dependency.add".to_string(),
            decision: "require_approval".to_string(),
            explanation: "rule dep-1 matched the manifest change".to_string(),
        }],
        approvals: vec![review::ApprovalLine {
            grant_id: "AG-1".to_string(),
            kind: "DEPENDENCY_ADD".to_string(),
            scope: "package=serde".to_string(),
            granted_by: "operator".to_string(),
        }],
        unresolved_findings: vec![review::FindingLine {
            id: "F-1".to_string(),
            kind: "REPORTCONTRADICTED".to_string(),
            severity: "CRITICAL".to_string(),
            evidence_ref: "src/other.rs".to_string(),
        }],
    }
}

#[test]
fn a_review_packet_carries_every_field_section_6_5_names() {
    // §6.5's list, read as a checklist. Asserted against the rendered YAML rather
    // than the struct, because the struct is what this test was written from and
    // the YAML is what a human actually gets — a field lost in serialization
    // (`skip_serializing_if` on the wrong thing, a rename) reaches the reviewer as
    // absent, and absent reads as "there were none".
    let dir = repo();
    let db = dir.path().join("conductor.db");
    let (_project, run) = live(dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    let packet = review::build(&mut store, &run, &review_fields()).expect("compose");
    let yaml = packet.to_yaml();

    // The discriminant, so a reader (and the import path) can tell which of the
    // four packet shapes this is.
    assert!(yaml.contains("packet: review"), "{yaml}");

    for needle in [
        // plan version and hash — inherited from the implementation half
        "plan_version",
        "plan_hash",
        // task ids, base commit, end state, proposed next state
        "T-0001",
        "base_commit",
        "end_state",
        "proposed_next_state",
        // the diff, stat inline
        "insertions",
        // agent claims vs reconciliation verdict, side by side
        "claims",
        "measured",
        "CONTRADICTED",
        "deviations",
        "used a different helper name",
        "unverified_claims",
        // every verification command with exit code and duration
        "unit-tests",
        "exit_code",
        "duration_ms",
        // policy evaluations and explanations
        "require_approval",
        "rule dep-1 matched the manifest change",
        // approvals granted with scope
        "AG-1",
        "package=serde",
        // unresolved findings
        "unresolved_findings",
        "F-1",
    ] {
        assert!(
            yaml.contains(needle),
            "§6.5 names {needle:?} and the review packet does not carry it:\n{yaml}"
        );
    }
}

#[test]
fn a_review_packet_shows_the_agents_claim_and_the_measured_verdict_without_reconciling_them() {
    // Acceptance row 6 is "the agent said COMPLETE and the tree says otherwise".
    // The packet's job is to put both in front of a human **unreconciled** — a
    // packet that resolved the disagreement itself would be making the decision it
    // exists to inform, and §4.8's doctrine is that the measured fact survives.
    let dir = repo();
    let db = dir.path().join("conductor.db");
    let (_project, run) = live(dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    let packet = review::build(&mut store, &run, &review_fields()).expect("compose");
    let yaml = packet.to_yaml();

    assert!(
        yaml.contains("claim: COMPLETE"),
        "the agent's claim must survive verbatim:\n{yaml}"
    );
    assert!(
        yaml.contains("reconciliation_verdict: CONTRADICTED"),
        "and so must the verdict that contradicts it:\n{yaml}"
    );
    // The two sides disagree about which file changed, and both lists travel.
    assert!(yaml.contains("src/added.rs"), "the claimed path:\n{yaml}");
    assert!(yaml.contains("src/other.rs"), "the measured path:\n{yaml}");
}

#[test]
fn the_same_durable_state_produces_a_byte_identical_review_packet_from_a_separate_store() {
    // §6.6, for the fourth packet. Two stores, two directories, two wall clocks.
    let first = repo();
    let second = repo();
    let db_one = first.path().join("conductor.db");
    let db_two = second.path().join("conductor.db");
    let (_p1, run_one) = live(first.path(), &db_one, 1_000);
    let (_p2, run_two) = live(second.path(), &db_two, 9_999_000);

    let mut store_one = Store::open_existing(&db_one).expect("store one");
    let mut store_two = Store::open_existing(&db_two).expect("store two");

    let one = review::build(&mut store_one, &run_one, &review_fields()).expect("one");
    let two = review::build(&mut store_two, &run_two, &review_fields()).expect("two");

    assert_eq!(
        one.canonical_bytes(),
        two.canonical_bytes(),
        "a review packet's canonical bytes must not depend on which store built it"
    );
    assert_eq!(one.hash().as_str(), two.hash().as_str());
}

#[test]
fn a_review_packet_links_a_large_diff_rather_than_carrying_it() {
    // The bound, on the packet whose reader most wants to see everything. A diff
    // travels as a path and a digest; the stat is what goes inline.
    let dir = repo();
    let db = dir.path().join("conductor.db");
    let (_project, run) = live(dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    let big = dir.path().join("run.diff");
    std::fs::write(&big, "+".repeat(600 * 1024)).expect("write a 600 KB diff");

    let mut fields = review_fields();
    fields.diff = continuation::Diff::linked(&big).expect("link");

    let packet = review::build(&mut store, &run, &fields).expect("compose");
    let emitted = packet
        .emit()
        .expect("a linked diff must stay within the ceiling");

    assert!(
        emitted.bytes().len() < 64 * 1024,
        "the packet carried the diff instead of linking it: {} bytes",
        emitted.bytes().len()
    );
    let yaml = packet.to_yaml();
    assert!(yaml.contains("run.diff"), "the path must travel:\n{yaml}");
    assert!(yaml.contains("blake3:"), "and so must the digest:\n{yaml}");
    assert!(
        !yaml.contains(&"+".repeat(1024)),
        "the diff's bytes must not be in the packet"
    );
}

#[test]
fn an_oversized_review_packet_is_refused_rather_than_silently_truncated() {
    // The refusal direction. A review packet quietly missing its last finding is
    // the worst possible thing to hand a human who is about to accept work, so the
    // ceiling is an error and not a truncation.
    let dir = repo();
    let db = dir.path().join("conductor.db");
    let (_project, run) = live(dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    let mut fields = review_fields();
    fields.unresolved_findings = (0..4_000)
        .map(|i| review::FindingLine {
            id: format!("F-{i}"),
            kind: "OUTOFSCOPEPATH".to_string(),
            severity: "BLOCKING".to_string(),
            evidence_ref: format!("src/generated/file_{i}.rs"),
        })
        .collect();

    let packet = review::build(&mut store, &run, &fields).expect("compose");
    match packet.emit() {
        Err(packet::PacketError::OverBudget { bytes, ceiling }) => {
            assert!(bytes > ceiling, "the refusal must name real numbers");
        }
        Err(other) => panic!("the wrong refusal: {other}"),
        Ok(emitted) => panic!(
            "an oversized review packet was emitted at {} bytes instead of refused",
            emitted.bytes().len()
        ),
    }

    // Positive control: the same packet without the flood emits, so the refusal
    // above is the budget and not the composition.
    let ok = review::build(&mut store, &run, &review_fields()).expect("compose");
    ok.emit().expect("an ordinary review packet emits");
}

#[test]
fn a_credential_an_agent_wrote_into_a_deviation_does_not_reach_the_review_file() {
    // The review packet added five agent-authored free-text fields
    // (`deviations`, `blockers`, `unverified_claims`, `commands_run`, `summary`),
    // and every one is a new surface for the leak S9 measured. `commands_run` is
    // the sharpest: an agent recording what it ran will paste the command line,
    // and command lines are where tokens live.
    let dir = repo();
    let db = dir.path().join("conductor.db");
    let (_project, run) = live(dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    // Fake values only, and shaped like the real thing so the detector is being
    // tested rather than a placeholder. S12's near-miss was a canary too short to
    // trip `is_placeholder`, so these are all comfortably long.
    let aws = format!("AKIA{}", "Q7ZJ4T2LX9WBMN3D");
    let github = format!("ghp_{}", "9zK4mQ2wX7LpR1sT6vB8nY3fH5jD0aGc4eU2");
    let mut fields = review_fields();
    fields.claims.commands_run = vec![format!("curl -H 'Authorization: token {github}' api")];
    fields.claims.deviations = vec![format!("had to set AWS_SECRET_ACCESS_KEY={aws}")];

    let packet = review::build(&mut store, &run, &fields).expect("compose");
    let yaml = packet.to_yaml();

    assert!(
        !yaml.contains(&aws),
        "an AWS-shaped key reached the review file"
    );
    assert!(
        !yaml.contains(&github),
        "a GitHub-shaped token reached the review file"
    );
    assert!(
        yaml.contains("redacted"),
        "and the packet must say that it redacted something:\n{yaml}"
    );

    // The redaction reaches the hashed bytes too, not only the rendered YAML —
    // `canonical_bytes` renders before it encodes, and a packet whose hash was
    // taken over the unredacted text would leak through the digest's preimage.
    let bytes = String::from_utf8_lossy(&packet.canonical_bytes()).to_string();
    assert!(!bytes.contains(&aws), "the canonical bytes carried the key");
    assert!(
        !bytes.contains(&github),
        "the canonical bytes carried the token"
    );
}

#[test]
fn a_review_packet_with_nothing_to_hide_is_not_marked_redacted() {
    // Positive control for the test above: if `redacted` were always present the
    // assertion there would be satisfied by a constant.
    let dir = repo();
    let db = dir.path().join("conductor.db");
    let (_project, run) = live(dir.path(), &db, 1_000);
    let mut store = Store::open_existing(&db).expect("store");

    let packet = review::build(&mut store, &run, &review_fields()).expect("compose");
    assert!(
        !packet.to_yaml().contains("redacted"),
        "a clean review packet must not claim a redaction happened"
    );
}
