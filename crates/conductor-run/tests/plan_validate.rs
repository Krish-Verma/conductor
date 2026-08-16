//! `plan validate` and the plan content hash — master plan §3.2, §3.6, §3.7.
//!
//! # What these tests are guarding against
//!
//! Two failure modes, and they are opposites.
//!
//! **A validator that accepts too much** is the one §3.7 is written about: "an
//! unbound criterion is the mechanism by which a task reaches `COMPLETE` on an
//! agent's word". Every rejection test below exists because the plan it feeds in
//! would otherwise let a task complete on nothing.
//!
//! **A validator that refuses everything** passes every one of those rejection
//! tests and is completely useless. Guarding against it takes two things, and
//! both are done here deliberately:
//!
//! 1. `valid.yaml` is the positive control, and every rejection fixture is that
//!    same plan with exactly one defect introduced. A rule that over-refuses
//!    shows up as a failure on the positive control.
//! 2. Every rejection test asserts the **complete** defect list, not merely that
//!    some defect was reported. A rule that fires on the wrong fixture is caught
//!    by the fixture it should have been silent on.
//!
//! # The hash tests
//!
//! §3.6: "reformatting does **not** invalidate approval; changing any field
//! **does**." Both halves are asserted, from a fixture pair whose only
//! difference is formatting and a second pair whose only difference is one
//! field. A hash over raw file bytes passes the second test and fails the first.

use std::collections::BTreeSet;
use std::path::PathBuf;

use conductor_core::PlanVersionState;
use conductor_run::plan::{self, Plan, ValidatedPlan, ValidationReport};
use conductor_run::verify::profile;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("plans")
        .join(name)
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn parsed(name: &str) -> Plan {
    plan::parse(&fixture(name)).unwrap_or_else(|e| panic!("fixture {name} must parse: {e}"))
}

/// The check ids `verification.yaml` defines — §3.7's catalogue.
///
/// Built by actually loading the fixture through `verify::profile`, so that a
/// change to what a profile can declare cannot leave this test agreeing with a
/// catalogue nothing else in Conductor would produce.
fn catalogue() -> BTreeSet<String> {
    let loaded = profile::parse(&fixture("verification.yaml")).expect("the profile fixture parses");
    plan::check_ids(&loaded.profile)
}

fn validated(name: &str) -> ValidatedPlan {
    plan::validate(&parsed(name), &catalogue())
        .unwrap_or_else(|report| panic!("fixture {name} must validate, but: {report}"))
}

fn refused(name: &str) -> ValidationReport {
    match plan::validate(&parsed(name), &catalogue()) {
        Ok(_) => panic!("fixture {name} must be refused, but validate accepted it"),
        Err(report) => report,
    }
}

/// The defect kinds a report carries, in report order.
fn kinds(report: &ValidationReport) -> Vec<&'static str> {
    report.defects().iter().map(|d| d.kind()).collect()
}

/// The ids a report's defects name, in report order.
fn subjects(report: &ValidationReport) -> Vec<String> {
    report.defects().iter().map(|d| d.subject()).collect()
}

// ---------------------------------------------------------------------------
// The positive control
// ---------------------------------------------------------------------------

#[test]
fn a_well_formed_plan_validates_and_yields_every_task_it_declares() {
    let validated = validated("valid.yaml");

    assert_eq!(validated.plan().id, "p-fixture");
    assert_eq!(validated.plan().version, 3);

    let ids: Vec<&str> = validated.tasks().map(|t| t.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["T-0001", "T-0002"],
        "the validated plan must expose the tasks in declaration order"
    );
}

#[test]
fn a_plan_whose_every_criterion_binds_to_a_check_needs_no_human_review_boundary() {
    let validated = validated("valid.yaml");
    assert!(
        !validated.requires_human_review(),
        "no criterion in valid.yaml is manual, so nothing forces a review boundary"
    );
    assert!(validated.manual_criteria().is_empty());
}

// ---------------------------------------------------------------------------
// §3.7 — duplicate ids
// ---------------------------------------------------------------------------

#[test]
fn two_tasks_sharing_an_id_are_refused_and_the_refusal_names_the_id() {
    let report = refused("duplicate_task_id.yaml");

    assert_eq!(kinds(&report), vec!["duplicate_id"]);
    assert_eq!(subjects(&report), vec!["T-0001"]);
    assert!(
        report.to_string().contains("T-0001"),
        "the message a human reads must name the offending id: {report}"
    );
}

#[test]
fn two_criteria_in_one_task_sharing_an_id_are_refused() {
    // Criterion ids are scoped to their task — every task in §6.5's packet
    // example starts at AC-1 — so this is a different namespace from the task
    // id above, and it needs its own evidence.
    let report = refused("many_defects.yaml");
    assert!(
        report
            .defects()
            .iter()
            .any(|d| d.kind() == "duplicate_id" && d.subject() == "AC-1"),
        "a criterion id used twice inside one task must be refused: {report}"
    );
}

// ---------------------------------------------------------------------------
// §3.7 — dangling dependencies
// ---------------------------------------------------------------------------

#[test]
fn a_dependency_naming_a_task_that_does_not_exist_is_refused_and_names_both_ends() {
    let report = refused("dangling_dependency.yaml");

    assert_eq!(kinds(&report), vec!["dangling_dependency"]);
    assert_eq!(subjects(&report), vec!["T-0002"]);

    let message = report.to_string();
    assert!(
        message.contains("T-0002") && message.contains("T-0009"),
        "the refusal must name the task and the dependency it cannot resolve: {message}"
    );
}

// ---------------------------------------------------------------------------
// §3.7 — dependency cycles
// ---------------------------------------------------------------------------

#[test]
fn a_dependency_cycle_is_refused_even_though_every_id_in_it_resolves() {
    let report = refused("dependency_cycle.yaml");

    // Both defects are reported, and that is not double-counting: **every cycle
    // contains at least one forward edge**, because some member of it must be
    // declared before the task that depends on it. So the two rules necessarily
    // co-occur here, and the assertion is "the cycle is named", not "the cycle
    // is the only thing named".
    //
    // The relationship is worth stating: once forward dependencies are refused,
    // a multi-task cycle is impossible by construction, and `dependency_cycles`
    // survives to catch the one case that is not a forward edge — a task that
    // depends on itself. It is defence in depth, not redundancy.
    assert!(
        kinds(&report).contains(&"dependency_cycle"),
        "the cycle must be named: {:?}",
        kinds(&report)
    );

    let message = report.to_string();
    assert!(
        message.contains("T-0001") && message.contains("T-0002"),
        "the refusal must name the tasks in the cycle so a human can break it: {message}"
    );
}

#[test]
fn a_dependency_pointing_at_a_task_declared_later_is_refused() {
    // §3.7's list says `plan validate` refuses "forward dependencies", and that
    // is a *different and stricter* rule than cycle detection — which is why it
    // has its own fixture rather than sharing the cycle one.
    //
    // `forward_dependency.yaml` is deliberately **acyclic**: T-0001 depends on
    // T-0002 and T-0002 depends on nothing. A validator that only looks for
    // cycles accepts it happily. So this fixture is precisely the difference
    // between the rule the master plan names and the rule that is easier to
    // write, and it is the reason the two are not collapsed.
    //
    // Why the strict rule is the right one to keep: it makes acyclicity
    // *structural* rather than merely checked — a graph whose every edge points
    // backwards cannot contain a cycle — and it makes a plan readable in the
    // order a human will execute it. It also settles a question the content
    // hash would otherwise leave open: if declaration order is semantically
    // constrained, then reordering tasks IS a semantic change, and invalidating
    // an approval on reorder is correct rather than incidental.
    let report = refused("forward_dependency.yaml");

    assert_eq!(kinds(&report), vec!["forward_dependency"]);

    let message = report.to_string();
    assert!(
        message.contains("T-0001") && message.contains("T-0002"),
        "the refusal must name both ends so a human can reorder them: {message}"
    );
}

#[test]
fn a_task_that_depends_on_itself_is_refused_as_a_cycle_of_one() {
    let report = refused("self_dependency.yaml");

    assert_eq!(kinds(&report), vec!["dependency_cycle"]);
    assert!(
        report.to_string().contains("T-0002"),
        "the refusal must name the task: {report}"
    );
}

// ---------------------------------------------------------------------------
// §3.7 — verification ids absent from verification.yaml
// ---------------------------------------------------------------------------

#[test]
fn a_criterion_bound_to_a_check_no_profile_defines_is_refused() {
    let report = refused("unknown_verification_id.yaml");

    assert_eq!(kinds(&report), vec!["unknown_verification_id"]);

    let message = report.to_string();
    assert!(
        message.contains("does-not-exist") && message.contains("AC-1"),
        "the refusal must name the check and the criterion that binds to it: {message}"
    );
}

// ---------------------------------------------------------------------------
// §3.7 — the one that matters most
// ---------------------------------------------------------------------------

#[test]
fn an_acceptance_criterion_bound_to_nothing_is_a_hard_error_and_not_a_warning() {
    let report = refused("unbound_criterion.yaml");

    assert_eq!(
        kinds(&report),
        vec!["unbound_criterion"],
        "prose with no binding must refuse the plan outright"
    );
    assert_eq!(subjects(&report), vec!["AC-3"]);

    let message = report.to_string();
    assert!(
        message.contains("T-0002") && message.contains("AC-3"),
        "the refusal must name the task and the criterion: {message}"
    );
    assert!(
        message.contains("manual"),
        "the refusal must point at §3.7's escape hatch, or the author's only \
         option is to delete the criterion: {message}"
    );
}

#[test]
fn the_same_criterion_declared_manual_validates_and_forces_a_review_boundary() {
    // `manual_criterion.yaml` is `unbound_criterion.yaml` plus `manual: true`
    // and nothing else. One key is the difference between refusal and
    // acceptance, and acceptance must not be silent.
    let validated = validated("manual_criterion.yaml");

    assert!(
        validated.requires_human_review(),
        "a manual criterion must force a review boundary, not merely be tolerated"
    );

    let manual = validated.manual_criteria();
    assert_eq!(manual.len(), 1);
    assert_eq!(manual[0].task, "T-0002");
    assert_eq!(manual[0].criterion, "AC-3");
}

#[test]
fn a_criterion_that_binds_to_a_check_is_not_reported_as_needing_a_human() {
    // The mirror of the test above: `manual` must be the thing that puts a
    // criterion on the review boundary, not "has a criterion".
    let validated = validated("valid.yaml");
    assert!(validated.manual_criteria().is_empty());
}

// ---------------------------------------------------------------------------
// Ids that address nothing
// ---------------------------------------------------------------------------

#[test]
fn a_blank_task_id_is_refused_because_it_addresses_nothing() {
    let report = refused("empty_id.yaml");
    assert_eq!(kinds(&report), vec!["empty_id"]);
    assert!(
        report.to_string().contains("task"),
        "the refusal must say which kind of id is blank, since it cannot quote \
         the id itself: {report}"
    );
}

// ---------------------------------------------------------------------------
// S11 task 1 — task actions and per-task execution requirements
//
// §4.3's binding rule — "a task whose policy can produce an approval gate may
// not run unattended below tier A" — is decided by
// `approval::gate::unattended_requirements(policy, actions)`, and until now
// no plan task could declare `actions` for it to read. §4.2 also names the
// requirement vector as living in `.conductor/project.yaml`, "or per-task
// override". These tests are the two fields that close both gaps.
// ---------------------------------------------------------------------------

#[test]
fn a_task_declaring_a_taxonomy_action_validates_and_the_action_is_reachable_from_the_validated_plan()
 {
    let validated = validated("actions_declared.yaml");
    let t0001 = validated
        .tasks()
        .find(|task| task.id == "T-0001")
        .expect("T-0001 is declared");
    assert_eq!(
        t0001.actions,
        vec!["git.push".to_string()],
        "a validated plan must expose the same actions the document declared"
    );
}

#[test]
fn declaring_an_action_changes_the_content_hash() {
    let without_actions = plan::content_hash(&fixture("valid.yaml")).expect("hash");
    let with_actions = plan::content_hash(&fixture("actions_declared.yaml")).expect("hash");
    assert_ne!(
        without_actions, with_actions,
        "§3.6: changing any field must invalidate approval, and `actions` is a \
         modelled field like any other — going from no actions to `[git.push]` \
         must not be invisible to the hash"
    );
}

#[test]
fn a_blank_action_string_is_refused_and_the_refusal_names_the_task() {
    let report = refused("empty_action.yaml");
    assert_eq!(kinds(&report), vec!["empty_action"]);
    assert!(
        report.to_string().contains("T-0001"),
        "the refusal must name the task the blank action belongs to: {report}"
    );
}

#[test]
fn an_action_name_outside_the_taxonomy_validates() {
    // Not a §3.7 refusal, and deliberately so: §3.7's list does not name
    // "unknown action", and §4.4 already floors an unrecognised action at
    // `deny` when it is evaluated — `Action::parse` is infallible and turns
    // any name outside the twenty-two-entry taxonomy into `Action::Unknown`,
    // which `Action::floor()` denies. Refusing it a second time here, at
    // validate, would be a second gate on the same question that can drift
    // from the first (e.g. the taxonomy grows and only one of the two rules
    // is updated to match) — so this is fail-closed at the point that
    // actually runs the action, not permissive for having stayed silent at
    // validate.
    let validated = validated("unknown_action.yaml");
    assert_eq!(
        validated.tasks().next().expect("a task").actions,
        vec!["telepathy.persuade".to_string()]
    );
}

#[test]
fn a_malformed_execution_requirements_override_is_refused_and_names_the_task() {
    let report = refused("malformed_execution_requirements.yaml");
    assert_eq!(kinds(&report), vec!["malformed_execution_requirements"]);
    assert!(
        report.to_string().contains("T-0001"),
        "the refusal must name the task whose override is broken: {report}"
    );
}

// ---------------------------------------------------------------------------
// Negative results are what people debug
// ---------------------------------------------------------------------------

#[test]
fn every_defect_is_reported_and_not_only_the_first() {
    let report = refused("many_defects.yaml");
    let mut found = kinds(&report);
    found.sort_unstable();
    found.dedup();

    assert_eq!(
        found,
        vec![
            "dangling_dependency",
            "duplicate_id",
            "unbound_criterion",
            "unknown_verification_id",
        ],
        "a plan with four kinds of defect must be told about all four in one \
         pass: {report}"
    );
}

// ---------------------------------------------------------------------------
// §3.6 — hash semantics, not bytes
// ---------------------------------------------------------------------------

#[test]
fn reformatting_a_plan_does_not_change_its_content_hash() {
    let original = plan::content_hash(&fixture("valid.yaml")).expect("hash");
    let reformatted = plan::content_hash(&fixture("valid_reformatted.yaml")).expect("hash");

    assert_ne!(
        fixture("valid.yaml"),
        fixture("valid_reformatted.yaml"),
        "the two fixtures must actually differ as bytes, or this test is vacuous"
    );
    assert_eq!(
        original, reformatted,
        "§3.6: reformatting must not invalidate approval"
    );
}

#[test]
fn reformatting_a_plan_does_not_change_the_plan_it_parses_to() {
    assert_eq!(
        parsed("valid.yaml"),
        parsed("valid_reformatted.yaml"),
        "if the models differ, the equal hashes above would be hiding a real \
         difference rather than proving there is none"
    );
}

#[test]
fn changing_a_single_field_changes_the_content_hash() {
    let original = plan::content_hash(&fixture("valid.yaml")).expect("hash");
    let changed = plan::content_hash(&fixture("field_changed.yaml")).expect("hash");

    assert_ne!(
        original, changed,
        "§3.6: changing any field must invalidate approval — and the field this \
         fixture changes is a scope glob, which is a write permission"
    );
}

#[test]
fn the_same_plan_hashes_the_same_way_every_time() {
    let text = fixture("valid.yaml");
    assert_eq!(
        plan::content_hash(&text).expect("hash"),
        plan::content_hash(&text).expect("hash")
    );
}

#[test]
fn the_content_hash_is_a_blake3_digest_as_the_commit_trailer_requires() {
    // §3.4's trailer is `Conductor-Plan: v3@blake3:9ac2…`. ADR-0007 makes
    // blake3 the only digest in Conductor, so the rendered form is part of the
    // contract rather than a display detail.
    let hash = plan::content_hash(&fixture("valid.yaml")).expect("hash");
    assert!(
        hash.as_str().starts_with("blake3:"),
        "unexpected hash form: {hash}"
    );
    assert_eq!(hash.as_str().len(), "blake3:".len() + 64);
}

// ---------------------------------------------------------------------------
// Forward compatibility, and the hole it must not open
// ---------------------------------------------------------------------------

#[test]
fn a_field_this_conductor_does_not_know_still_loads() {
    let plan = parsed("future_field.yaml");
    assert_eq!(plan.tasks().count(), 2);
}

#[test]
fn a_field_this_conductor_does_not_know_still_changes_the_content_hash() {
    let original = plan::content_hash(&fixture("valid.yaml")).expect("hash");
    let extended = plan::content_hash(&fixture("future_field.yaml")).expect("hash");

    assert_ne!(
        original, extended,
        "§3.3 puts .conductor/ inside the agent's own workspace; a key this \
         version ignores must still be covered by the approval, or appending \
         one is a way to change an approved plan without changing its hash"
    );
}

// ---------------------------------------------------------------------------
// Parse-level refusals
// ---------------------------------------------------------------------------

#[test]
fn a_document_that_is_not_a_plan_is_refused_rather_than_read_as_an_empty_plan() {
    let error = plan::parse("policy:\n  rules: []\n").expect_err("must be refused");
    assert!(
        error.to_string().contains("plan"),
        "the error must say what was missing: {error}"
    );
}

#[test]
fn a_plan_with_no_version_is_refused() {
    let error = plan::parse("plan:\n  id: p-fixture\n  milestones: []\n")
        .expect_err("a plan with no version is not a plan version");
    assert!(
        error.to_string().contains("version"),
        "the error must name the missing field: {error}"
    );
}

#[test]
fn text_that_is_not_yaml_at_all_is_refused() {
    assert!(plan::parse("\tthis: [is: not: yaml").is_err());
}

// ---------------------------------------------------------------------------
// §5.2 — the plan lifecycle
// ---------------------------------------------------------------------------

#[test]
fn the_plan_states_are_the_five_the_state_machine_names() {
    // Asserted about `conductor_core::PlanVersionState`, which is the *only*
    // spelling of §5.1's `plan_version.state` column — `plan::ledger` reads and
    // writes it and nothing else defines one. The plan module carried a second,
    // identical enum until S11; this test asserted §5.2's five states about the
    // copy nobody used, which is the failure mode two representations always
    // have. Same intent, pointed at the enum that is actually persisted.
    let spellings: Vec<&str> = PlanVersionState::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        spellings,
        vec![
            "DRAFT",
            "VALIDATED",
            "AWAITING_APPROVAL",
            "APPROVED",
            "SUPERSEDED"
        ]
    );
    for state in PlanVersionState::ALL {
        assert_eq!(
            state.as_str().parse::<PlanVersionState>().ok(),
            Some(*state)
        );
    }
    assert!(
        "APPROVE".parse::<PlanVersionState>().is_err(),
        "a state Conductor cannot name must never be read as DRAFT, which is \
         the state that permits editing"
    );
}

#[test]
fn a_plan_document_cannot_declare_itself_approved() {
    // §3.3: `.conductor/` is inside the agent's workspace, so an agent can
    // write `plan.yaml`. If `state:` were a field of the document, writing
    // `state: APPROVED` would be self-approval. The key is not modelled, and
    // — like any other unmodelled key — it is inert.
    let text = fixture("valid.yaml").replace("plan:\n", "plan:\n  state: APPROVED\n");
    let plan = plan::parse(&text).expect("the document still loads");
    let validated = plan::validate(&plan, &catalogue()).expect("and still validates");

    // Nothing in the loaded plan carries approval: a plan's state is not
    // reachable from a parsed document at all, and the ledger is what holds it.
    assert_eq!(validated.plan(), &parsed("valid.yaml"));

    // And the forgery does not inherit the original's approval either, because
    // the hash covers the whole document.
    assert_ne!(
        plan::content_hash(&text).expect("hash"),
        plan::content_hash(&fixture("valid.yaml")).expect("hash")
    );
}
