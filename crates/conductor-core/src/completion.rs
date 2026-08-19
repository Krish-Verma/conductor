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
//! Two of the seven belonged to slices that did not exist when this gate was
//! written. They were **not** quietly skipped and **not** hardcoded to `true`.
//! Each had an evidence type with exactly one variant — `NotEvaluated { owner }`
//! — so there was no way to *write down* "policy is satisfied".
//!
//! The trap is that the moment an owning slice adds a satisfied variant,
//! [`evaluate`]'s exhaustive `match` on that enum stops compiling: the slice
//! cannot ship without deciding, at this gate, what its evidence means. **It
//! has now fired twice** — S9 for [`PolicyEvidence`] and S11 for
//! [`AcceptanceEvidence`] — and both arms above are the decision it forced.
//! Adding an eighth criterion breaks the match on [`Criterion::ALL`] the same
//! way.
//!
//! `NotEvaluated` survives both landings, and does not mean "the slice is
//! missing" any more. It means the narrower thing that is still true of real
//! rows: for acceptance, that no plan document has ever been materialized for
//! this task, which is every `task` row created before S11.
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
    /// A verdict a human **accepted at a review boundary** — added at S13.
    ///
    /// # Why criterion 6 has to admit this too
    ///
    /// §5.2 draws `AWAITING_REVIEW → COMPLETE`, and until S13 nothing could take
    /// that edge. Every verdict that routes a run to `AWAITING_REVIEW` —
    /// `CONTRADICTED`, `OUT_OF_SCOPE`, `GOVERNANCE_VIOLATION`, an unresolved
    /// `POLICY_SENSITIVE` — is a verdict criterion 6 refuses, and
    /// [`ReconciledRoute::Complete`](crate::ReconciledRoute) carries a token only
    /// [`evaluate`] can mint. So the edge existed on the diagram and in
    /// [`crate::TaskState::successors`] while being unreachable in fact, which is
    /// the same shape of defect ADR-0013 found behind criterion 7 and ADR-0017
    /// found behind the core verb: a mechanism that is drawn, agreed, and has no
    /// caller.
    ///
    /// The resolution is the one criterion 7 already established for policy — a
    /// human decision enters the gate as *evidence*, not as an assertion — and
    /// like [`ReconciliationEvidence::AuthorizedPolicySensitive`] this variant
    /// cannot be constructed from a verdict alone: the authorizing grant is a
    /// required field, so "accepted" can only be claimed by a caller that holds
    /// one.
    ///
    /// # What acceptance does not do
    ///
    /// It resolves the **review boundary**, not the other six criteria. A human
    /// accepting a review is not a human overruling verification: criteria 1–3
    /// still require a `PASS` bound to the current tree, criterion 4 still counts
    /// findings that no human has resolved, and criterion 7 still wants its
    /// grants. If acceptance could excuse a failing check, §4.5's *"verification
    /// is authoritative"* would quietly become *"verification is advisory"* —
    /// and the decisions that exist for a failing check are `repair`,
    /// `revise_plan` and `stop`.
    ///
    /// The verdict is carried verbatim and is **not** rewritten to a clean one.
    /// A reader of the durable record must be able to see that a human accepted
    /// `CONTRADICTED`, because that is a materially different history from a run
    /// that was clean.
    AcceptedAtReview {
        /// The verdict, still named honestly. Never rewritten to a clean one.
        verdict: String,
        /// The `approval_grant.id` of the §4.3 `REVIEW_ACCEPTANCE` that
        /// authorized it.
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

/// One acceptance criterion, and what the checks it was bound to actually did.
///
/// # Evidence, not a verdict
///
/// The obvious alternative is a `satisfied: bool` filled in by whoever read the
/// plan. That would move criterion 5's meaning out of this gate and into every
/// caller, and the gate is the one place §4.5 is allowed to be interpreted.
/// Carrying the observed results instead means the tree-binding rule — *"`PASS`
/// **at the current tree hash**"* — is applied here, once, by the same code
/// that applies it to criteria 1, 2 and 3.
///
/// # Why both `verified_by` and `results`
///
/// They are not two spellings of one list. `verified_by` is what the plan
/// **declared**; `results` is what the run **observed**. A check the plan names
/// that produced no result at all is exactly the difference between them, and
/// it is a refusal — a conditional check whose trigger never matched proves
/// nothing. A refusal that could not name it would tell a human "something is
/// unbound" without saying what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CriterionEvidence {
    /// The criterion's id in the plan document — §3.6's stable ids.
    pub id: String,
    /// §3.7's escape hatch: *"mark it `manual: true`, which forces a review
    /// boundary."*
    pub manual: bool,
    /// The check ids the plan bound it to, in declaration order.
    pub verified_by: Vec<String>,
    /// The results observed for those check ids. A named check that did not
    /// run is simply absent here, which is why `verified_by` is also carried.
    pub results: Vec<CheckEvidence>,
}

impl CriterionEvidence {
    /// Why this criterion is not satisfied, or `None` when it is.
    fn refuse(&self, tree_hash: &str) -> Option<String> {
        if self.manual {
            return Some(format!(
                "criterion {:?} is manual, so nothing mechanical can satisfy it; \
                 §3.7 calls that the escape hatch that \"forces a review boundary\"",
                self.id
            ));
        }
        // "≥1 passing check", literally: one is enough. A sibling that failed
        // is refused by whichever of criteria 1–3 owns it, so reading this as
        // "all of them" would make criterion 5 a duplicate of those rather than
        // the separate question §4.5 asks.
        if self
            .results
            .iter()
            .any(|result| result.outcome.is_pass() && result.tree_hash == tree_hash)
        {
            return None;
        }
        if self.verified_by.is_empty() {
            return Some(format!(
                "criterion {:?} names no check at all; §3.7 refuses an unbound \
                 criterion at validation because it is \"the mechanism by which a \
                 task reaches COMPLETE on an agent's word\"",
                self.id
            ));
        }
        Some(format!(
            "criterion {:?} binds to {:?} and none of them passed at tree \
             {tree_hash}: {}",
            self.id,
            self.verified_by,
            self.observed(tree_hash)
        ))
    }

    /// What each named check produced, for the refusal message.
    fn observed(&self, tree_hash: &str) -> String {
        self.verified_by
            .iter()
            .map(
                |check_id| match self.results.iter().find(|r| &r.check_id == check_id) {
                    None => format!("{check_id} produced no result"),
                    Some(result) if result.tree_hash != tree_hash => format!(
                        "{check_id} is {} but on tree {}",
                        result.outcome.as_str(),
                        result.tree_hash
                    ),
                    Some(result) => format!("{check_id} is {}", result.outcome.as_str()),
                },
            )
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Criterion 5 — **owned by S11, and answered since S11.**
///
/// Three variants, for the reason [`PolicyEvidence`] gives for criterion 7:
/// *"the plan declares none"*, *"the plan declares these, and here is what each
/// bound check did"*, and *"no plan document has ever been read for this task"*
/// are three facts, and the first and third must never be conflated. Only the
/// third is the absence of evidence, and only the third defers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum AcceptanceEvidence {
    /// A plan was materialized for this task and declares no acceptance
    /// criteria — `task.acceptance_criteria = '[]'`. Criterion 5 has nothing to
    /// require, and says so rather than being skipped. Added at S11.
    ///
    /// **Vacuous, and safe here in a way criterion 1's empty set is not.**
    /// Criterion 1 still refuses a run in which no required check passed at
    /// this tree, so a criterion-less task cannot complete unverified; it
    /// merely has nothing *extra* bound to it. §3.7 does not refuse a
    /// criterion-less task at validation either, so refusing one here would
    /// enforce at the gate a rule its author was never told about.
    NoCriteria,
    /// The criteria the plan declares, with the results of the checks each was
    /// bound to. Added at S11.
    ///
    /// An **empty** vector is refused by [`evaluate`] rather than accepted:
    /// "I evaluated the criteria" naming none asserts nothing, and leaving it
    /// vacuously true would make the empty vector the easiest way to switch
    /// criterion 5 off. [`AcceptanceEvidence::NoCriteria`] is how a plan that
    /// declares none says so.
    Evaluated {
        /// One per criterion, in the plan's declaration order.
        criteria: Vec<CriterionEvidence>,
    },
    /// No plan document has ever been materialized for this task —
    /// `task.acceptance_criteria` is `NULL`, which is every row created before
    /// S11. The honest statement about one is *"this was never checked"*, not
    /// *"this was checked and found empty"*, and only this variant says the
    /// first thing.
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
                // S13. See the variant's documentation: without this arm §5.2's
                // `AWAITING_REVIEW → COMPLETE` edge is undrawable, and every
                // review decision except `stop` authorizes nothing.
                ReconciliationEvidence::AcceptedAtReview { .. } => {}
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
                // S11's plan ledger materializes `task.acceptance_criteria`, so
                // these two are now real answers rather than a deferral.
                AcceptanceEvidence::NoCriteria => {}
                AcceptanceEvidence::Evaluated { criteria } if criteria.is_empty() => {
                    refusals.push(Refusal {
                        criterion: *criterion,
                        detail: "an acceptance evaluation that names no criterion \
                                 has asserted nothing; a plan that declares none \
                                 is NoCriteria, and a task no plan has been \
                                 materialized for is NotEvaluated"
                            .to_string(),
                    });
                }
                AcceptanceEvidence::Evaluated { criteria } => {
                    // One refusal for the criterion, not one per acceptance
                    // criterion: `Refusal` is keyed by which of §4.5's seven
                    // failed, and a human reading a stuck task wants every
                    // unsatisfied criterion in that one entry rather than the
                    // list's length depending on how many the plan declared.
                    let details: Vec<String> = criteria
                        .iter()
                        .filter_map(|c| c.refuse(&evidence.tree_hash))
                        .collect();
                    if !details.is_empty() {
                        refusals.push(Refusal {
                            criterion: *criterion,
                            detail: details.join("; "),
                        });
                    }
                }
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
