//! S7 — the policy algebra and its two-stage evaluation, master plan §4.4.
//!
//! # What these tests are trying to break
//!
//! §4.4 is a security decision procedure, so the interesting assertions are all
//! negative: a thing that must *not* be permitted. A negative assertion passes
//! for free if the fixture never actually attempts the forbidden move, which is
//! exactly how S2's isolation test and S5's row 22 passed while proving nothing.
//!
//! Every refusal test here is therefore paired with a **positive control**: the
//! same fixture with the one mechanism under test removed, asserting that the
//! forbidden move *succeeds*. If the control ever stops succeeding, the refusal
//! test has become vacuous and both fail together.

use conductor_run::policy::evaluate::{Request, evaluate};
use conductor_run::policy::facts::{self, key};
use conductor_run::policy::model::{
    Action, ActionPattern, BuiltinInvariant, Effect, Fact, FactSet, FactSource, Origin,
    PolicyDocument, PolicyException, ResolvedPolicy, Rule, Scope,
};

const A: Effect = Effect::Allow;
const R: Effect = Effect::RequireApproval;
const D: Effect = Effect::Deny;

/// The action the precedence matrix ranges over.
///
/// Deliberately one that **no built-in invariant governs**: the matrix is
/// measuring the interaction of ceiling, global, project, task and exception,
/// and an action carrying an unconditional built-in `deny` floor would pin every
/// cell to `deny` and hide every one of those interactions.
const MATRIX_ACTION: &str = "deployment.execute";

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn rule(id: &str, origin: Origin, pattern: &str, effect: Effect, locked: bool) -> Rule {
    Rule {
        id: id.to_string(),
        origin,
        pattern: ActionPattern::parse(pattern).expect("a valid action pattern"),
        effect,
        scope: Scope::everywhere(),
        locked,
        when: Vec::new(),
    }
}

fn doc(origin: Origin, rules: Vec<Rule>, exceptions: Vec<PolicyException>) -> PolicyDocument {
    PolicyDocument::new(origin, rules, exceptions).expect("a well-formed document")
}

fn scope_of(pairs: &[(&str, &str)]) -> Scope {
    Scope::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
}

fn exception(id: &str, origin: Origin, action: &str, effect: Effect) -> PolicyException {
    PolicyException {
        id: id.to_string(),
        origin,
        action: Action::parse(action),
        effect,
        scope: scope_of(&[("run", "r-0001")]),
        expires_at_ms: 10_000,
    }
}

/// Evaluate `action` under `policy`, in the scope the matrix exception names.
fn decide(policy: &ResolvedPolicy, action: &str) -> Effect {
    evaluate(
        policy,
        &Request::new(Action::parse(action), 1_000).with_context("run", "r-0001"),
    )
    .effect
}

// ---------------------------------------------------------------------------
// the algebra
// ---------------------------------------------------------------------------

#[test]
fn effects_are_totally_ordered_allow_below_require_approval_below_deny() {
    // §4.4: `allow  <  require_approval  <  deny`, and "the join is `max`".
    assert!(A < R);
    assert!(R < D);
    assert!(A < D);

    // The join must be literally `max` — not a hand-written match that can drift
    // away from the ordering it is supposed to implement.
    for a in Effect::ALL {
        for b in Effect::ALL {
            assert_eq!(
                a.join(*b),
                *a.max(b),
                "join({a:?}, {b:?}) disagrees with Ord::max"
            );
            assert_eq!(a.join(*b), b.join(*a), "join must be commutative");
        }
    }
}

/// The join, written out as a literal table.
///
/// Deliberately **not** `max`: an expectation computed with the implementation's
/// own operator would agree with any operator the implementation happened to
/// use, including a wrong one.
fn join2(a: Effect, b: Effect) -> Effect {
    const TABLE: [[Effect; 3]; 3] = [
        //        allow  require  deny
        /* allow   */ [A, R, D],
        /* require */ [R, R, D],
        /* deny    */ [D, D, D],
    ];
    TABLE[idx(a)][idx(b)]
}

/// The meet, likewise literal. An exception may only *lower* an effect, so the
/// exception step is a meet against the joined value.
fn meet2(a: Effect, b: Effect) -> Effect {
    const TABLE: [[Effect; 3]; 3] = [
        //        allow  require  deny
        /* allow   */ [A, A, A],
        /* require */ [A, R, R],
        /* deny    */ [A, R, D],
    ];
    TABLE[idx(a)][idx(b)]
}

fn idx(e: Effect) -> usize {
    match e {
        Effect::Allow => 0,
        Effect::RequireApproval => 1,
        Effect::Deny => 2,
    }
}

#[test]
fn the_literal_join_and_meet_tables_are_the_ones_the_matrix_needs() {
    // A control on the controls: if these hand-written tables were wrong, every
    // matrix cell would be checked against the wrong expectation and the whole
    // table test would be worthless. Twelve values, written out by hand.
    assert_eq!(join2(A, A), A);
    assert_eq!(join2(A, R), R);
    assert_eq!(join2(A, D), D);
    assert_eq!(join2(R, D), D);
    assert_eq!(join2(R, R), R);
    assert_eq!(join2(D, D), D);
    assert_eq!(meet2(A, D), A);
    assert_eq!(meet2(R, D), R);
    assert_eq!(meet2(D, D), D);
    assert_eq!(meet2(D, A), A);
    assert_eq!(meet2(R, A), A);
    assert_eq!(meet2(A, A), A);
}

// ---------------------------------------------------------------------------
// the precedence matrix — every cell
// ---------------------------------------------------------------------------

/// §4.4's two stages, over every combination of the five sources.
///
/// Five positions — locked global (the Stage-1 ceiling), unlocked global,
/// project, task, and a scoped exception — each either absent or carrying one of
/// the three effects: `4^5 = 1024` cells, all asserted, with the count itself
/// asserted so that a future edit cannot quietly shrink the sweep.
#[test]
fn every_cell_of_the_precedence_matrix() {
    let choices = [None, Some(A), Some(R), Some(D)];
    let mut cells = 0usize;

    for ceiling in choices {
        for global in choices {
            for project in choices {
                for task in choices {
                    for exception_effect in choices {
                        let mut global_rules = Vec::new();
                        if let Some(effect) = ceiling {
                            global_rules.push(rule(
                                "global.locked",
                                Origin::Global,
                                MATRIX_ACTION,
                                effect,
                                true,
                            ));
                        }
                        if let Some(effect) = global {
                            global_rules.push(rule(
                                "global.default",
                                Origin::Global,
                                MATRIX_ACTION,
                                effect,
                                false,
                            ));
                        }
                        let project_rules = project
                            .map(|effect| {
                                vec![rule(
                                    "project.rule",
                                    Origin::Project,
                                    MATRIX_ACTION,
                                    effect,
                                    false,
                                )]
                            })
                            .unwrap_or_default();
                        let task_rules = task
                            .map(|effect| {
                                vec![rule(
                                    "task.rule",
                                    Origin::Task,
                                    MATRIX_ACTION,
                                    effect,
                                    false,
                                )]
                            })
                            .unwrap_or_default();
                        let exceptions = exception_effect
                            .map(|effect| {
                                vec![exception(
                                    "project.exception",
                                    Origin::Project,
                                    MATRIX_ACTION,
                                    effect,
                                )]
                            })
                            .unwrap_or_default();

                        let policy = ResolvedPolicy::new(
                            Some(doc(Origin::Global, global_rules, Vec::new())),
                            Some(doc(Origin::Project, project_rules, exceptions)),
                            Some(doc(Origin::Task, task_rules, Vec::new())),
                        )
                        .expect("a well-formed policy");

                        // Expected, from the literal tables and §4.4's prose:
                        //   Stage 1: the ceiling is the join of the locked rules.
                        //   Stage 2: join everything, then let a matching,
                        //            unexpired exception lower the result — but
                        //            never below the ceiling.
                        let ceiling_effect = ceiling.unwrap_or(A);
                        let joined = join2(
                            join2(
                                join2(ceiling_effect, global.unwrap_or(A)),
                                project.unwrap_or(A),
                            ),
                            task.unwrap_or(A),
                        );
                        let expected = match exception_effect {
                            None => joined,
                            Some(x) => meet2(joined, join2(x, ceiling_effect)),
                        };

                        let got = decide(&policy, MATRIX_ACTION);
                        assert_eq!(
                            got, expected,
                            "cell(ceiling={ceiling:?}, global={global:?}, \
                             project={project:?}, task={task:?}, exception={exception_effect:?})"
                        );
                        cells += 1;
                    }
                }
            }
        }
    }

    assert_eq!(cells, 1024, "the matrix must be swept exhaustively");
}

/// Twelve cells written out by hand, as a control on the generator above.
///
/// The exhaustive sweep derives its expectation from a formula. If that formula
/// were wrong in the same way the implementation is wrong, 1024 cells would
/// agree and prove nothing. These twelve are read straight off §4.4's prose.
#[test]
fn named_cells_read_straight_off_the_specification() {
    struct Cell {
        what: &'static str,
        ceiling: Option<Effect>,
        global: Option<Effect>,
        project: Option<Effect>,
        task: Option<Effect>,
        exception: Option<Effect>,
        expected: Effect,
    }
    let cells = [
        Cell {
            what: "nothing says anything",
            ceiling: None,
            global: None,
            project: None,
            task: None,
            exception: None,
            expected: A,
        },
        Cell {
            what: "a project may tighten an allow to a deny",
            ceiling: None,
            global: Some(A),
            project: Some(D),
            task: None,
            exception: None,
            expected: D,
        },
        Cell {
            what: "a project rule cannot loosen a global deny",
            ceiling: None,
            global: Some(D),
            project: Some(A),
            task: None,
            exception: None,
            expected: D,
        },
        Cell {
            what: "a task constraint may tighten",
            ceiling: None,
            global: Some(A),
            project: Some(A),
            task: Some(R),
            exception: None,
            expected: R,
        },
        Cell {
            what: "an exception lowers an unlocked require_approval to allow",
            ceiling: None,
            global: Some(R),
            project: None,
            task: None,
            exception: Some(A),
            expected: A,
        },
        Cell {
            what: "the same exception is clamped by a locked ceiling",
            ceiling: Some(R),
            global: None,
            project: None,
            task: None,
            exception: Some(A),
            expected: R,
        },
        Cell {
            what: "a locked deny cannot be excepted away",
            ceiling: Some(D),
            global: None,
            project: None,
            task: None,
            exception: Some(A),
            expected: D,
        },
        Cell {
            what: "an exception may not raise an effect",
            ceiling: None,
            global: Some(A),
            project: None,
            task: None,
            exception: Some(D),
            expected: A,
        },
        Cell {
            what: "a locked allow imposes no floor of its own",
            ceiling: Some(A),
            global: Some(A),
            project: Some(A),
            task: Some(A),
            exception: None,
            expected: A,
        },
        Cell {
            what: "the ceiling still participates in the join",
            ceiling: Some(R),
            global: Some(A),
            project: Some(A),
            task: Some(A),
            exception: None,
            expected: R,
        },
        Cell {
            what: "an exception lowers only as far as the ceiling",
            ceiling: Some(R),
            global: Some(D),
            project: None,
            task: None,
            exception: Some(A),
            expected: R,
        },
        Cell {
            what: "an exception at require_approval under a locked require_approval",
            ceiling: Some(R),
            global: Some(D),
            project: None,
            task: None,
            exception: Some(R),
            expected: R,
        },
    ];

    for cell in cells {
        let mut global_rules = Vec::new();
        if let Some(effect) = cell.ceiling {
            global_rules.push(rule(
                "global.locked",
                Origin::Global,
                MATRIX_ACTION,
                effect,
                true,
            ));
        }
        if let Some(effect) = cell.global {
            global_rules.push(rule(
                "global.default",
                Origin::Global,
                MATRIX_ACTION,
                effect,
                false,
            ));
        }
        let policy = ResolvedPolicy::new(
            Some(doc(Origin::Global, global_rules, Vec::new())),
            Some(doc(
                Origin::Project,
                cell.project
                    .map(|e| {
                        vec![rule(
                            "project.rule",
                            Origin::Project,
                            MATRIX_ACTION,
                            e,
                            false,
                        )]
                    })
                    .unwrap_or_default(),
                cell.exception
                    .map(|e| {
                        vec![exception(
                            "project.exception",
                            Origin::Project,
                            MATRIX_ACTION,
                            e,
                        )]
                    })
                    .unwrap_or_default(),
            )),
            Some(doc(
                Origin::Task,
                cell.task
                    .map(|e| vec![rule("task.rule", Origin::Task, MATRIX_ACTION, e, false)])
                    .unwrap_or_default(),
                Vec::new(),
            )),
        )
        .expect("a well-formed policy");

        assert_eq!(
            decide(&policy, MATRIX_ACTION),
            cell.expected,
            "{}",
            cell.what
        );
    }
}

// ---------------------------------------------------------------------------
// mechanism 1 — the locked ceiling, with its positive control
// ---------------------------------------------------------------------------

/// Build a policy where the project tries to loosen a global rule via a scoped
/// exception. `locked` decides whether the global rule forms a Stage-1 ceiling.
fn loosening_attempt(locked: bool) -> ResolvedPolicy {
    ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule(
                "global.deployment",
                Origin::Global,
                "deployment.execute",
                R,
                locked,
            )],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            Vec::new(),
            vec![exception(
                "project.ship-it",
                Origin::Project,
                "deployment.execute",
                A,
            )],
        )),
        None,
    )
    .expect("a well-formed policy")
}

#[test]
fn a_locked_ceiling_stops_a_project_loosening_past_it() {
    let policy = loosening_attempt(true);
    assert_eq!(
        decide(&policy, "deployment.execute"),
        R,
        "a locked global rule is a ceiling: nothing below it may exceed it (§4.4 Stage 1)"
    );
}

#[test]
fn positive_control_the_same_loosening_succeeds_without_the_lock() {
    // Without this, the test above is vacuous: it would pass even if the
    // exception path were simply broken, or if the exception never matched at
    // all. Here the *only* difference is `locked`, and the outcome flips — so
    // the lock is demonstrably what stopped the loosening.
    let policy = loosening_attempt(false);
    assert_eq!(
        decide(&policy, "deployment.execute"),
        A,
        "an unlocked global rule must be loosenable by a scoped exception, \
         otherwise the locked case proves nothing"
    );
}

#[test]
fn a_project_rule_cannot_loosen_and_a_project_rule_can_tighten() {
    let loosen = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "release.publish", D, false)],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            vec![rule("p", Origin::Project, "release.publish", A, false)],
            Vec::new(),
        )),
        None,
    )
    .expect("policy");
    assert_eq!(decide(&loosen, "release.publish"), D);

    // Positive control on the fixture itself: the project rule is not inert —
    // reverse the two effects and the project's value is the one that wins.
    let tighten = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "release.publish", A, false)],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            vec![rule("p", Origin::Project, "release.publish", D, false)],
            Vec::new(),
        )),
        None,
    )
    .expect("policy");
    assert_eq!(decide(&tighten, "release.publish"), D);
}

#[test]
fn a_locked_rule_outside_the_global_document_is_refused() {
    // A project that could mint its own ceiling would be a project that could
    // raise its own ceiling. Refused at construction, not at evaluation.
    let err = PolicyDocument::new(
        Origin::Project,
        vec![rule("p", Origin::Project, "git.push", D, true)],
        Vec::new(),
    )
    .expect_err("a locked project rule must be refused");
    let message = err.to_string();
    assert!(message.contains("locked"), "unhelpful message: {message}");
}

#[test]
fn a_locked_rule_smuggled_into_the_project_slot_still_cannot_raise_the_ceiling() {
    // Defence in depth: even if `PolicyDocument::new` were bypassed, the ceiling
    // is computed from the *global* document alone, so a locked rule sitting in
    // the project document contributes nothing to Stage 1.
    let policy = ResolvedPolicy::new(
        Some(doc(Origin::Global, Vec::new(), Vec::new())),
        Some(PolicyDocument::new_unchecked(
            Origin::Project,
            vec![Rule {
                locked: true,
                ..rule("p.locked", Origin::Project, "deployment.execute", R, false)
            }],
            vec![exception(
                "p.exception",
                Origin::Project,
                "deployment.execute",
                A,
            )],
        )),
        None,
    )
    .expect("policy");

    let decision = evaluate(
        &policy,
        &Request::new(Action::parse("deployment.execute"), 1_000).with_context("run", "r-0001"),
    );
    assert_eq!(
        decision.ceiling, A,
        "only the global document may set a ceiling"
    );
    assert_eq!(
        decision.effect, A,
        "and the exception is therefore unconstrained by it"
    );
}

// ---------------------------------------------------------------------------
// mechanism 2 — unknown → deny, with its positive control
// ---------------------------------------------------------------------------

#[test]
fn an_action_outside_the_taxonomy_denies() {
    // §4.4: "the taxonomy will be incomplete on day one and incompleteness must
    // not read as permission".
    let policy = ResolvedPolicy::new(None, None, None).expect("an empty policy");
    let decision = evaluate(&policy, &Request::new(Action::parse("quantum.entangle"), 0));

    assert_eq!(decision.effect, D);
    assert!(decision.unknown_action);
    assert!(
        matches!(decision.action, Action::Unknown(ref s) if s == "quantum.entangle"),
        "the unrecognised name must survive into the decision so `explain` can print it"
    );
}

#[test]
fn positive_control_the_same_empty_policy_allows_a_known_action() {
    // Proves the deny above comes from the action being unknown and not from an
    // empty policy denying everything.
    let policy = ResolvedPolicy::new(None, None, None).expect("an empty policy");
    assert_eq!(decide(&policy, "deployment.execute"), A);
}

#[test]
fn an_exception_cannot_allow_an_unknown_action() {
    let policy = ResolvedPolicy::new(
        None,
        Some(doc(
            Origin::Project,
            Vec::new(),
            vec![exception("x", Origin::Project, "quantum.entangle", A)],
        )),
        None,
    )
    .expect("policy");
    assert_eq!(decide(&policy, "quantum.entangle"), D);

    // Positive control: the very same exception, against a known action, does
    // lower the effect — so the deny above is the unknown-action floor and not
    // an exception that silently never applies.
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "deployment.execute", R, false)],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            Vec::new(),
            vec![exception("x", Origin::Project, "deployment.execute", A)],
        )),
        None,
    )
    .expect("policy");
    assert_eq!(decide(&policy, "deployment.execute"), A);
}

#[test]
fn every_action_named_in_the_specification_parses_to_a_known_variant() {
    // §4.4's typed-action block, verbatim.
    const TAXONOMY: &[&str] = &[
        "git.commit.local",
        "git.push",
        "git.remote.modify",
        "git.branch.delete",
        "git.force_push",
        "dependency.add.runtime",
        "dependency.add.dev",
        "dependency.remove",
        "lockfile.modify",
        "database.migration.create",
        "database.migration.apply",
        "database.destructive_change",
        "filesystem.write.outside_workspace",
        "network.external_access",
        "credential.access",
        "deployment.execute",
        "release.publish",
        "architecture.change",
        "authentication.change",
        "authorization.change",
        "billing.spend",
        "service.paid_addition",
    ];
    assert_eq!(TAXONOMY.len(), 22);
    assert_eq!(Action::KNOWN.len(), 22);

    for name in TAXONOMY {
        let action = Action::parse(name);
        assert!(action.is_known(), "{name} did not parse to a known action");
        assert_eq!(action.as_str(), *name, "round trip");
    }
    // And nothing outside it is known — including near-misses.
    for name in ["git.pushh", "dependency.add", "", "GIT.PUSH"] {
        assert!(
            !Action::parse(name).is_known(),
            "{name:?} must not be treated as a known action"
        );
    }
}

// ---------------------------------------------------------------------------
// mechanism 3 — a deny must rest only on deterministic facts
// ---------------------------------------------------------------------------

fn deny_on_fact(source: FactSource) -> conductor_run::policy::evaluate::Decision {
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![Rule {
                when: vec![key::ARCHITECTURE_CHANGE.to_string()],
                ..rule("g.arch", Origin::Global, "architecture.change", D, false)
            }],
            Vec::new(),
        )),
        None,
        None,
    )
    .expect("policy");

    let fact = match source {
        FactSource::Deterministic => Fact::deterministic(key::ARCHITECTURE_CHANGE, "crates/**"),
        FactSource::ModelAssisted => Fact::model_assisted(key::ARCHITECTURE_CHANGE, "crates/**"),
        FactSource::Human => Fact::human(key::ARCHITECTURE_CHANGE, "crates/**"),
    };
    evaluate(
        &policy,
        &Request::new(Action::parse("architecture.change"), 0)
            .with_facts(FactSet::from_iter([fact])),
    )
}

#[test]
fn a_deny_resting_on_a_model_assisted_fact_is_capped_at_require_approval() {
    // §4.4: "a `deny` must rest only on `deterministic` facts … a model must
    // never be the sole reason Conductor blocks work".
    let decision = deny_on_fact(FactSource::ModelAssisted);

    assert_eq!(decision.effect, R);
    assert_eq!(
        decision.caps.len(),
        1,
        "the cap must be recorded, not silent"
    );
    assert_eq!(decision.caps[0].fact_key, key::ARCHITECTURE_CHANGE);
    assert_eq!(decision.caps[0].fact_source, FactSource::ModelAssisted);
    assert_eq!(decision.caps[0].source_id, "g.arch");
}

#[test]
fn positive_control_the_same_rule_denies_on_a_deterministic_fact() {
    // Without this the cap test is vacuous: a rule that never matched at all
    // would also fail to produce a deny.
    let decision = deny_on_fact(FactSource::Deterministic);
    assert_eq!(decision.effect, D);
    assert!(decision.caps.is_empty());
}

#[test]
fn a_human_sourced_fact_also_cannot_carry_a_deny() {
    // §4.4 is explicit: *only* `deterministic`. A human assertion is evidence
    // for asking, not for blocking without recourse.
    let decision = deny_on_fact(FactSource::Human);
    assert_eq!(decision.effect, R);
    assert_eq!(decision.caps[0].fact_source, FactSource::Human);
}

#[test]
fn a_require_approval_may_rest_on_any_fact_source() {
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![Rule {
                when: vec![key::ARCHITECTURE_CHANGE.to_string()],
                ..rule("g.arch", Origin::Global, "architecture.change", R, false)
            }],
            Vec::new(),
        )),
        None,
        None,
    )
    .expect("policy");

    for fact in [
        Fact::deterministic(key::ARCHITECTURE_CHANGE, "x"),
        Fact::model_assisted(key::ARCHITECTURE_CHANGE, "x"),
        Fact::human(key::ARCHITECTURE_CHANGE, "x"),
    ] {
        let source = fact.source;
        let decision = evaluate(
            &policy,
            &Request::new(Action::parse("architecture.change"), 0)
                .with_facts(FactSet::from_iter([fact])),
        );
        assert_eq!(
            decision.effect, R,
            "{source:?} must be able to carry a require_approval"
        );
        assert!(decision.caps.is_empty());
    }
}

// ---------------------------------------------------------------------------
// built-in invariants
// ---------------------------------------------------------------------------

#[test]
fn the_four_builtin_invariants_are_the_ones_the_specification_names() {
    let ids: Vec<&str> = BuiltinInvariant::ALL.iter().map(|i| i.id()).collect();
    assert_eq!(
        ids,
        [
            "builtin.never-write-outside-run-workspace",
            "builtin.never-print-a-secret-matching-value",
            "builtin.never-push-to-a-remote",
            "builtin.never-operate-on-an-unregistered-repository",
        ]
    );
}

#[test]
fn a_builtin_invariant_cannot_be_loosened_by_any_document_or_exception() {
    // Every configurable position set to its most permissive value, plus a
    // scoped exception explicitly granting `allow`, plus no locked ceiling at
    // all — the loosest policy expressible.
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "*", A, false)],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            vec![rule("p", Origin::Project, "*", A, false)],
            vec![
                exception("x1", Origin::Project, "git.push", A),
                exception("x2", Origin::Project, "git.force_push", A),
                exception(
                    "x3",
                    Origin::Project,
                    "filesystem.write.outside_workspace",
                    A,
                ),
            ],
        )),
        Some(doc(
            Origin::Task,
            vec![rule("t", Origin::Task, "*", A, false)],
            Vec::new(),
        )),
    )
    .expect("policy");

    for action in [
        "git.push",
        "git.force_push",
        "filesystem.write.outside_workspace",
    ] {
        assert_eq!(
            decide(&policy, action),
            D,
            "{action} is a built-in invariant and is not configurable at all (§4.4)"
        );
    }

    // Positive control: the identical policy, identical exception mechanism, on
    // an action no built-in invariant governs, does reach `allow`. So the denies
    // above come from the invariants and not from the policy being inert.
    assert_eq!(decide(&policy, "deployment.execute"), A);
}

#[test]
fn the_secret_invariant_denies_on_a_deterministic_secret_fact_only() {
    let policy = ResolvedPolicy::new(None, None, None).expect("policy");

    let deterministic = evaluate(
        &policy,
        &Request::new(Action::parse("git.commit.local"), 0).with_facts(FactSet::from_iter([
            Fact::deterministic(key::SECRET_MATCH, "github-token"),
        ])),
    );
    assert_eq!(deterministic.effect, D);
    assert_eq!(deterministic.invariants.len(), 1);

    // The same invariant, backed only by a model-assisted claim, is capped —
    // the deny-needs-deterministic-facts rule has no exemption for built-ins.
    let model = evaluate(
        &policy,
        &Request::new(Action::parse("git.commit.local"), 0).with_facts(FactSet::from_iter([
            Fact::model_assisted(key::SECRET_MATCH, "github-token"),
        ])),
    );
    assert_eq!(model.effect, R);
    assert_eq!(
        model.caps[0].source_id,
        "builtin.never-print-a-secret-matching-value"
    );
}

#[test]
fn an_unregistered_repository_denies_every_action() {
    let policy = ResolvedPolicy::new(None, None, None).expect("policy");
    let facts = FactSet::from_iter([Fact::deterministic(key::REPOSITORY_REGISTERED, "false")]);

    for action in Action::KNOWN {
        let decision = evaluate(
            &policy,
            &Request::new(Action::parse(action), 0).with_facts(facts.clone()),
        );
        assert_eq!(decision.effect, D, "{action} on an unregistered repository");
    }

    // Positive control: with the repository registered, the same actions are not
    // all denied — so the sweep above is measuring the invariant.
    let registered = FactSet::from_iter([Fact::deterministic(key::REPOSITORY_REGISTERED, "true")]);
    let decision = evaluate(
        &policy,
        &Request::new(Action::parse("git.commit.local"), 0).with_facts(registered),
    );
    assert_eq!(decision.effect, A);
}

// ---------------------------------------------------------------------------
// scopes, patterns and expiry
// ---------------------------------------------------------------------------

#[test]
fn an_expired_exception_does_not_apply_and_says_so() {
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "deployment.execute", R, false)],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            Vec::new(),
            vec![exception("x", Origin::Project, "deployment.execute", A)],
        )),
        None,
    )
    .expect("policy");

    // `expires_at_ms` is 10_000 in the fixture.
    let alive = evaluate(
        &policy,
        &Request::new(Action::parse("deployment.execute"), 9_999).with_context("run", "r-0001"),
    );
    assert_eq!(
        alive.effect, A,
        "positive control: it applies while unexpired"
    );

    let expired = evaluate(
        &policy,
        &Request::new(Action::parse("deployment.execute"), 10_001).with_context("run", "r-0001"),
    );
    assert_eq!(expired.effect, R);
    assert!(
        expired
            .not_matched
            .iter()
            .any(|c| c.rule_id == "x" && c.reason.to_string().contains("expired")),
        "an expired exception must be reported as expired, not silently dropped: {:?}",
        expired.not_matched
    );
}

#[test]
fn an_exception_applies_only_in_the_scope_it_names() {
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "deployment.execute", R, false)],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            Vec::new(),
            vec![exception("x", Origin::Project, "deployment.execute", A)],
        )),
        None,
    )
    .expect("policy");

    let in_scope = evaluate(
        &policy,
        &Request::new(Action::parse("deployment.execute"), 0).with_context("run", "r-0001"),
    );
    assert_eq!(in_scope.effect, A, "positive control");

    let other_run = evaluate(
        &policy,
        &Request::new(Action::parse("deployment.execute"), 0).with_context("run", "r-0002"),
    );
    assert_eq!(other_run.effect, R);

    let no_run = evaluate(
        &policy,
        &Request::new(Action::parse("deployment.execute"), 0),
    );
    assert_eq!(no_run.effect, R);
}

#[test]
fn a_wildcard_pattern_matches_a_prefix_and_nothing_else() {
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "dependency.*", R, false)],
            Vec::new(),
        )),
        None,
        None,
    )
    .expect("policy");

    for action in [
        "dependency.add.runtime",
        "dependency.add.dev",
        "dependency.remove",
    ] {
        assert_eq!(decide(&policy, action), R, "{action}");
    }
    assert_eq!(decide(&policy, "lockfile.modify"), A);
    assert_eq!(decide(&policy, "deployment.execute"), A);
}

#[test]
fn a_wildcard_exception_is_not_expressible_and_a_hand_built_one_loosens_nothing() {
    // §4.4: "if a scoped exception matches **exactly**". A wildcard exception is
    // a blanket loosening wearing an exception's clothes.
    //
    // The type is what forbids it: `PolicyException::action` is an [`Action`],
    // not an [`ActionPattern`], so `dependency.*` does not become a pattern —
    // it becomes `Action::Unknown`, which matches no real action and which
    // evaluation denies. The YAML loader refuses the spelling by name as well
    // (see `policy_snapshot.rs`), but that refusal is a courtesy: even a
    // hand-built wildcard exception cannot loosen anything.
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "dependency.*", R, false)],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            Vec::new(),
            vec![PolicyException {
                id: "x".to_string(),
                origin: Origin::Project,
                action: Action::parse("dependency.*"),
                effect: A,
                scope: scope_of(&[("run", "r-0001")]),
                expires_at_ms: 10_000,
            }],
        )),
        None,
    )
    .expect("policy");

    for action in [
        "dependency.add.runtime",
        "dependency.add.dev",
        "dependency.remove",
    ] {
        assert_eq!(
            decide(&policy, action),
            R,
            "{action} must not be loosened by a wildcard exception"
        );
    }
    // Positive control on the same fixture: an exception naming one of those
    // actions exactly *does* loosen it, so the assertions above are about the
    // wildcard and not about exceptions being broken.
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![rule("g", Origin::Global, "dependency.*", R, false)],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            Vec::new(),
            vec![exception("x", Origin::Project, "dependency.add.runtime", A)],
        )),
        None,
    )
    .expect("policy");
    assert_eq!(decide(&policy, "dependency.add.runtime"), A);
    assert_eq!(decide(&policy, "dependency.remove"), R);
}

#[test]
fn an_unscoped_exception_is_refused_because_it_is_a_rule() {
    let err = PolicyDocument::new(
        Origin::Project,
        Vec::new(),
        vec![PolicyException {
            id: "x".to_string(),
            origin: Origin::Project,
            action: Action::parse("git.commit.local"),
            effect: A,
            scope: Scope::everywhere(),
            expires_at_ms: 1,
        }],
    )
    .expect_err("an unscoped exception must be refused");
    assert!(err.to_string().contains("scope"), "{err}");
}

// ---------------------------------------------------------------------------
// explain — the negative results are the product
// ---------------------------------------------------------------------------

fn explain_fixture() -> ResolvedPolicy {
    ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![
                rule(
                    "global.locked-deploy",
                    Origin::Global,
                    "deployment.execute",
                    R,
                    true,
                ),
                rule("global.other-action", Origin::Global, "git.push", D, false),
                Rule {
                    scope: scope_of(&[("repo", "other")]),
                    ..rule(
                        "global.other-scope",
                        Origin::Global,
                        "deployment.execute",
                        D,
                        false,
                    )
                },
                Rule {
                    when: vec!["production".to_string()],
                    ..rule(
                        "global.needs-fact",
                        Origin::Global,
                        "deployment.execute",
                        D,
                        false,
                    )
                },
            ],
            Vec::new(),
        )),
        Some(doc(
            Origin::Project,
            Vec::new(),
            vec![exception(
                "project.ship-it",
                Origin::Project,
                "deployment.execute",
                A,
            )],
        )),
        None,
    )
    .expect("policy")
}

#[test]
fn explain_names_every_rule_it_considered_and_why_each_did_not_match() {
    // §4.4: "every rule that matched **and every rule considered that did not,
    // with the reason** … Negative results are what people debug."
    let policy = explain_fixture();
    let decision = evaluate(
        &policy,
        &Request::new(Action::parse("deployment.execute"), 1_000)
            .with_context("run", "r-0001")
            .with_context("repo", "acme")
            .with_facts(FactSet::from_iter([Fact::deterministic(
                key::DEPENDENCY_ADDED,
                "serde_yaml",
            )])),
    );
    let text = conductor_run::policy::explain::render(&decision);

    // The matched rule, and the ceiling it formed.
    assert!(text.contains("global.locked-deploy"), "{text}");
    assert!(text.contains("ceiling"), "{text}");

    // Every rule considered and rejected, each with its own distinct reason.
    for (id, reason) in [
        ("global.other-action", "action"),
        ("global.other-scope", "scope"),
        ("global.needs-fact", "fact"),
    ] {
        let line = text
            .lines()
            .find(|l| l.contains(id))
            .unwrap_or_else(|| panic!("{id} is missing from the explanation:\n{text}"));
        assert!(
            line.to_ascii_lowercase().contains(reason),
            "{id} was reported without saying it failed on the {reason}: {line}"
        );
    }

    // Facts, with their sources.
    assert!(text.contains(key::DEPENDENCY_ADDED), "{text}");
    assert!(text.contains("deterministic"), "{text}");

    // The policy hash and the exception, with scope and expiry.
    assert!(text.contains(&decision.policy_hash), "{text}");
    assert!(text.contains("project.ship-it"), "{text}");
    assert!(text.contains("run=r-0001"), "{text}");
    assert!(text.contains("10000"), "the expiry must be printed: {text}");
}

#[test]
fn positive_control_a_rule_that_did_match_is_not_listed_as_not_matching() {
    // The test above would pass if `explain` listed every rule as non-matching
    // with every reason. This one forbids that degenerate rendering.
    let policy = explain_fixture();
    let decision = evaluate(
        &policy,
        &Request::new(Action::parse("deployment.execute"), 1_000)
            .with_context("run", "r-0001")
            .with_context("repo", "acme"),
    );

    let matched: Vec<&str> = decision
        .matched
        .iter()
        .map(|m| m.rule_id.as_str())
        .collect();
    let not_matched: Vec<&str> = decision
        .not_matched
        .iter()
        .map(|c| c.rule_id.as_str())
        .collect();

    assert!(matched.contains(&"global.locked-deploy"), "{matched:?}");
    assert!(
        !not_matched.contains(&"global.locked-deploy"),
        "a matching rule must not also be reported as non-matching: {not_matched:?}"
    );
    assert_eq!(
        matched.len() + not_matched.len() + usize::from(decision.exception.is_some()),
        5,
        "every rule and exception in the fixture must be accounted for exactly once: \
         matched={matched:?} not_matched={not_matched:?} exception={:?}",
        decision.exception
    );
}

#[test]
fn explain_reports_a_deny_that_was_capped_and_says_why() {
    let decision = deny_on_fact(FactSource::ModelAssisted);
    let text = conductor_run::policy::explain::render(&decision);
    assert!(text.contains("model_assisted"), "{text}");
    assert!(
        text.to_ascii_lowercase().contains("capped"),
        "a capped deny must say so — otherwise the operator sees a \
         require_approval with no explanation: {text}"
    );
}

// ---------------------------------------------------------------------------
// deterministic fact extractors (§4.4's table)
// ---------------------------------------------------------------------------

#[test]
fn lockfile_paths_are_matched_by_name_and_nothing_else_is() {
    let found = facts::lockfile_modified(&[
        "Cargo.lock".to_string(),
        "crates/a/src/lib.rs".to_string(),
        "web/package-lock.json".to_string(),
        "web/pnpm-lock.yaml".to_string(),
        "uv.lock".to_string(),
        "notes/uv.lock.md".to_string(),
    ]);
    let values: Vec<&str> = found.iter().map(|f| f.value.as_str()).collect();
    assert_eq!(
        values,
        [
            "Cargo.lock",
            "web/package-lock.json",
            "web/pnpm-lock.yaml",
            "uv.lock"
        ]
    );
    assert!(found.iter().all(|f| f.source == FactSource::Deterministic));
}

#[test]
fn a_runtime_dependency_added_to_a_cargo_manifest_is_a_deterministic_fact() {
    let before = "[package]\nname = \"a\"\n\n[dependencies]\nserde = \"1\"\n";
    let after = "[package]\nname = \"a\"\n\n[dependencies]\nserde = \"1\"\nserde_yaml = \"0.9\"\n";

    let added = facts::dependency_manifest_diff("Cargo.toml", before, after);
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].key, key::DEPENDENCY_ADDED);
    assert_eq!(added[0].value, "serde_yaml");
    assert_eq!(added[0].source, FactSource::Deterministic);

    // Positive control: an unchanged manifest yields nothing, so the extractor
    // is reading the diff and not simply listing every dependency.
    assert!(facts::dependency_manifest_diff("Cargo.toml", before, before).is_empty());
}

#[test]
fn a_dev_dependency_is_not_a_runtime_dependency() {
    let before = "[dependencies]\nserde = \"1\"\n";
    let after = "[dependencies]\nserde = \"1\"\n\n[dev-dependencies]\ntempfile = \"3\"\n";
    let added = facts::dependency_manifest_diff("Cargo.toml", before, after);
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].value, "tempfile");
    assert_eq!(added[0].key, key::DEV_DEPENDENCY_ADDED);
}

#[test]
fn a_package_json_dependency_is_read_from_the_dependencies_object() {
    let before = r#"{"dependencies": {"react": "18"}}"#;
    let after =
        r#"{"dependencies": {"react": "18", "left-pad": "1"}, "devDependencies": {"vitest": "1"}}"#;
    let added = facts::dependency_manifest_diff("package.json", before, after);
    let values: Vec<(&str, &str)> = added
        .iter()
        .map(|f| (f.key.as_str(), f.value.as_str()))
        .collect();
    assert_eq!(
        values,
        [
            (key::DEPENDENCY_ADDED, "left-pad"),
            (key::DEV_DEPENDENCY_ADDED, "vitest"),
        ]
    );
}

#[test]
fn a_changed_remote_is_a_deterministic_fact_from_git_config() {
    let before = "remote.origin.url git@github.com:acme/x.git\n";
    let after = "remote.origin.url git@github.com:acme/x.git\nremote.evil.url https://evil/\n";
    let changed = facts::remotes_changed(before, after);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].key, key::REMOTE_MODIFIED);
    assert_eq!(changed[0].value, "remote.evil.url");
    assert_eq!(changed[0].source, FactSource::Deterministic);

    // Positive control.
    assert!(facts::remotes_changed(before, before).is_empty());
}

#[test]
fn a_write_outside_the_workspace_root_is_a_deterministic_fact() {
    let root = std::path::Path::new("/tmp/ws");
    let outside = facts::outside_workspace(
        root,
        &[
            std::path::PathBuf::from("/tmp/ws/src/lib.rs"),
            std::path::PathBuf::from("/tmp/elsewhere/x"),
            std::path::PathBuf::from("/tmp/ws/../escape"),
        ],
    );
    let values: Vec<&str> = outside.iter().map(|f| f.value.as_str()).collect();
    assert_eq!(values, ["/tmp/elsewhere/x", "/tmp/ws/../escape"]);
    assert!(
        outside
            .iter()
            .all(|f| f.source == FactSource::Deterministic)
    );
}

#[test]
fn a_secret_in_the_diff_is_a_deterministic_fact_and_never_carries_the_secret() {
    let diff = "+GITHUB_TOKEN=ghp_1234567890abcdefghijklmnopqrstuvwxyzAB\n";
    let found = facts::secrets_in_diff(diff);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, key::SECRET_MATCH);
    assert_eq!(found[0].source, FactSource::Deterministic);
    // The fact travels into explanations and approval requests. It must name the
    // kind and never the value.
    assert!(
        !found[0].value.contains("ghp_1234567890"),
        "the fact leaked the secret: {:?}",
        found[0]
    );
    assert!(
        found[0]
            .evidence
            .as_deref()
            .is_none_or(|e| !e.contains("ghp_1234567890")),
        "the evidence leaked the secret: {:?}",
        found[0]
    );

    assert!(facts::secrets_in_diff("+let x = 1;\n").is_empty());
}

#[test]
fn a_new_file_under_a_migration_glob_is_a_deterministic_fact() {
    let added = facts::migrations_added(
        &[
            "db/migrate/001_init.sql".to_string(),
            "src/lib.rs".to_string(),
        ],
        &["db/migrate/**".to_string()],
    );
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].value, "db/migrate/001_init.sql");
    assert_eq!(added[0].source, FactSource::Deterministic);
}

#[test]
fn architecture_change_can_only_ever_be_model_assisted() {
    // §4.4: "**not deterministic.** Path globs are a proxy → `model_assisted` →
    // `require_approval` at most, never `deny`, always with the diff attached."
    let proxies = facts::architecture_change_proxy(
        &["crates/conductor-core/src/state.rs".to_string()],
        &["crates/*/src/state.rs".to_string()],
        "--- a/state.rs\n+++ b/state.rs\n",
    );
    assert_eq!(proxies.len(), 1);
    let fact = proxies.into_iter().next().expect("one").into_fact();
    assert_eq!(fact.key, key::ARCHITECTURE_CHANGE);
    assert_eq!(
        fact.source,
        FactSource::ModelAssisted,
        "a path-glob proxy is not a deterministic fact"
    );
    assert!(
        fact.evidence.is_some(),
        "§4.4 requires the diff to travel with it"
    );

    // And the consequence the type exists to force: a rule that would deny on
    // this fact cannot.
    let policy = ResolvedPolicy::new(
        Some(doc(
            Origin::Global,
            vec![Rule {
                when: vec![key::ARCHITECTURE_CHANGE.to_string()],
                ..rule("g", Origin::Global, "architecture.change", D, false)
            }],
            Vec::new(),
        )),
        None,
        None,
    )
    .expect("policy");
    let decision = evaluate(
        &policy,
        &Request::new(Action::parse("architecture.change"), 0)
            .with_facts(FactSet::from_iter([fact])),
    );
    assert_eq!(decision.effect, R);
}
