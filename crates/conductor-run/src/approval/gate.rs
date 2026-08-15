//! §4.3's binding rule, expressed as an §4.2 requirement.
//!
//! > **Binding rule:** a task whose policy can produce an approval gate **may
//! > not run unattended** below tier A. Enforced by §4.2's eligibility check,
//! > not by documentation.
//!
//! The last clause is the design. A rule enforced by prose is a rule that holds
//! until somebody is in a hurry, so this module turns "can produce an approval
//! gate" into an entry in [`ExecutionRequirements`] — `control_surface: hard` —
//! and hands it to `eligibility::check`, which already refuses, already names
//! the dimension, and already fails closed on a stale probe. No second refusal
//! path is written here, because a second path is a path that can drift from
//! the first.
//!
//! # Why `hard` and not "sandboxed"
//!
//! §4.3's tier A *is* `control_surface: Hard` — the seatbelt AF_UNIX
//! default-deny, measured with a positive control (M10, M11). §4.2's
//! fail-closed vector reports `None` for an unmeasured host, so "below tier A"
//! and "requires `control_surface: hard`" are the same statement, and the
//! second one is the one the existing gate can act on.
//!
//! # Where this is, and is not, wired
//!
//! [`unattended_requirements`] is a pure function, like `eligibility::check`
//! itself. **It is not called from the attempt-launch path**, for the reason
//! the master plan's note on acceptance row 30 gives: §4.2 says "before
//! launching an attempt", that is enforcement, and S9 owns enforcement. Until
//! S9 wires it, the binding rule is *decided* and not *reachable from a real
//! launch* — recorded here so the difference cannot be mistaken for coverage.

use crate::policy::eligibility::ExecutionRequirements;
use crate::policy::model::{Action, Effect, ResolvedPolicy};
use conductor_core::containment::{Enforcement, GatingDimension};
use serde::Serialize;

/// Whether an action can produce an approval gate under a policy, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "gate", rename_all = "snake_case")]
pub enum GatePossibility {
    /// Some rule, invariant or floor can resolve this action to
    /// `require_approval`.
    Possible {
        /// What can produce it. Named, because §4.4's doctrine is that a
        /// negative result without its reason sends the reader back to the file
        /// to guess — and a positive one that cannot say why is worse.
        reason: String,
    },
    /// Nothing in this policy can gate this action.
    Impossible,
}

impl GatePossibility {
    /// Whether a gate is possible.
    pub fn is_possible(&self) -> bool {
        matches!(self, GatePossibility::Possible { .. })
    }
}

/// Can this policy produce an approval gate for this action?
///
/// Two sources, and the second is the one that is easy to miss:
///
/// 1. **A rule declaring `require_approval`** that could match the action.
/// 2. **A rule declaring `deny` whose `when:` names facts.** ADR-0010 caps such
///    a rule at `require_approval` when a named fact turns out to be
///    `model_assisted`, so it *can* gate — and whether it does depends on facts
///    that do not exist yet at launch time. Counted, because the alternative is
///    launching unattended and discovering the gate with nobody there.
///
/// A rule declaring `deny` with **no** `when:` is a standing policy statement
/// resting on no facts, cannot be capped (ADR-0010), and therefore cannot gate.
///
/// # Why the built-in invariants are not a third source
///
/// §4.4 applies the deny cap to invariants "with no exemption", so a
/// fact-conditioned invariant would gate if the fact it rests on were
/// `model_assisted`. Both of them rest on facts that S7's extractors emit as
/// **deterministic** — `facts::key::SECRET_MATCH` and
/// `facts::key::REPOSITORY_REGISTERED` — so the cap cannot fire on either and
/// neither can produce a gate.
///
/// **Revisit trigger:** any extractor that emits one of those two keys with a
/// non-deterministic source. That would make every action gateable, which is a
/// defensible answer but a very different one, and it must be a decision rather
/// than a discovery. [`the_invariants_rest_only_on_deterministic_facts`] is the
/// test that fails if it ever changes.
///
/// [`the_invariants_rest_only_on_deterministic_facts`]: #
pub fn gate_possible(policy: &ResolvedPolicy, action: &Action) -> GatePossibility {
    for document in policy.documents() {
        for rule in document.rules() {
            if !rule.pattern.matches(action) {
                continue;
            }
            match rule.effect {
                Effect::RequireApproval => {
                    return GatePossibility::Possible {
                        reason: format!(
                            "{} declares require_approval for {}",
                            rule.id,
                            rule.pattern.as_str()
                        ),
                    };
                }
                Effect::Deny if !rule.when.is_empty() => {
                    return GatePossibility::Possible {
                        reason: format!(
                            "{} denies on facts {:?}; ADR-0010 caps it at \
                             require_approval if any of them is model-assisted",
                            rule.id, rule.when
                        ),
                    };
                }
                _ => {}
            }
        }
    }
    GatePossibility::Impossible
}

/// §4.3's binding rule as an §4.2 requirement vector.
///
/// Returns `control_surface: hard` when **any** of the actions a task may
/// perform can produce a gate, and an empty vector otherwise. Empty means
/// §4.2's `check` compares nothing and proceeds, which is the right answer: a
/// task that cannot be gated has no approval integrity to protect.
///
/// Feed it to `eligibility::check` alongside the task's own
/// `execution_requirements`; both are `ExecutionRequirements`, and
/// [`ExecutionRequirements::require`] takes the stronger of two values when
/// merged by [`merge`].
pub fn unattended_requirements(
    policy: &ResolvedPolicy,
    actions: &[Action],
) -> ExecutionRequirements {
    let mut requirements = ExecutionRequirements::new();
    if actions
        .iter()
        .any(|action| gate_possible(policy, action).is_possible())
    {
        requirements.require(GatingDimension::ControlSurface, Enforcement::Hard);
    }
    requirements
}

/// Combine two requirement vectors, taking the **stronger** demand per
/// dimension.
///
/// Stronger, not "the second one wins": a task that asks for
/// `control_surface: audit_only` must not be able to talk §4.3's binding rule
/// down from `hard`, which is precisely the loosening §4.4's ceiling exists to
/// prevent one level up.
pub fn merge(left: &ExecutionRequirements, right: &ExecutionRequirements) -> ExecutionRequirements {
    let mut merged = ExecutionRequirements::new();
    for dimension in GatingDimension::ALL {
        let demanded = [left.get(*dimension), right.get(*dimension)]
            .into_iter()
            .flatten()
            .max();
        if let Some(enforcement) = demanded {
            merged.require(*dimension, enforcement);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::load;
    use crate::policy::model::Origin;

    fn policy(yaml: &str) -> ResolvedPolicy {
        let document = load::parse_document(yaml, Origin::Global).expect("parse");
        load::resolve_documents(Some(document), None, None).expect("resolve")
    }

    #[test]
    fn merging_takes_the_stronger_demand() {
        let mut weak = ExecutionRequirements::new();
        weak.require(GatingDimension::ControlSurface, Enforcement::AuditOnly);
        let mut strong = ExecutionRequirements::new();
        strong.require(GatingDimension::ControlSurface, Enforcement::Hard);
        assert_eq!(
            merge(&weak, &strong).get(GatingDimension::ControlSurface),
            Some(Enforcement::Hard)
        );
        assert_eq!(
            merge(&strong, &weak).get(GatingDimension::ControlSurface),
            Some(Enforcement::Hard)
        );
    }

    #[test]
    fn a_standing_deny_with_no_when_cannot_gate() {
        // ADR-0010: "a rule with **no** `when:` is a standing policy statement
        // resting on no facts at all, and still denies". A deny is not
        // approvable, so it produces no gate.
        let policy =
            policy("policy:\n  rules:\n    - {id: g.push, action: git.push, effect: deny}\n");
        assert_eq!(
            gate_possible(&policy, &Action::parse("git.push")),
            GatePossibility::Impossible
        );
    }
}
