//! `binding_hash` — §4.3's scoping mechanism.
//!
//! > `binding_hash: blake3(action ‖ canonical(facts) ‖ policy_hash ‖ scope)`
//! >
//! > **`binding_hash` is the scoping mechanism.** A grant authorizes an
//! > operation only if the recomputed hash matches at use time. So
//! > `dependency.add.runtime:foo` cannot authorize `…:bar` (different facts),
//! > cannot authorize `deployment.execute` (different action), and stops
//! > applying if the policy snapshot changed — which is correct, not
//! > inconvenient.
//!
//! # Two departures from the formula as written, both deliberate
//!
//! **1. The approval kind is a fifth component.** §4.3's formula names four
//! inputs, and §4.3 also requires that the four kinds "never collapse", warning
//! that collapsing them "would let a plan approval satisfy a deployment gate".
//! With only the four named inputs, that property would rest on the *naming
//! convention* that no plan version id ever equals a typed action name. A
//! domain separator makes it rest on the mechanism instead. Reported as a
//! master-plan delta rather than taken silently.
//!
//! **2. Every component is length-prefixed.** §4.3 writes `‖` and does not say
//! what it is. Plain concatenation makes `("ab", "c")` and `("a", "bc")` the
//! same preimage, which in a value whose entire job is to distinguish one
//! authorization from another is not a theoretical objection. This is the same
//! argument `OperationId::compute` records for its separator byte, taken one
//! step further because these components are operator-supplied text.
//!
//! # `canonical(facts)` carries each fact's source
//!
//! ADR-0010's third revisit trigger is "S8 granting approvals in a way that
//! lets a capped deny be satisfied more cheaply than an uncapped one". A rule
//! whose `when:` rests on a `model_assisted` fact is capped from `deny` to
//! `require_approval` and is therefore *grantable*; the same rule resting on a
//! `deterministic` fact denies and is not. Putting `source` in the preimage
//! means a grant obtained while the evidence was model-assisted binds to a
//! different operation than the deterministic one, so the cheap path cannot be
//! replayed against the expensive one. (The other half of that guarantee is in
//! [`super::authorize`]: a `deny` is not approvable at all.)

use std::fmt;

use serde::{Deserialize, Serialize};

use super::kind::{ApprovalKind, Subject};
use crate::policy::evaluate::Decision;
use crate::policy::model::{FactSet, Scope};

/// Domain separator. Present in every preimage so that a digest computed by a
/// different Conductor — or by a different hashing site inside this one —
/// cannot be mistaken for a binding.
pub const BINDING_DOMAIN: &str = "conductor.approval.binding.v1";

/// `approval_grant.binding_hash`, rendered `blake3:<hex>` (ADR-0007).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BindingHash(String);

impl BindingHash {
    /// The stored text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Rebuild from what the database holds.
    ///
    /// Deliberately named for where the value comes from. Nothing in this crate
    /// *decides* on a stored binding: [`super::authorize`] recomputes and
    /// compares, so a row whose digest disagrees with its own inputs authorizes
    /// nothing. Same doctrine as `repair_observation.fingerprint`.
    pub fn from_stored(stored: impl Into<String>) -> BindingHash {
        BindingHash(stored.into())
    }
}

impl fmt::Display for BindingHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything a `binding_hash` is computed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// What is authorized. Contributes both the kind and §4.3's `action`.
    pub subject: Subject,
    /// The facts the request rested on, with their sources.
    pub facts: FactSet,
    /// The **snapshot** the decision was made against (§4.4: "a run evaluates
    /// against its snapshot for its entire life").
    pub policy_hash: String,
    /// §4.3's `scope`, e.g. `{run: r-0041}`.
    pub scope: Scope,
}

impl Binding {
    /// The binding a decision produces at this scope.
    ///
    /// This is the constructor the runtime uses at **use time**, which is what
    /// makes §4.3's "recomputed hash" true: the action, the facts and the
    /// policy hash all come from the decision that is about to be acted on, not
    /// from anything stored alongside the grant.
    pub fn for_decision(decision: &Decision, scope: &Scope) -> Binding {
        Binding {
            subject: Subject::PolicyAction {
                action: decision.action.clone(),
            },
            facts: decision.facts.iter().cloned().collect(),
            policy_hash: decision.policy_hash.clone(),
            scope: scope.clone(),
        }
    }

    /// The kind this binding is for.
    pub fn kind(&self) -> ApprovalKind {
        self.subject.kind()
    }

    /// `blake3(domain ‖ kind ‖ subject ‖ canonical(facts) ‖ policy_hash ‖ scope)`.
    pub fn hash(&self) -> BindingHash {
        let mut hasher = blake3::Hasher::new();
        absorb(&mut hasher, BINDING_DOMAIN);
        absorb(&mut hasher, self.subject.kind().as_str());
        // The component count is absorbed too, so a subject with two components
        // cannot be confused with one whose single component happens to encode
        // both.
        absorb(
            &mut hasher,
            &self.subject.binding_components().len().to_string(),
        );
        for component in self.subject.binding_components() {
            absorb(&mut hasher, &component);
        }
        absorb(&mut hasher, &canonical_facts(&self.facts));
        absorb(&mut hasher, &self.policy_hash);
        absorb(&mut hasher, &canonical_scope(&self.scope));
        BindingHash(format!("blake3:{}", hasher.finalize().to_hex()))
    }
}

/// Length-prefixed absorption: `<byte length> 0x1f <bytes>`.
fn absorb(hasher: &mut blake3::Hasher, component: &str) {
    hasher.update(component.len().to_string().as_bytes());
    hasher.update(&[0x1f]);
    hasher.update(component.as_bytes());
}

/// §4.3's `canonical(facts)`.
///
/// Sorted by key then value then source, so two evaluations that observed the
/// same facts in a different order produce the same binding — the same
/// discipline §4.4's policy snapshot uses ("sorted keys, no timestamps"), for
/// the same reason: a grant that depended on iteration order would be a grant
/// that sometimes did not apply.
///
/// `evidence` is **excluded**. It is a pointer to an artifact — a path and a
/// digest — and two evaluations of identical facts can legitimately write it to
/// two different artifact paths. Binding to it would expire grants for reasons
/// that have nothing to do with what was authorized.
fn canonical_facts(facts: &FactSet) -> String {
    let mut entries: Vec<String> = facts
        .iter()
        .map(|fact| {
            format!(
                "{}\u{1f}{}\u{1f}{}",
                fact.key,
                fact.value,
                fact.source.as_str()
            )
        })
        .collect();
    entries.sort();
    entries.join("\u{1e}")
}

/// §4.3's `scope`, rendered from a `BTreeMap` and therefore already sorted.
fn canonical_scope(scope: &Scope) -> String {
    scope
        .pairs()
        .map(|(key, value)| format!("{key}\u{1f}{value}"))
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::model::{Action, Effect, Fact};

    fn scope() -> Scope {
        Scope::from_pairs([("run".to_string(), "r-0041".to_string())])
    }

    fn policy_binding(facts: FactSet) -> Binding {
        Binding {
            subject: Subject::PolicyAction {
                action: Action::parse("dependency.add.runtime"),
            },
            facts,
            policy_hash: "blake3:41ef".to_string(),
            scope: scope(),
        }
    }

    #[test]
    fn the_hash_is_stable_and_prefixed() {
        let binding = policy_binding(FactSet::new());
        assert_eq!(binding.hash(), binding.hash());
        assert!(binding.hash().as_str().starts_with("blake3:"));
    }

    #[test]
    fn fact_order_does_not_change_the_binding() {
        let forwards: FactSet = [
            Fact::deterministic("dependency", "serde_yaml"),
            Fact::deterministic("manifest", "Cargo.toml"),
        ]
        .into_iter()
        .collect();
        let backwards: FactSet = [
            Fact::deterministic("manifest", "Cargo.toml"),
            Fact::deterministic("dependency", "serde_yaml"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            policy_binding(forwards).hash(),
            policy_binding(backwards).hash()
        );
    }

    #[test]
    fn a_facts_source_is_part_of_the_binding() {
        // ADR-0010's carry-forward, at the level of the digest.
        let model: FactSet = [Fact::model_assisted("proxy", "src/**")]
            .into_iter()
            .collect();
        let deterministic: FactSet = [Fact::deterministic("proxy", "src/**")]
            .into_iter()
            .collect();
        assert_ne!(
            policy_binding(model).hash(),
            policy_binding(deterministic).hash()
        );
    }

    #[test]
    fn evidence_is_not_part_of_the_binding() {
        let bare: FactSet = [Fact::deterministic("dependency", "serde")]
            .into_iter()
            .collect();
        let with_evidence: FactSet =
            [Fact::deterministic("dependency", "serde").with_evidence("artifacts/x.patch")]
                .into_iter()
                .collect();
        assert_eq!(
            policy_binding(bare).hash(),
            policy_binding(with_evidence).hash()
        );
    }

    #[test]
    fn the_components_cannot_be_slid_past_one_another() {
        // Without a length prefix these two would share a preimage.
        let left = Binding {
            subject: Subject::PolicyAction {
                action: Action::parse("git.push"),
            },
            facts: FactSet::new(),
            policy_hash: "blake3:ab".to_string(),
            scope: Scope::everywhere(),
        };
        let right = Binding {
            subject: Subject::PolicyAction {
                action: Action::parse("git.pushblake3:a"),
            },
            facts: FactSet::new(),
            policy_hash: "b".to_string(),
            scope: Scope::everywhere(),
        };
        assert_ne!(left.hash(), right.hash());
    }

    #[test]
    fn each_kind_binds_into_its_own_space() {
        let subjects = [
            Subject::PlanVersion {
                plan_version_id: "x".to_string(),
            },
            Subject::PolicyAction {
                action: Action::parse("x"),
            },
            Subject::Rule {
                rule_id: "x".to_string(),
                requested: Effect::Allow,
            },
            Subject::ReviewPacket {
                packet_id: "x".to_string(),
            },
        ];
        let hashes: Vec<BindingHash> = subjects
            .into_iter()
            .map(|subject| {
                Binding {
                    subject,
                    facts: FactSet::new(),
                    policy_hash: "blake3:p".to_string(),
                    scope: scope(),
                }
                .hash()
            })
            .collect();
        for (index, left) in hashes.iter().enumerate() {
            for right in &hashes[index + 1..] {
                assert_ne!(left, right, "kinds must not share a binding space");
            }
        }
    }
}
