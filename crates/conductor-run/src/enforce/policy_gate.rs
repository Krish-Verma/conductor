//! Turning a `POLICY_SENSITIVE` verdict into a decision and, when a human is
//! needed, into a durable request — acceptance rows 13, 12 and 25.
//!
//! §4.8 routes the verdict: *"`POLICY_SENSITIVE` | deps/lockfile/migrations/
//! git-config touched | policy evaluation → approval or review"*. S3 built the
//! verdict, S7 built the evaluation and S8 built the request — and nothing
//! joined them, which the master plan says in as many words:
//!
//! > What it does **not** do is turn a `require_approval` decision into an
//! > `approval_request` from inside a run: nothing in the run path creates one,
//! > because that is enforcement and S9 owns it.
//!
//! This module is that join, and it is the reason rows 12, 13 and 25 can be
//! scored at all.
//!
//! # The invariant it exists to establish
//!
//! **A run in `AWAITING_APPROVAL` has a pending `approval_request`.** Without
//! it, a run reaches a state whose only exit is a human granting something, and
//! there is nothing for a human to grant: the run waits forever and the CLI's
//! pending list is empty. S3 could route to `AWAITING_APPROVAL` and did; that it
//! was a dead end was invisible because no test asked what a human was supposed
//! to do next.
//!
//! # Reading the policy, and what happens when it cannot be read
//!
//! The policy comes from the **run's own snapshot** — `run.policy_hash` →
//! `policy_snapshot.canonical_blob` — never from the filesystem, so an edit to
//! `.conductor/policy.yaml` mid-run cannot change what the run is judged by
//! (§4.4, acceptance row 23).
//!
//! A snapshot that cannot be decoded is **not** treated as an empty policy. An
//! empty policy allows everything, so decoding failure would silently convert
//! "we cannot tell what the rules are" into "there are no rules" — on exactly
//! the code path that exists because something sensitive was touched. It routes
//! to approval with a `CRITICAL` finding instead: a human looks, and the reason
//! they are looking is recorded.
//!
//! # Deny does not create a request
//!
//! §4.4's `deny` is not approvable — that is what distinguishes it from
//! `require_approval` — so a denied action routes to `AWAITING_REVIEW` with a
//! finding, and no request is written. Creating one would offer a human a
//! button that must not exist.

use conductor_core::{Fence, ReconciledRoute, RunId};
use conductor_git::{FindingKind, Reconciliation};
use conductor_store::Store;

use crate::approval::kind::{Expiry, Subject};
use crate::approval::store::{NewApprovalRequest, RequestState, request};
use crate::policy::evaluate::{Request as PolicyRequest, evaluate};
use crate::policy::facts::{self};
use crate::policy::model::{Action, Effect};

/// How long a policy approval raised by a run stays open.
///
/// §4.3 makes a TTL **mandatory** for the policy kind, so a value has to be
/// chosen here rather than omitted. Eight hours: long enough to span a working
/// day so an operator who steps away does not return to an expired request, and
/// short enough that a request nobody answered does not sit open indefinitely
/// authorising an action whose facts have long since gone stale.
pub const POLICY_APPROVAL_TTL_MS: i64 = 8 * 60 * 60 * 1000;

/// What the policy decided about this attempt's observed changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyRoute {
    /// A human must authorize it. The request is already durable.
    Approval {
        /// `approval_request.id`.
        request_id: String,
        /// Why, in terms a human can act on.
        explanation: String,
    },
    /// Refused, or undecidable. A human looks; there is nothing to grant.
    Review {
        /// Why.
        detail: String,
    },
    /// Nothing the policy gates. The run continues on its ordinary route.
    Proceed,
}

impl PolicyRoute {
    /// §5.2's destination for this decision.
    pub fn route(&self) -> ReconciledRoute {
        match self {
            PolicyRoute::Approval { .. } => ReconciledRoute::AwaitingApproval,
            PolicyRoute::Review { .. } => ReconciledRoute::AwaitingReview,
            PolicyRoute::Proceed => ReconciledRoute::Verifying,
        }
    }
}

/// Anything this gate can fail with.
#[derive(Debug, thiserror::Error)]
pub enum PolicyGateError {
    /// The store said no.
    #[error("store: {0}")]
    Store(#[from] conductor_store::StoreError),
    /// The approval request could not be written.
    #[error("approval: {0}")]
    Approval(#[from] crate::approval::store::ApprovalError),
    /// A query against the approval tables failed.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Which §4.4 actions this reconciliation's observed deltas amount to.
///
/// Derived from what was **observed in the repository**, never from the agent's
/// report — §4.8's whole thesis. A finding kind maps to the action it evidences;
/// a changed path maps through S7's own extractors, so the mapping used to
/// decide is the same one used to explain.
///
/// Deliberately conservative: an observed delta that no action in §4.4's
/// taxonomy describes contributes no action, and a run whose sensitive deltas
/// are *all* of that kind therefore reaches the "no action" case below, which
/// asks a human rather than proceeding.
pub fn actions_for(reconciliation: &Reconciliation) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut push = |action: Action| {
        if !actions.contains(&action) {
            actions.push(action);
        }
    };

    for finding in reconciliation.findings() {
        match finding.kind {
            // Repository structure. `git.remote.modify` is row 14's action.
            FindingKind::RemoteChanged => push(Action::GitRemoteModify),
            FindingKind::RefRemoved => push(Action::GitBranchDelete),
            // A hook is code that runs on every future git operation in the
            // clone; §4.9 lists installing one among the operations that must
            // reach a human.
            FindingKind::HookAdded | FindingKind::HookChanged => push(Action::ArchitectureChange),
            FindingKind::ConfigChanged => push(Action::GitRemoteModify),
            _ => {}
        }
    }

    // Dependency manifests, lockfiles and migrations, through S7's extractors.
    let changed: Vec<String> = reconciliation.changed_paths.clone();
    if !facts::lockfile_modified(&changed).is_empty() {
        push(Action::LockfileModify);
    }
    for path in &changed {
        if is_dependency_manifest(path) {
            push(Action::DependencyAddRuntime);
        }
    }
    actions
}

/// Whether a path is a dependency manifest §4.4 would gate.
///
/// The same names S7's `SensitivePatterns` default uses. Kept here as a
/// function rather than duplicated as a pattern list so the two cannot drift
/// into disagreeing about what a manifest is.
fn is_dependency_manifest(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "Cargo.toml"
            | "package.json"
            | "pyproject.toml"
            | "requirements.txt"
            | "go.mod"
            | "Gemfile"
            | "pom.xml"
            | "build.gradle"
    )
}

/// Evaluate the run's pinned policy against what was observed, and record what
/// a human needs in order to answer.
///
/// Called only for a `POLICY_SENSITIVE` verdict: every other verdict is
/// §4.8's business and routes without consulting policy.
pub fn decide(
    store: &mut Store,
    fence: &Fence,
    reconciliation: &Reconciliation,
    now_ms: i64,
) -> Result<PolicyRoute, PolicyGateError> {
    let run_id = fence.run_id().clone();
    let actions = actions_for(reconciliation);

    // The snapshot, not the file (§4.4, row 23).
    let pinned = crate::policy::load::pinned_for_run(store.conn(), &run_id);
    let (policy, policy_hash) = match pinned {
        Ok(pinned) => {
            let hash = pinned.hash.clone();
            (Some(pinned.policy), hash)
        }
        Err(error) => {
            // Undecidable, which is not the same as permitted.
            let detail = format!(
                "the policy snapshot this run is pinned to could not be read, so \
                 no rule could be applied to a change that requires one: {error}"
            );
            raise(
                store,
                fence,
                &run_id,
                "POLICY_SNAPSHOT_UNREADABLE",
                &detail,
                now_ms,
            )?;
            let request_id = create_request(
                store,
                &run_id,
                Subject::PolicyAction {
                    action: Action::ArchitectureChange,
                },
                "unknown",
                Vec::new(),
                &detail,
                now_ms,
            )?;
            return Ok(PolicyRoute::Approval {
                request_id,
                explanation: detail,
            });
        }
    };
    let policy = policy.expect("Ok(pinned) always carries a policy");

    if actions.is_empty() {
        // Something sensitive changed and nothing in §4.4's taxonomy describes
        // it. Proceeding would mean advancing on the strength of *not having a
        // word for it*, which is the "unknown action fails closed" rule applied
        // one level up.
        let detail = format!(
            "the attempt changed something §4.8 treats as policy-sensitive, but \
             no §4.4 action describes it, so no rule could be evaluated; observed \
             findings: {}",
            reconciliation
                .findings()
                .iter()
                .map(|f| format!("{:?}", f.kind))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let request_id = create_request(
            store,
            &run_id,
            Subject::PolicyAction {
                action: Action::ArchitectureChange,
            },
            &policy_hash,
            Vec::new(),
            &detail,
            now_ms,
        )?;
        return Ok(PolicyRoute::Approval {
            request_id,
            explanation: detail,
        });
    }

    // Evaluate every observed action and take the strongest outcome. §4.4's
    // join is `max`, and an attempt that both added a dependency and repointed
    // a remote must be judged by the stricter of the two.
    let mut strongest = Effect::Allow;
    let mut gating_action = actions[0].clone();
    let mut gating_decision = None;
    let mut matched_rules: Vec<String> = Vec::new();
    let mut explanations: Vec<String> = Vec::new();

    for action in &actions {
        let decision = evaluate(&policy, &PolicyRequest::new(action.clone(), now_ms));
        for rule in &decision.matched {
            if !matched_rules.contains(&rule.rule_id) {
                matched_rules.push(rule.rule_id.clone());
            }
        }
        explanations.push(format!("{} → {:?}", action.as_str(), decision.effect));
        if decision.effect > strongest {
            strongest = decision.effect;
            gating_action = action.clone();
            gating_decision = Some(decision);
        } else if gating_decision.is_none() {
            gating_decision = Some(decision);
        }
    }
    let explanation = explanations.join("; ");

    match strongest {
        Effect::Allow => Ok(PolicyRoute::Proceed),
        Effect::RequireApproval => {
            // **Ask whether it has already been answered before asking again.**
            //
            // This is what makes acceptance row 12's "resumes on grant"
            // terminate. A granted run re-enters reconciliation — the work is
            // still in the workspace, so the verdict is `POLICY_SENSITIVE`
            // again and this function runs again. Without this check it would
            // raise a second request and route back to `AWAITING_APPROVAL`
            // forever: the grant would be unspendable and the loop would be
            // invisible, because every individual step looks correct.
            //
            // `authorize` recomputes the binding from the decision in hand and
            // compares it to the stored one; it never trusts the stored value
            // (S8). So a grant issued for a *different* action, a different
            // policy snapshot or a different run does not satisfy this, and an
            // expired or revoked one does not either.
            let decision = gating_decision.expect("a gating decision exists");
            let scope = run_scope(&run_id);
            match crate::approval::authorize(store.conn(), &decision, &scope, now_ms)? {
                crate::approval::Authorization::Authorized { grant_id } => {
                    // §4.3: consume immediately before the effect, and exactly
                    // once. A grant that authorised this run's continuation and
                    // stayed spendable would authorise the next one too.
                    let binding = crate::approval::Binding::for_decision(&decision, &scope).hash();
                    match crate::approval::store::consume(
                        store.conn_mut(),
                        &grant_id,
                        &binding,
                        now_ms,
                    )? {
                        // `Reusable` is a grant a human deliberately marked
                        // reusable (§4.3's `reuse`), which is spent by *not*
                        // being spent. Both are authorization to proceed; only
                        // the ledger entry differs.
                        crate::approval::store::Consumption::Consumed { .. }
                        | crate::approval::store::Consumption::Reusable { .. } => {
                            Ok(PolicyRoute::Proceed)
                        }
                        // Somebody else spent it between the check and here.
                        // Refusing is the only safe reading.
                        other => {
                            let detail = format!(
                                "grant {grant_id} authorised {} but could not be \
                                 consumed ({other:?}), so the run does not proceed \
                                 on it",
                                gating_action.as_str()
                            );
                            raise(
                                store,
                                fence,
                                &run_id,
                                "APPROVAL_NOT_CONSUMABLE",
                                &detail,
                                now_ms,
                            )?;
                            Ok(PolicyRoute::Review { detail })
                        }
                    }
                }
                crate::approval::Authorization::Refused(refusal) => {
                    let explanation = format!("{explanation} (not yet authorized: {refusal:?})");
                    let request_id = create_request(
                        store,
                        &run_id,
                        Subject::PolicyAction {
                            action: gating_action,
                        },
                        &policy_hash,
                        matched_rules,
                        &explanation,
                        now_ms,
                    )?;
                    Ok(PolicyRoute::Approval {
                        request_id,
                        explanation,
                    })
                }
            }
        }
        Effect::Deny => {
            // Not approvable. No request is written, deliberately.
            let detail = format!("policy denies {}: {explanation}", gating_action.as_str());
            raise(store, fence, &run_id, "POLICY_DENIED", &detail, now_ms)?;
            Ok(PolicyRoute::Review { detail })
        }
    }
}

/// Write the durable request a human answers.
///
/// The id is derived from the run and the subject, and the insert is
/// `INSERT`-once: a second attempt that reaches the same decision does not
/// stack a second request for a human to wade through.
fn create_request(
    store: &mut Store,
    run_id: &RunId,
    subject: Subject,
    policy_hash: &str,
    matched_rules: Vec<String>,
    explanation: &str,
    now_ms: i64,
) -> Result<String, PolicyGateError> {
    let stem = format!(
        "AR-{}-{}",
        run_id.as_str(),
        subject
            .binding_components()
            .join("-")
            .replace(['/', ' ', ':'], "_")
    );

    // Two different repeats have to be told apart, and getting this wrong is
    // how the first version of this function crashed:
    //
    // * **The same question, still open.** A second attempt reaching the same
    //   decision must reuse the open request, or a human wades through a stack
    //   of identical asks and granting one of them leaves the others dangling.
    //
    // * **The same question, already answered.** The previous request was
    //   granted and that grant has since been spent, revoked or expired. The
    //   answer is gone, so the question must be *asked again* — a new request,
    //   with a new id. Reusing the id would collide (the first version did,
    //   with a `UNIQUE constraint failed`), and reusing the *row* would be
    //   worse: it would resurrect a request a human already answered.
    let mut open: Option<String> = None;
    let mut next_ordinal = 1usize;
    {
        let mut stmt = store
            .conn()
            .prepare("SELECT id, state FROM approval_request WHERE id LIKE ?1 ESCAPE '\\'")?;
        let like = format!("{}%", stem.replace(['\\', '%', '_'], "\\$0"));
        let rows = stmt.query_map(rusqlite::params![like], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (existing_id, state) = row?;
            next_ordinal += 1;
            if state == RequestState::Requested.as_str() {
                open = Some(existing_id);
            }
        }
    }
    if let Some(open) = open {
        return Ok(open);
    }
    let id = format!("{stem}-{next_ordinal}");

    let new = NewApprovalRequest {
        id: id.clone(),
        subject,
        run_id: Some(run_id.clone()),
        facts: Default::default(),
        policy_hash: policy_hash.to_string(),
        matched_rules,
        explanation: explanation.to_string(),
        evidence_ref: None,
        expires: Expiry::At(now_ms + POLICY_APPROVAL_TTL_MS),
    };
    request(store.conn_mut(), &new, now_ms)?;
    Ok(id)
}

/// §4.8's verdict → §5.2's next state, **with policy consulted**.
///
/// The single routing function. `run_one_attempt` and §4.7's `recover_one` both
/// call it, because a run that reached `AWAITING_APPROVAL` through an attempt
/// and a run that reached it through recovery must be judged by the same rules —
/// and, more sharply, a *granted* run comes back through the recovery path, so a
/// policy check that lived only in the attempt path would never see the grant
/// and the run would wait forever.
///
/// Every verdict other than `POLICY_SENSITIVE` routes exactly as
/// [`crate::worker::route_for`] always did; policy is consulted only where §4.8
/// says to consult it.
pub fn route_reconciliation(
    store: &mut Store,
    fence: &Fence,
    reconciliation: &Reconciliation,
    now_ms: i64,
) -> Result<(ReconciledRoute, String, Option<String>), PolicyGateError> {
    if reconciliation.verdict != conductor_git::Verdict::PolicySensitive {
        return Ok((
            crate::worker::route_for(reconciliation),
            format!("verdict={}", reconciliation.verdict),
            None,
        ));
    }

    let decided = decide(store, fence, reconciliation, now_ms)?;
    let (detail, request_id) = match &decided {
        PolicyRoute::Approval {
            explanation,
            request_id,
        } => (
            format!("verdict={}; policy: {explanation}", reconciliation.verdict),
            Some(request_id.clone()),
        ),
        PolicyRoute::Review { detail } => (
            format!("verdict={}; policy: {detail}", reconciliation.verdict),
            None,
        ),
        PolicyRoute::Proceed => (
            format!(
                "verdict={}; policy authorizes it, so the run continues",
                reconciliation.verdict
            ),
            None,
        ),
    };
    Ok((decided.route(), detail, request_id))
}

/// §4.3's `{run: r-0041}` — the scope a run-raised approval is granted for.
///
/// The run id is *in the binding*, which is what stops a grant issued for one
/// run from authorizing another run that happens to have touched the same
/// manifest under the same policy.
pub fn run_scope(run_id: &RunId) -> crate::policy::model::Scope {
    crate::policy::model::Scope::from_pairs([("run".to_string(), run_id.as_str().to_string())])
}

fn raise(
    store: &mut Store,
    fence: &Fence,
    run_id: &RunId,
    kind: &str,
    detail: &str,
    now_ms: i64,
) -> Result<(), PolicyGateError> {
    store.record_finding(
        fence,
        &format!("f-{}-{kind}", run_id.as_str()),
        kind,
        "CRITICAL",
        detail,
        now_ms,
    )?;
    Ok(())
}
