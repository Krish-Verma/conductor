//! §4.5's seven completion criteria, and the only gate that can name
//! `COMPLETE`.
//!
//! > **Completion criteria — a task may reach `COMPLETE` only when all hold:**
//! > 1. Every required check `PASS` **at the current tree hash**.
//! > 2. Every conditional check triggered by the actual diff has run and passed.
//! > 3. All invariant checks pass.
//! > 4. Zero unresolved findings.
//! > 5. Every acceptance criterion binds to ≥1 passing check.
//! > 6. Reconciliation verdict ∈ {`CLEAN_COMPLETE`, `CLEAN_NO_REPORT`}.
//! > 7. Every policy-sensitive action detected has a matching, unexpired,
//! >    correctly-scoped grant.
//! >
//! > Note what is absent: **the agent's report.**
//!
//! # Why the variant carries a token
//!
//! S3 shipped [`ReconciledRoute`](crate::ReconciledRoute) with no `Complete`
//! variant and recorded the reason: "until S4 supplies verification, no code
//! path — tested or untested — can route a reconciled run to it." A bare
//! `Complete` variant added now would undo that in one line, because any code
//! anywhere could construct it.
//!
//! So `Complete` carries a [`VerifiedComplete`], whose fields are private and
//! whose only constructor is [`evaluate`]. The guarantee is the same shape as
//! S3's `TerminalAttempt`: the value cannot be spoken into existence, only
//! earned.
//!
//! ```compile_fail
//! # use conductor_core::completion::VerifiedComplete;
//! // Private fields: `evaluate` is the only way to obtain one.
//! let _ = VerifiedComplete { tree_hash: "t".to_string() };
//! ```
//!
//! Non-vacuity control — the same shape with something that *is* public:
//!
//! ```
//! # use conductor_core::completion::{CheckEvidence};
//! # use conductor_core::VerificationOutcome;
//! let _ = CheckEvidence {
//!     check_id: "typecheck".to_string(),
//!     outcome: VerificationOutcome::Pass,
//!     tree_hash: "t".to_string(),
//! };
//! ```
//!
//! # How a later slice is stopped from silently forgetting a criterion
//!
//! Two of the seven belong to slices that do not exist yet. They are **not**
//! quietly skipped and they are **not** hardcoded to `true`. Each has an
//! evidence type with exactly one variant — `NotEvaluated { owner }` — so today
//! there is no way to *write down* "policy is satisfied".
//!
//! When S7 lands and adds `PolicyEvidence::AllGrantsPresent`, [`evaluate`]'s
//! exhaustive `match` on that enum stops compiling. S7 therefore cannot ship
//! without deciding, at this gate, what its evidence means. The same holds for
//! S11 and acceptance bindings. Adding an eighth criterion breaks the match on
//! [`Criterion::ALL`] for the same reason.
//!
//! Belt and braces: [`VerifiedComplete::deferred`] carries the outstanding list
//! at runtime, and a test pins it, so a change is a deliberate edit rather than
//! a silence.

use serde::Serialize;

use crate::VerificationOutcome;

/// The slice that owes a criterion its implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Slice {
    /// This slice: the verification runner.
    S4Verification,
    /// The policy engine.
    S7Policy,
    /// The plan ledger, which is where acceptance criteria come from.
    S11PlanLedger,
}

/// One of §4.5's seven criteria.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Criterion {
    /// 1. Every required check `PASS` at the current tree hash.
    RequiredChecks,
    /// 2. Every conditional check triggered by the actual diff has run and passed.
    ConditionalChecks,
    /// 3. All invariant checks pass.
    InvariantChecks,
    /// 4. Zero unresolved findings.
    NoUnresolvedFindings,
    /// 5. Every acceptance criterion binds to ≥1 passing check.
    AcceptanceBindings,
    /// 6. Reconciliation verdict ∈ {`CLEAN_COMPLETE`, `CLEAN_NO_REPORT`}.
    ReconciliationVerdict,
    /// 7. Every policy-sensitive action has a matching, unexpired, correctly
    ///    scoped grant.
    PolicyGrants,
}

impl Criterion {
    /// All seven, in §4.5's order.
    pub const ALL: &'static [Criterion] = &[
        Criterion::RequiredChecks,
        Criterion::ConditionalChecks,
        Criterion::InvariantChecks,
        Criterion::NoUnresolvedFindings,
        Criterion::AcceptanceBindings,
        Criterion::ReconciliationVerdict,
        Criterion::PolicyGrants,
    ];

    /// Which slice implements it.
    pub fn owner(&self) -> Slice {
        match self {
            Criterion::RequiredChecks
            | Criterion::ConditionalChecks
            | Criterion::InvariantChecks
            | Criterion::NoUnresolvedFindings
            | Criterion::ReconciliationVerdict => Slice::S4Verification,
            Criterion::AcceptanceBindings => Slice::S11PlanLedger,
            Criterion::PolicyGrants => Slice::S7Policy,
        }
    }
}

/// One check's contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckEvidence {
    /// `check.id`.
    pub check_id: String,
    /// What it produced.
    pub outcome: VerificationOutcome,
    /// The tree it observed. Compared against the run's current tree, because
    /// §4.5's criterion 1 is "`PASS` **at the current tree hash**".
    pub tree_hash: String,
}

/// The results of one group of checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ChecksEvidence {
    checks: Vec<CheckEvidence>,
}

impl ChecksEvidence {
    /// Build from observed results.
    pub fn new(checks: impl IntoIterator<Item = CheckEvidence>) -> Self {
        ChecksEvidence {
            checks: checks.into_iter().collect(),
        }
    }

    /// The results.
    pub fn checks(&self) -> &[CheckEvidence] {
        &self.checks
    }

    fn refuse(&self, tree_hash: &str) -> Option<String> {
        for check in &self.checks {
            if check.tree_hash != tree_hash {
                return Some(format!(
                    "check {:?} was decided on tree {} but the run's tree is {}",
                    check.check_id, check.tree_hash, tree_hash
                ));
            }
            if !check.outcome.is_pass() {
                return Some(format!(
                    "check {:?} is {} at tree {}",
                    check.check_id,
                    check.outcome.as_str(),
                    tree_hash
                ));
            }
        }
        None
    }
}

/// How many findings are still open. §4.8: findings never auto-resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FindingsEvidence {
    unresolved: usize,
}

impl FindingsEvidence {
    /// Record the count observed in the store.
    pub fn unresolved(count: usize) -> Self {
        FindingsEvidence { unresolved: count }
    }
}

/// §4.8's verdict, reduced to what criterion 6 asks about.
///
/// A `String` for the unclean case rather than the `Verdict` enum itself:
/// `Verdict` lives in `conductor-git`, and `conductor-core` has no
/// dependencies. `conductor-git` provides the conversion, so call sites stay
/// typed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ReconciliationEvidence {
    /// `CLEAN_COMPLETE` or `CLEAN_NO_REPORT`.
    Clean,
    /// `POLICY_SENSITIVE`, **and the action that made it so was authorized**
    /// through §4.3 — added at S9.
    ///
    /// # Why criterion 6 has to admit this
    ///
    /// §4.5 states criterion 6 as "verdict ∈ {`CLEAN_COMPLETE`,
    /// `CLEAN_NO_REPORT`}" and criterion 7 as "every policy-sensitive action
    /// has a matching, unexpired, correctly scoped grant". Read literally, the
    /// two cannot both matter: a run with a policy-sensitive action never has a
    /// clean verdict — the manifest is still modified after the human approves —
    /// so criterion 6 refuses first and **criterion 7 can never be the
    /// deciding criterion**. A criterion that is unreachable by construction is
    /// not a criterion.
    ///
    /// Worse, it makes acceptance rows 12 and 13 vacuous in the other
    /// direction: "resumes on grant" would resume a run that then refuses to
    /// complete no matter what the human said, so the approval would authorize
    /// nothing.
    ///
    /// The reading that makes both criteria load-bearing is that criterion 6
    /// excludes the verdicts nobody has resolved — `CORRUPT`, `CONTRADICTED`,
    /// `OUT_OF_SCOPE`, `NO_CHANGE`, and `POLICY_SENSITIVE` *without* a grant —
    /// and that an authorized `POLICY_SENSITIVE` is resolved by criterion 7,
    /// which exists for exactly this case.
    ///
    /// This variant cannot be constructed from a verdict alone: the authorizing
    /// evidence is a required field, so "authorized" can only be claimed by a
    /// caller that has one.
    AuthorizedPolicySensitive {
        /// The verdict, still named honestly.
        verdict: String,
        /// What authorized it — the grant, or the rule that allowed it.
        authorization: String,
    },
    /// Any other verdict.
    NotClean {
        /// The verdict's name, for the refusal message.
        verdict: String,
    },
    /// Reconciliation has not run. Every exit from `RUNNING` passes through it
    /// (§4.8), so this is always a refusal.
    NotReconciled,
}

/// Criterion 5 — **owned by S11.**
///
/// One variant on purpose. See the module docs: adding the satisfied variant is
/// what makes [`evaluate`] stop compiling until S11 wires it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AcceptanceEvidence {
    /// The plan ledger, which owns acceptance criteria, does not exist yet.
    NotEvaluated {
        /// The slice that owes this.
        owner: Slice,
    },
}

/// Criterion 7 — **owned by S7/S8.**
///
/// One variant on purpose, for the same reason as [`AcceptanceEvidence`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PolicyEvidence {
    /// Nothing the attempt did was policy-sensitive, so criterion 7 has
    /// nothing to require. Added at S9.
    ///
    /// Distinct from [`PolicyEvidence::NotEvaluated`] on purpose: "we looked
    /// and there was nothing to authorize" and "nobody looked" are the two
    /// answers that must never be conflated, because only one of them is
    /// evidence.
    NoSensitiveActions,
    /// Every policy-sensitive action observed was authorized — §4.5's criterion
    /// 7, satisfied. Added at S9.
    AllGrantsPresent {
        /// Which action, and what authorized it.
        detail: String,
    },
    /// The policy engine does not exist yet.
    NotEvaluated {
        /// The slice that owes this.
        owner: Slice,
    },
}

/// Everything the gate reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompletionEvidence {
    /// The run's tree **now**. Every check is measured against this.
    pub tree_hash: String,
    /// Criterion 1.
    pub required: ChecksEvidence,
    /// Criterion 2.
    pub conditional: ChecksEvidence,
    /// Criterion 3.
    pub invariants: ChecksEvidence,
    /// Criterion 4.
    pub findings: FindingsEvidence,
    /// Criterion 5.
    pub acceptance: AcceptanceEvidence,
    /// Criterion 6.
    pub reconciliation: ReconciliationEvidence,
    /// Criterion 7.
    pub policy: PolicyEvidence,
}

/// Why a task may not complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    /// Which of the seven.
    pub criterion: Criterion,
    /// What is wrong, in terms a human can act on.
    pub detail: String,
}

/// Proof that every criterion S4 owns held at one tree.
///
/// Private fields. [`evaluate`] is the only constructor, which is what makes
/// `ReconciledRoute::Complete` unreachable without the evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifiedComplete {
    tree_hash: String,
    deferred: Vec<Criterion>,
}

impl VerifiedComplete {
    /// The tree the checks passed on.
    pub fn tree_hash(&self) -> &str {
        &self.tree_hash
    }

    /// Criteria that were **not** evaluated because their slice has not landed.
    ///
    /// Carried on the token rather than dropped, so a `COMPLETE` reached today
    /// is honest about what it did not check.
    pub fn deferred(&self) -> &[Criterion] {
        &self.deferred
    }
}

/// Evaluate all seven criteria. The only way to obtain a [`VerifiedComplete`].
///
/// Returns **every** unmet criterion rather than the first: a human deciding
/// what to do about a stuck task wants the whole picture.
pub fn evaluate(evidence: &CompletionEvidence) -> Result<VerifiedComplete, Vec<Refusal>> {
    let mut refusals = Vec::new();
    let mut deferred = Vec::new();

    // Exhaustive over `Criterion::ALL`: an eighth criterion added to the enum
    // makes this match fail to compile, which is the intended trap.
    for criterion in Criterion::ALL {
        match criterion {
            Criterion::RequiredChecks => {
                if evidence.required.checks().is_empty() {
                    // Vacuous truth is the danger here: "all required checks
                    // passed" is trivially true of a profile with none, and a
                    // task completing without a single check having run is
                    // precisely what "verification is authoritative" denies.
                    refusals.push(Refusal {
                        criterion: *criterion,
                        detail: "no required check has been run at this tree, so \
                                 nothing has been verified"
                            .to_string(),
                    });
                } else if let Some(detail) = evidence.required.refuse(&evidence.tree_hash) {
                    refusals.push(Refusal {
                        criterion: *criterion,
                        detail,
                    });
                }
            }
            Criterion::ConditionalChecks => {
                // An empty set is legitimate here: the diff may trigger nothing.
                if let Some(detail) = evidence.conditional.refuse(&evidence.tree_hash) {
                    refusals.push(Refusal {
                        criterion: *criterion,
                        detail,
                    });
                }
            }
            Criterion::InvariantChecks => {
                if let Some(detail) = evidence.invariants.refuse(&evidence.tree_hash) {
                    refusals.push(Refusal {
                        criterion: *criterion,
                        detail,
                    });
                }
            }
            Criterion::NoUnresolvedFindings => {
                if evidence.findings.unresolved > 0 {
                    refusals.push(Refusal {
                        criterion: *criterion,
                        detail: format!(
                            "{} finding(s) are unresolved; findings never \
                             auto-resolve (§4.8)",
                            evidence.findings.unresolved
                        ),
                    });
                }
            }
            Criterion::ReconciliationVerdict => match &evidence.reconciliation {
                ReconciliationEvidence::Clean => {}
                // See the variant's documentation: without this arm criterion 7
                // is unreachable and acceptance rows 12 and 13 authorize
                // nothing.
                ReconciliationEvidence::AuthorizedPolicySensitive { .. } => {}
                ReconciliationEvidence::NotClean { verdict } => refusals.push(Refusal {
                    criterion: *criterion,
                    detail: format!(
                        "the reconciliation verdict is {verdict}, not \
                         CLEAN_COMPLETE or CLEAN_NO_REPORT"
                    ),
                }),
                ReconciliationEvidence::NotReconciled => refusals.push(Refusal {
                    criterion: *criterion,
                    detail: "the attempt has not been reconciled; every exit from \
                             RUNNING passes through reconciliation (§4.8)"
                        .to_string(),
                }),
            },
            Criterion::AcceptanceBindings => match &evidence.acceptance {
                // When S11 adds a second variant this match stops being
                // exhaustive and the build fails here. That is the point.
                AcceptanceEvidence::NotEvaluated { .. } => deferred.push(*criterion),
            },
            Criterion::PolicyGrants => match &evidence.policy {
                // S9 wired the policy gate into the run path, so these two are
                // now real answers rather than a deferral.
                PolicyEvidence::NoSensitiveActions => {}
                PolicyEvidence::AllGrantsPresent { .. } => {}
                PolicyEvidence::NotEvaluated { .. } => deferred.push(*criterion),
            },
        }
    }

    if refusals.is_empty() {
        Ok(VerifiedComplete {
            tree_hash: evidence.tree_hash.clone(),
            deferred,
        })
    } else {
        Err(refusals)
    }
}
