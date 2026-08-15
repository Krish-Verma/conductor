//! §4.3's **four** approval kinds, and the subjects they authorize.
//!
//! > | Kind | Authorizes | Granularity | Expires |
//! > |---|---|---|---|
//! > | Plan approval | a plan version becoming authoritative | one plan version | no |
//! > | Policy approval | one policy-gated action, once | one `binding_hash` | yes |
//! > | Policy exception | temporarily loosening a rule | rule + scope, within the ceiling | yes (mandatory) |
//! > | Review acceptance | that completed work is accepted | one review packet | no |
//! >
//! > Collapsing them would let a plan approval satisfy a deployment gate.
//!
//! # How the collapse is prevented, mechanically
//!
//! Three things, none of which is a convention:
//!
//! 1. **The kind is derived from the subject, never stored beside it.**
//!    [`Subject::kind`] is a total function, so there is no constructor that
//!    accepts a kind and a subject separately and therefore no way for the two
//!    to disagree. A `bool` cannot be substituted for a [`Subject`].
//! 2. **The kind is a domain separator in the binding preimage**
//!    ([`super::binding`]). Two approvals over otherwise identical material
//!    hash differently, so the anti-collapse property survives even if a caller
//!    only ever compares hashes.
//! 3. **Expiry is a property of the kind**, not a field a caller chooses.
//!    [`ApprovalKind::expiry_rule`] is checked at write time, so a plan
//!    approval cannot be given a TTL that would silently lapse and a policy
//!    approval cannot be made perpetual.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::policy::model::{Action, Effect};

/// Which of §4.3's four kinds an approval is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApprovalKind {
    /// A plan version becoming authoritative. One plan version. Does not expire.
    Plan,
    /// One policy-gated action, once. One `binding_hash`. Expires.
    Policy,
    /// Temporarily loosening a rule, within the Stage-1 ceiling. Expires,
    /// mandatorily.
    PolicyException,
    /// That completed work is accepted. One review packet. Does not expire.
    ReviewAcceptance,
}

impl ApprovalKind {
    /// The four kinds, in §4.3's table order.
    pub const ALL: &'static [ApprovalKind] = &[
        ApprovalKind::Plan,
        ApprovalKind::Policy,
        ApprovalKind::PolicyException,
        ApprovalKind::ReviewAcceptance,
    ];

    /// The exact string persisted in `approval_request.kind` (schema v6).
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalKind::Plan => "PLAN_APPROVAL",
            ApprovalKind::Policy => "POLICY_APPROVAL",
            ApprovalKind::PolicyException => "POLICY_EXCEPTION",
            ApprovalKind::ReviewAcceptance => "REVIEW_ACCEPTANCE",
        }
    }

    /// Parse a stored kind. `None` for anything else — an unrecognised kind is
    /// not a default kind, for the same reason §4.4's unknown action is not a
    /// default action.
    pub fn parse(text: &str) -> Option<ApprovalKind> {
        ApprovalKind::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == text)
    }

    /// §4.3's "Authorizes" column.
    pub fn authorizes(&self) -> &'static str {
        match self {
            ApprovalKind::Plan => "a plan version becoming authoritative",
            ApprovalKind::Policy => "one policy-gated action, once",
            ApprovalKind::PolicyException => "temporarily loosening a rule",
            ApprovalKind::ReviewAcceptance => "that completed work is accepted",
        }
    }

    /// §4.3's "Granularity" column.
    pub fn granularity(&self) -> &'static str {
        match self {
            ApprovalKind::Plan => "one plan version",
            ApprovalKind::Policy => "one binding_hash",
            ApprovalKind::PolicyException => "rule + scope, within the ceiling",
            ApprovalKind::ReviewAcceptance => "one review packet",
        }
    }

    /// §4.3's "Expires" column, as a rule the writer is held to.
    pub fn expiry_rule(&self) -> ExpiryRule {
        match self {
            // "no" — and a plan approval that lapsed would take an authoritative
            // plan out from under a running task with no human involved.
            ApprovalKind::Plan | ApprovalKind::ReviewAcceptance => ExpiryRule::Forbidden,
            // "yes", and "yes (mandatory)". The distinction §4.3 draws is
            // emphasis, not semantics: both must carry one.
            ApprovalKind::Policy | ApprovalKind::PolicyException => ExpiryRule::Mandatory,
        }
    }
}

impl fmt::Display for ApprovalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What §4.3's fourth column requires of a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryRule {
    /// The kind does not expire, and giving it a TTL is refused.
    Forbidden,
    /// The kind expires, and omitting the TTL is refused.
    Mandatory,
}

/// A TTL, or its deliberate absence.
///
/// Not `Option<i64>` with a comment: "does not expire" is a decision §4.3 makes
/// per kind, and a type that says so cannot be confused with a missing value or
/// a sentinel far in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "expiry", rename_all = "snake_case")]
pub enum Expiry {
    /// §4.3: a plan approval and a review acceptance do not expire.
    Never,
    /// Milliseconds since the epoch, UTC (§4.4: RFC 3339, UTC only).
    At(
        /// The instant the approval stops applying.
        i64,
    ),
}

impl Expiry {
    /// Whether this expiry has passed. `Never` never has.
    ///
    /// The comparison is `expires_at <= now`, matching §4.4's exception check,
    /// so an approval is not usable in the millisecond it expires.
    pub fn is_expired(&self, now_ms: i64) -> bool {
        match self {
            Expiry::Never => false,
            Expiry::At(at) => *at <= now_ms,
        }
    }

    /// The stored column value: `NULL` for `Never`.
    pub fn as_millis(&self) -> Option<i64> {
        match self {
            Expiry::Never => None,
            Expiry::At(at) => Some(*at),
        }
    }

    /// Rebuild from the stored column.
    pub fn from_stored(stored: Option<i64>) -> Expiry {
        match stored {
            None => Expiry::Never,
            Some(at) => Expiry::At(at),
        }
    }

    /// Whether this expiry satisfies a kind's rule.
    pub fn satisfies(&self, rule: ExpiryRule) -> bool {
        matches!(
            (rule, self),
            (ExpiryRule::Forbidden, Expiry::Never) | (ExpiryRule::Mandatory, Expiry::At(_))
        )
    }
}

/// What an approval authorizes.
///
/// The variants are §4.3's four "Granularity" cells. The kind is read off the
/// variant, so a request cannot claim to be one kind while describing another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// One plan version becoming authoritative (§5.2's plan machine).
    PlanVersion {
        /// `plan_version.id`.
        plan_version_id: String,
    },
    /// One policy-gated action. The rest of the binding — facts, policy hash,
    /// scope — travels with it in [`super::binding::Binding`].
    PolicyAction {
        /// The action §4.4 resolved to `require_approval`.
        action: Action,
    },
    /// Temporarily loosening one rule. §4.4 clamps the result at the Stage-1
    /// ceiling, so `requested` is what was asked for, not what will be granted.
    Rule {
        /// `rule.id` being loosened.
        rule_id: String,
        /// The effect the exception asks for.
        requested: Effect,
    },
    /// One review packet being accepted (§6.5).
    ReviewPacket {
        /// The packet's identifier.
        packet_id: String,
    },
}

impl Subject {
    /// Which kind this subject *is*. Total, and the only way to obtain a kind.
    pub fn kind(&self) -> ApprovalKind {
        match self {
            Subject::PlanVersion { .. } => ApprovalKind::Plan,
            Subject::PolicyAction { .. } => ApprovalKind::Policy,
            Subject::Rule { .. } => ApprovalKind::PolicyException,
            Subject::ReviewPacket { .. } => ApprovalKind::ReviewAcceptance,
        }
    }

    /// The components this subject contributes to the binding preimage, each
    /// hashed with its own length prefix by [`super::binding::Binding::hash`].
    pub fn binding_components(&self) -> Vec<String> {
        match self {
            Subject::PlanVersion { plan_version_id } => vec![plan_version_id.clone()],
            Subject::PolicyAction { action } => vec![action.as_str().to_string()],
            Subject::Rule {
                rule_id,
                requested: effect,
            } => vec![rule_id.clone(), effect.as_str().to_string()],
            Subject::ReviewPacket { packet_id } => vec![packet_id.clone()],
        }
    }

    /// What goes in `approval_request.action`.
    ///
    /// A policy approval's action is §4.4's typed action. The other three kinds
    /// have no typed action, so they carry the verb that names what a human is
    /// being asked to authorize. These are **not** §4.4 actions and are never
    /// parsed as one — `Action::parse` would turn them into `Action::Unknown`,
    /// whose floor is `deny`, which is the right answer for a policy question
    /// and a meaningless one here.
    pub fn action_column(&self) -> String {
        match self {
            Subject::PlanVersion { .. } => "plan.approve".to_string(),
            Subject::PolicyAction { action } => action.as_str().to_string(),
            Subject::Rule { .. } => "policy.exception".to_string(),
            Subject::ReviewPacket { .. } => "review.accept".to_string(),
        }
    }

    /// What goes in `approval_request.subject` — `None` for a policy approval,
    /// whose subject is already `action` plus `facts`.
    pub fn subject_column(&self) -> Option<String> {
        match self {
            Subject::PlanVersion { plan_version_id } => Some(plan_version_id.clone()),
            Subject::PolicyAction { .. } => None,
            Subject::Rule {
                rule_id,
                requested: effect,
            } => Some(format!("{rule_id}={}", effect.as_str())),
            Subject::ReviewPacket { packet_id } => Some(packet_id.clone()),
        }
    }

    /// Rebuild a subject from the two stored columns.
    ///
    /// Returns `None` when the row cannot be interpreted as the kind it claims
    /// — a hand-edited or truncated row authorizes nothing rather than
    /// defaulting to the most convenient reading.
    pub fn from_stored(kind: ApprovalKind, action: &str, subject: Option<&str>) -> Option<Subject> {
        match kind {
            ApprovalKind::Plan => Some(Subject::PlanVersion {
                plan_version_id: subject?.to_string(),
            }),
            ApprovalKind::Policy => Some(Subject::PolicyAction {
                action: Action::parse(action),
            }),
            ApprovalKind::PolicyException => {
                let (rule_id, effect) = subject?.rsplit_once('=')?;
                Some(Subject::Rule {
                    rule_id: rule_id.to_string(),
                    requested: Effect::parse(effect)?,
                })
            }
            ApprovalKind::ReviewAcceptance => Some(Subject::ReviewPacket {
                packet_id: subject?.to_string(),
            }),
        }
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Subject::PlanVersion { plan_version_id } => write!(f, "plan version {plan_version_id}"),
            Subject::PolicyAction { action } => write!(f, "action {action}"),
            Subject::Rule { rule_id, requested } => write!(f, "rule {rule_id} → {requested}"),
            Subject::ReviewPacket { packet_id } => write!(f, "review packet {packet_id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_its_stored_string() {
        for kind in ApprovalKind::ALL {
            assert_eq!(ApprovalKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(ApprovalKind::parse("APPROVED"), None);
        assert_eq!(ApprovalKind::parse(""), None);
    }

    #[test]
    fn every_subject_round_trips_through_its_two_columns() {
        let subjects = [
            Subject::PlanVersion {
                plan_version_id: "pv-4".to_string(),
            },
            Subject::PolicyAction {
                action: Action::parse("deployment.execute"),
            },
            Subject::Rule {
                rule_id: "project.deps".to_string(),
                requested: Effect::Allow,
            },
            Subject::ReviewPacket {
                packet_id: "RP-9".to_string(),
            },
        ];
        for subject in subjects {
            let restored = Subject::from_stored(
                subject.kind(),
                &subject.action_column(),
                subject.subject_column().as_deref(),
            );
            assert_eq!(restored.as_ref(), Some(&subject));
        }
    }

    #[test]
    fn a_rule_id_containing_an_equals_sign_still_round_trips() {
        // `rsplit_once`, not `split_once`: the effect is the last field, and a
        // rule id is operator-supplied text.
        let subject = Subject::Rule {
            rule_id: "project.dep=weird".to_string(),
            requested: Effect::RequireApproval,
        };
        assert_eq!(
            Subject::from_stored(
                ApprovalKind::PolicyException,
                &subject.action_column(),
                subject.subject_column().as_deref()
            ),
            Some(subject)
        );
    }

    #[test]
    fn a_row_missing_its_subject_authorizes_nothing() {
        assert_eq!(
            Subject::from_stored(ApprovalKind::Plan, "plan.approve", None),
            None
        );
        assert_eq!(
            Subject::from_stored(ApprovalKind::PolicyException, "policy.exception", Some("x")),
            None
        );
    }

    #[test]
    fn expiry_never_is_never_expired_and_a_ttl_is_expired_at_its_own_instant() {
        assert!(!Expiry::Never.is_expired(i64::MAX));
        assert!(Expiry::At(1_000).is_expired(1_000));
        assert!(!Expiry::At(1_000).is_expired(999));
        assert_eq!(Expiry::Never.as_millis(), None);
        assert_eq!(Expiry::from_stored(None), Expiry::Never);
        assert_eq!(Expiry::from_stored(Some(7)), Expiry::At(7));
    }
}
