//! Two-stage evaluation — master plan §4.4.
//!
//! ```text
//! Stage 1 — CEILING (locked policy)
//!     Locked global rules produce a maximum permissiveness.
//!     Nothing below can exceed it — not project rules, not exceptions,
//!     not a human grant. Unlocking is a separate, audited operation.
//!
//! Stage 2 — JOIN
//!     effect = max(builtin_invariant, global_default, project_rule, task_constraint)
//!     then, if a scoped exception matches exactly, is unexpired,
//!     and the Stage-1 ceiling permits it:
//!         effect = exception.effect
//! ```
//!
//! # Why the stages are not collapsed
//!
//! §4.4 line 590: *"A single 'most restrictive wins' join makes locked rules
//! peers with project rules, which means locking does no work in the one
//! direction it exists for."* The join alone can only ever tighten, so it needs
//! no ceiling to stop a project loosening. The **exception** is the construct
//! that lowers an effect, and the ceiling is what bounds it. Fold the two stages
//! together and the lock becomes decoration — which is exactly the failure the
//! ceiling test's positive control is designed to catch.
//!
//! # The three floors
//!
//! Three things bound the exception from below, and all three are also in the
//! join, so the exception can never reach past any of them:
//!
//! | floor | why |
//! |---|---|
//! | the Stage-1 ceiling | §4.4 Stage 1 |
//! | the built-in invariants | §4.4: "not configurable at all" |
//! | [`Action::floor`] | §4.4: "unknown action → `deny`" |
//!
//! # The deny cap
//!
//! §4.4: *"a `deny` must rest only on `deterministic` facts … a model must never
//! be the sole reason Conductor blocks work — a hallucinated block is
//! indistinguishable from a real one and trains the user to override blocks."*
//!
//! A rule's *supporting* facts are the ones its `when:` names. A rule with no
//! `when:` rests on no facts at all and its `deny` therefore stands: it is a
//! standing policy statement, not an inference. Built-in invariants go through
//! the same cap, with no exemption — an invariant that fired on a model-assisted
//! secret claim is still a model blocking work.

use std::collections::BTreeMap;

use serde::Serialize;

use super::model::{
    Action, AppliedInvariant, BuiltinInvariant, Effect, Fact, FactSet, FactSource, Origin,
    PolicyException, ResolvedPolicy, Rule, Scope, ScopeMatch,
};

/// One question for the policy engine.
#[derive(Debug, Clone)]
pub struct Request {
    /// What Conductor is about to do.
    pub action: Action,
    /// What it knows, and how it knows it.
    pub facts: FactSet,
    /// The scope this evaluation happens in — `run`, `repo`, `task`.
    pub context: BTreeMap<String, String>,
    /// Now, in milliseconds since the epoch. Used only for expiry.
    pub now_ms: i64,
}

impl Request {
    /// A request with no facts and no context.
    pub fn new(action: Action, now_ms: i64) -> Request {
        Request {
            action,
            facts: FactSet::new(),
            context: BTreeMap::new(),
            now_ms,
        }
    }

    /// Attach the facts.
    pub fn with_facts(mut self, facts: FactSet) -> Request {
        self.facts = facts;
        self
    }

    /// Add one context entry.
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Request {
        self.context.insert(key.into(), value.into());
        self
    }
}

/// A rule that applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatchedRule {
    /// `rule.id`.
    pub rule_id: String,
    /// Which document it came from.
    pub origin: Origin,
    /// The pattern as written.
    pub pattern: String,
    /// What the rule says.
    pub declared: Effect,
    /// What it contributed after the deny cap.
    pub contributed: Effect,
    /// Whether it is part of the Stage-1 ceiling.
    pub locked: bool,
}

/// Why something did not apply.
///
/// §4.4: *"every rule considered that did not, **with the reason**. Negative
/// results are what people debug."* Every variant names the specific value that
/// failed, because "did not match" without the value is a message that sends the
/// reader back to the file to guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoMatch {
    /// The pattern does not cover the action.
    ActionPattern {
        /// The pattern as written.
        pattern: String,
        /// The action asked about.
        action: String,
    },
    /// An exception names a different action. Exceptions match exactly (§4.4).
    DifferentAction {
        /// The action the exception names.
        names: String,
        /// The action asked about.
        action: String,
    },
    /// A scope constraint did not hold.
    Scope {
        /// The constraint's key.
        key: String,
        /// What the scope requires.
        expected: String,
        /// What the context carried.
        actual: Option<String>,
    },
    /// A fact the rule requires is not in the fact set.
    MissingFact {
        /// The absent key.
        key: String,
    },
    /// The exception's TTL has passed.
    Expired {
        /// When it expired.
        expires_at_ms: i64,
        /// The evaluation's clock.
        now_ms: i64,
    },
}

impl std::fmt::Display for NoMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoMatch::ActionPattern { pattern, action } => {
                write!(f, "action pattern {pattern} does not cover {action}")
            }
            NoMatch::DifferentAction { names, action } => write!(
                f,
                "exception names action {names}, not {action} (exceptions match exactly)"
            ),
            NoMatch::Scope {
                key,
                expected,
                actual,
            } => match actual {
                Some(actual) => {
                    write!(f, "scope {key}={expected} but this evaluation has {actual}")
                }
                None => write!(
                    f,
                    "scope {key}={expected} but this evaluation carries no {key}"
                ),
            },
            NoMatch::MissingFact { key } => {
                write!(f, "requires the fact {key}, which was not observed")
            }
            NoMatch::Expired {
                expires_at_ms,
                now_ms,
            } => write!(f, "expired at {expires_at_ms} (now {now_ms})"),
        }
    }
}

/// A rule or exception that was considered and did not apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConsideredRule {
    /// Its id.
    pub rule_id: String,
    /// Which document it came from.
    pub origin: Origin,
    /// The pattern or exact action it names.
    pub pattern: String,
    /// What it would have said.
    pub declared: Effect,
    /// Why it did not apply.
    pub reason: NoMatch,
}

/// A `deny` that was lowered because its supporting facts were not deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DenyCap {
    /// The rule id or built-in invariant id whose deny was capped.
    pub source_id: String,
    /// The fact it rested on.
    pub fact_key: String,
    /// That fact's derivation.
    pub fact_source: FactSource,
    /// What the effect was lowered to.
    pub capped_to: Effect,
}

/// An exception that matched, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppliedException {
    /// Its id.
    pub id: String,
    /// Which document it came from.
    pub origin: Origin,
    /// The action it names.
    pub action: String,
    /// What it asks for.
    pub requested: Effect,
    /// What it actually produced after the floors.
    pub granted: Effect,
    /// Rendered scope.
    pub scope: String,
    /// Its TTL.
    pub expires_at_ms: i64,
}

/// Everything an evaluation concluded, and everything it looked at.
///
/// The decision *is* the explanation. `explain` renders this structure and never
/// recomputes anything — a second evaluation path that could disagree with the
/// first would make the explanation a plausible story rather than a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    /// The action asked about, as parsed.
    #[serde(serialize_with = "serialize_action")]
    pub action: Action,
    /// The resolved effect.
    pub effect: Effect,
    /// Stage 1's ceiling: the maximum permissiveness locked policy allows.
    pub ceiling: Effect,
    /// The locked rules that formed it.
    pub ceiling_rules: Vec<String>,
    /// Rules that applied.
    pub matched: Vec<MatchedRule>,
    /// Rules and exceptions that did not, each with its reason.
    pub not_matched: Vec<ConsideredRule>,
    /// Built-in invariants that applied.
    pub invariants: Vec<AppliedInvariantRecord>,
    /// The exception that applied, if any.
    pub exception: Option<AppliedException>,
    /// The facts the evaluation was given.
    pub facts: Vec<Fact>,
    /// Denies lowered by §4.4's deterministic-facts rule.
    pub caps: Vec<DenyCap>,
    /// Whether the action is outside the taxonomy.
    pub unknown_action: bool,
    /// `blake3:<hex>` over the canonical policy this decision was made against.
    pub policy_hash: String,
}

/// A built-in invariant that fired, as recorded in a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppliedInvariantRecord {
    /// Which invariant.
    pub invariant: BuiltinInvariant,
    /// Its id, for humans.
    pub id: &'static str,
    /// What it contributed after the deny cap.
    pub contributed: Effect,
    /// The fact keys it rests on.
    pub supporting: Vec<String>,
}

fn serialize_action<S: serde::Serializer>(action: &Action, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(action.as_str())
}

/// Evaluate one action against one policy — §4.4's two stages.
pub fn evaluate(policy: &ResolvedPolicy, request: &Request) -> Decision {
    let mut matched = Vec::new();
    let mut not_matched = Vec::new();
    let mut caps = Vec::new();

    // ---- Stage 1: the ceiling ------------------------------------------
    //
    // Read from the **global document only**. A locked rule sitting in the
    // project or task slot — which `PolicyDocument::new` refuses, but which a
    // corrupted snapshot could still carry — contributes nothing here.
    let mut ceiling = Effect::Allow;
    let mut ceiling_rules = Vec::new();
    for rule in policy.global().rules().iter().filter(|r| r.locked) {
        match consider(rule, request) {
            Ok(()) => {
                let contributed = cap_deny(rule.effect, &rule.id, &rule.when, request, &mut caps);
                ceiling = ceiling.join(contributed);
                ceiling_rules.push(rule.id.clone());
                matched.push(MatchedRule {
                    rule_id: rule.id.clone(),
                    origin: rule.origin,
                    pattern: rule.pattern.as_str().to_string(),
                    declared: rule.effect,
                    contributed,
                    locked: true,
                });
            }
            Err(reason) => not_matched.push(considered(rule, reason)),
        }
    }

    // ---- Stage 2: the join ---------------------------------------------
    let mut invariants = Vec::new();
    let mut joined = request.action.floor().join(ceiling);

    for invariant in BuiltinInvariant::ALL {
        let Some(applied) = invariant.applies(&request.action, &request.facts) else {
            continue;
        };
        let AppliedInvariant {
            effect, supporting, ..
        } = applied;
        let contributed = cap_deny(effect, invariant.id(), &supporting, request, &mut caps);
        joined = joined.join(contributed);
        invariants.push(AppliedInvariantRecord {
            invariant: *invariant,
            id: invariant.id(),
            contributed,
            supporting,
        });
    }
    let invariant_floor = invariants
        .iter()
        .fold(Effect::Allow, |acc, i| acc.join(i.contributed));

    for document in policy.documents() {
        for rule in document.rules() {
            // The locked global rules were consumed by Stage 1; consuming them
            // again here would double-count them in `matched`.
            if rule.locked && document.origin() == Origin::Global {
                continue;
            }
            match consider(rule, request) {
                Ok(()) => {
                    let contributed =
                        cap_deny(rule.effect, &rule.id, &rule.when, request, &mut caps);
                    joined = joined.join(contributed);
                    matched.push(MatchedRule {
                        rule_id: rule.id.clone(),
                        origin: rule.origin,
                        pattern: rule.pattern.as_str().to_string(),
                        declared: rule.effect,
                        contributed,
                        locked: rule.locked,
                    });
                }
                Err(reason) => not_matched.push(considered(rule, reason)),
            }
        }
    }

    // ---- Stage 2, second half: the exception ---------------------------
    //
    // The exception is the only construct that lowers an effect. Three floors
    // bound it — the ceiling, the built-in invariants, and the unknown-action
    // floor — and it can never raise an effect, which is what the `meet` is for.
    let mut effect = joined;
    let mut applied_exception = None;
    for document in policy.documents() {
        for exception in document.exceptions() {
            match consider_exception(exception, request) {
                Err(reason) => not_matched.push(considered_exception(exception, reason)),
                Ok(()) => {
                    if applied_exception.is_some() {
                        // Two exceptions matching one evaluation: the first in
                        // global-then-project-then-task order wins, and the rest
                        // are reported rather than silently dropped. They cannot
                        // loosen further than the first anyway, because each is
                        // clamped by the same floors.
                        not_matched.push(considered_exception(
                            exception,
                            NoMatch::DifferentAction {
                                names: exception.action.as_str().to_string(),
                                action: format!(
                                    "{} (an earlier exception already applied)",
                                    request.action
                                ),
                            },
                        ));
                        continue;
                    }
                    let floor = exception
                        .effect
                        .join(ceiling)
                        .join(invariant_floor)
                        .join(request.action.floor());
                    let granted = joined.meet(floor);
                    effect = granted;
                    applied_exception = Some(AppliedException {
                        id: exception.id.clone(),
                        origin: exception.origin,
                        action: exception.action.as_str().to_string(),
                        requested: exception.effect,
                        granted,
                        scope: exception.scope.to_string(),
                        expires_at_ms: exception.expires_at_ms,
                    });
                }
            }
        }
    }

    Decision {
        action: request.action.clone(),
        effect,
        ceiling,
        ceiling_rules,
        matched,
        not_matched,
        invariants,
        exception: applied_exception,
        facts: request.facts.iter().cloned().collect(),
        caps,
        unknown_action: !request.action.is_known(),
        policy_hash: super::load::snapshot(policy).hash,
    }
}

/// §4.4's deny rule, applied to one contribution.
///
/// Returns the effect the contribution is allowed to carry, recording a
/// [`DenyCap`] when it had to be lowered. Nothing but a `deny` is ever capped:
/// a `require_approval` may rest on any source.
fn cap_deny(
    effect: Effect,
    source_id: &str,
    supporting: &[String],
    request: &Request,
    caps: &mut Vec<DenyCap>,
) -> Effect {
    if effect != Effect::Deny {
        return effect;
    }
    for key in supporting {
        let Some(fact) = request.facts.get(key) else {
            continue;
        };
        if !fact.source.may_carry_a_deny() {
            caps.push(DenyCap {
                source_id: source_id.to_string(),
                fact_key: key.clone(),
                fact_source: fact.source,
                capped_to: Effect::RequireApproval,
            });
            return Effect::RequireApproval;
        }
    }
    Effect::Deny
}

/// Whether a rule applies, or the first reason it does not.
fn consider(rule: &Rule, request: &Request) -> Result<(), NoMatch> {
    if !rule.pattern.matches(&request.action) {
        return Err(NoMatch::ActionPattern {
            pattern: rule.pattern.as_str().to_string(),
            action: request.action.as_str().to_string(),
        });
    }
    scope_ok(&rule.scope, request)?;
    for key in &rule.when {
        if !request.facts.contains(key) {
            return Err(NoMatch::MissingFact { key: key.clone() });
        }
    }
    Ok(())
}

/// Whether an exception applies, or the first reason it does not.
///
/// §4.4: "matches **exactly**, is unexpired, and the Stage-1 ceiling permits
/// it". The first two are here; the ceiling is applied at the join, because an
/// exception the ceiling clamps still *matched* and must be reported as such.
fn consider_exception(exception: &PolicyException, request: &Request) -> Result<(), NoMatch> {
    if exception.action != request.action {
        return Err(NoMatch::DifferentAction {
            names: exception.action.as_str().to_string(),
            action: request.action.as_str().to_string(),
        });
    }
    scope_ok(&exception.scope, request)?;
    if exception.expires_at_ms <= request.now_ms {
        return Err(NoMatch::Expired {
            expires_at_ms: exception.expires_at_ms,
            now_ms: request.now_ms,
        });
    }
    Ok(())
}

fn scope_ok(scope: &Scope, request: &Request) -> Result<(), NoMatch> {
    match scope.matches(&request.context) {
        ScopeMatch::Matched => Ok(()),
        ScopeMatch::Mismatch {
            key,
            expected,
            actual,
        } => Err(NoMatch::Scope {
            key,
            expected,
            actual,
        }),
    }
}

fn considered(rule: &Rule, reason: NoMatch) -> ConsideredRule {
    ConsideredRule {
        rule_id: rule.id.clone(),
        origin: rule.origin,
        pattern: rule.pattern.as_str().to_string(),
        declared: rule.effect,
        reason,
    }
}

fn considered_exception(exception: &PolicyException, reason: NoMatch) -> ConsideredRule {
    ConsideredRule {
        rule_id: exception.id.clone(),
        origin: exception.origin,
        pattern: exception.action.as_str().to_string(),
        declared: exception.effect,
        reason,
    }
}

// ---------------------------------------------------------------------------
// run-lifetime pinning (§4.4, acceptance row 23)
// ---------------------------------------------------------------------------

/// One action whose effect the current policy tightened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tightened {
    /// The pending action.
    #[serde(serialize_with = "serialize_action")]
    pub action: Action,
    /// What the run's pinned snapshot says.
    pub pinned: Effect,
    /// What the policy on disk says now.
    pub current: Effect,
}

/// What to do about a policy that changed under a running run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum DriftDecision {
    /// Carry on under the pinned snapshot.
    Proceed,
    /// §4.4's exception: *"if the new policy is strictly more restrictive for a
    /// pending action, the run pauses and asks. Never silently proceeds under a
    /// policy the human just tightened."*
    Pause {
        /// Every pending action the new policy tightened.
        tightened: Vec<Tightened>,
    },
}

/// Compare a run's pinned policy against the one on disk, for the actions the
/// run still has to perform.
///
/// **Only tightening pauses.** A loosened policy does not pause and does not
/// take effect: the run keeps evaluating against its snapshot either way, which
/// is what "a run evaluates against its snapshot for its entire life" means. The
/// pause exists because proceeding under a rule a human has just tightened is
/// indistinguishable, after the fact, from ignoring them.
pub fn drift(
    pinned: &super::load::Pinned,
    current: &ResolvedPolicy,
    pending: &[Action],
    context: &BTreeMap<String, String>,
    now_ms: i64,
) -> DriftDecision {
    let mut tightened = Vec::new();
    for action in pending {
        let request = Request {
            action: action.clone(),
            facts: FactSet::new(),
            context: context.clone(),
            now_ms,
        };
        let before = evaluate(&pinned.policy, &request).effect;
        let after = evaluate(current, &request).effect;
        if after > before {
            tightened.push(Tightened {
                action: action.clone(),
                pinned: before,
                current: after,
            });
        }
    }
    if tightened.is_empty() {
        DriftDecision::Proceed
    } else {
        DriftDecision::Pause { tightened }
    }
}
