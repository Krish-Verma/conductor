//! The review packet, composed — master plan §6.5.
//!
//! > **Review packet:** plan version and hash · task IDs · base commit and end
//! > state · the actual diff (linked, stat inline) · agent claims vs
//! > reconciliation verdict, side by side · every verification command with exit
//! > code and duration · policy evaluations and explanations · approvals granted
//! > with scope · deviations · unresolved findings · proposed next state.
//!
//! # Why this is the packet S12 did not build
//!
//! S12 shipped the other three and deliberately stopped here, because §6.5's
//! review packet has exactly one consumer — `review export` — and S13 owns it.
//! Building it a slice early would have produced a fourth packet with no reader,
//! which is precisely the defect ADR-0018 is about. So it arrives with its
//! consumer, and the two land together.
//!
//! # Its reader is a person, and that changes one thing
//!
//! The other three packets are read by an agent, so §6.5's context minimization
//! (ADR-0016) is about not drowning a model in irrelevant history. This one is
//! read by a human deciding whether to accept work, and the failure mode inverts:
//! the dangerous omission is not size but **a fact the reviewer needed and did not
//! get**. So the side-by-side is mandatory rather than summarised — an agent's
//! claim next to what git measured is the single most decision-relevant thing on
//! the page, and it is the pair acceptance row 6 exists for.
//!
//! What does *not* change is the bound. A 600 KB diff still travels as a path and
//! a digest ([`Evidence::linked`](super::Evidence::linked),
//! [`Diff::linked`](super::continuation::Diff::linked)) with the stat inline, and
//! [`super::emit`] still refuses an oversized packet rather than truncating it —
//! because a review packet silently missing its last finding is worse than one
//! that refuses to be written.
//!
//! # Why the fields are passed in rather than gathered here
//!
//! [`build`] takes the base from the store and the review half from its caller,
//! the same split [`super::continuation`] uses for `Observed`. Two reasons. The
//! reconciliation verdict is **not a persisted column** — it is re-derived by
//! comparing the stored baseline against the workspace — so a builder reaching
//! for it here would need a workspace path, an artifact root and a scope, none of
//! which a packet has any business knowing. And the *claims* half comes from an
//! agent report that the export path has already had to read and parse. Gathering
//! is the caller's job; being canonical, bounded and redacted is this module's.

use conductor_core::RunId;
use conductor_store::Store;
use serde::Serialize;
use serde_yaml::{Mapping, Value};

use super::continuation::Diff;
use super::implementation::{self, ImplementationPacket};
use super::{Emitted, PacketError};

/// What the agent said, in its own words — §6.5's left-hand column.
///
/// Every field is a claim. None of it is evidence, and the naming keeps that
/// visible to whoever reads the YAML: §4.8's whole doctrine is that this half is
/// one input to a classification whose other input is the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Claims {
    /// `COMPLETE`, `PARTIAL` or `FAILED` — or absent when no report arrived,
    /// which is acceptance rows 3 and 4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    /// The task the agent said it was working on. A cross-check, not an id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Paths the agent says it touched.
    pub files_touched: Vec<String>,
    /// The agent's prose.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// Commands the agent says it ran. Not verification.
    pub commands_run: Vec<String>,
    /// Acceptance criteria the agent claims it satisfied. Not a binding.
    pub acceptance_criteria: Vec<String>,
    /// §6.5's `deviations`: where the agent knowingly departed from the plan.
    pub deviations: Vec<String>,
    /// What stopped it.
    pub blockers: Vec<String>,
    /// What it asserts and nothing checked.
    pub unverified_claims: Vec<String>,
}

/// What the repository and the runtime measured — §6.5's right-hand column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Measured {
    /// §4.8's verdict, verbatim. Never rewritten to a friendlier one.
    pub reconciliation_verdict: String,
    /// The tree the verdict and every check below are bound to.
    pub tree_hash: String,
    /// What actually changed, measured out of git.
    pub changed_paths: Vec<String>,
    /// Commits Conductor made on the run branch.
    pub commits: Vec<String>,
}

/// One verification check, as §6.5 asks for it: *"every verification command with
/// exit code and duration"*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckLine {
    /// `check.id`.
    pub check_id: String,
    /// The argv, joined. Read from the profile, **not** from the store: §5.1
    /// persists `command_hash` and never the command text, so a reviewer asking
    /// "what did it actually run" cannot be answered from the row alone.
    pub command: String,
    /// `PASS` / `FAIL` / `INCONCLUSIVE` / `VOID`.
    pub outcome: String,
    /// `None` when the process never produced one — a timeout, or a program that
    /// was not there. Distinguished from `0` deliberately.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Wall-clock milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// The tree the result is bound to. Present so a reviewer can see a `PASS`
    /// that belongs to a *different* tree, which §4.5 says does not count.
    pub tree_hash: String,
}

/// One policy evaluation and its explanation — §6.5's *"policy evaluations and
/// explanations"*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyLine {
    /// The §4.4 action name.
    pub action: String,
    /// `allow` / `require_approval` / `deny`.
    pub decision: String,
    /// Why — the same prose `conductor policy explain` prints, so the reviewer
    /// and the 2 a.m. operator read one explanation rather than two.
    pub explanation: String,
}

/// One grant, with its scope — §6.5's *"approvals granted with scope"*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalLine {
    /// `approval_grant.id`.
    pub grant_id: String,
    /// §4.3's kind.
    pub kind: String,
    /// The scope pairs, rendered. A grant a reviewer cannot see the scope of is a
    /// grant they cannot judge.
    pub scope: String,
    /// Who granted it.
    pub granted_by: String,
}

/// One unresolved finding — §6.5's *"unresolved findings"*.
///
/// Only unresolved ones travel. A resolved finding already has a human's answer
/// attached, and §4.8 keeps it in the record; putting it in front of the *next*
/// reviewer would invite them to answer it twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindingLine {
    /// `finding.id`.
    pub id: String,
    /// `finding.kind`.
    pub kind: String,
    /// `CRITICAL`, `BLOCKING`, …
    pub severity: String,
    /// Where to look.
    pub evidence_ref: String,
}

/// §6.5's review packet: the implementation packet plus what a human needs to
/// decide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewFields {
    /// The tasks this review covers.
    ///
    /// A list because §6.5 says *"task IDs"*. In v1 a review boundary is per
    /// *run*, and a run belongs to one task, so it holds one member — including
    /// at a milestone boundary, where what fires the review is the run that
    /// finished the milestone's last task. Batching several tasks into one review
    /// is a cadence v1 does not offer, and the list shape is what would carry it
    /// if it ever does.
    pub tasks: Vec<String>,
    /// Which boundary fired this review.
    pub boundary: String,
    /// `task.state` when the packet was exported.
    pub end_state: String,
    /// What a human is being asked to agree to. **Advisory**: it is what
    /// Conductor would do next, not what the decision will be, and naming it
    /// here is how a reviewer notices they disagree.
    pub proposed_next_state: String,
    /// The agent's side.
    pub claims: Claims,
    /// The repository's side.
    pub measured: Measured,
    /// The diff — linked by path and digest, with the stat inline.
    pub diff: Diff,
    /// Every check, with exit code and duration.
    pub checks: Vec<CheckLine>,
    /// Policy evaluations and their explanations.
    pub policy: Vec<PolicyLine>,
    /// Grants, with scope.
    pub approvals: Vec<ApprovalLine>,
    /// Findings nobody has resolved.
    pub unresolved_findings: Vec<FindingLine>,
}

/// The implementation packet plus §6.5's review half.
#[derive(Debug, Clone)]
pub struct ComposedReviewPacket {
    base: ImplementationPacket,
    added: ReviewFields,
}

impl ComposedReviewPacket {
    /// The review half, as it was composed.
    pub fn added(&self) -> &ReviewFields {
        &self.added
    }

    fn to_value(&self) -> Value {
        // Start from the implementation packet, exactly as `repair` and
        // `continuation` do: the plan version, hash, objective, scope, acceptance
        // criteria and verification commands a reviewer needs are already there,
        // and a second document repeating them is a second document that can
        // disagree.
        let Value::Mapping(mut m) = self.base.to_value_for_continuation() else {
            unreachable!("an implementation packet is a mapping")
        };
        m.insert(Value::from("packet"), Value::from("review"));

        // Serialized from the typed value rather than re-listed field by field
        // here, for the reason `repair` gives: two spellings of one set of fields
        // is two things that can drift. Key order does not matter —
        // `super::canonical_bytes` sorts (§6.6).
        let added =
            serde_yaml::to_value(&self.added).unwrap_or_else(|_| Value::Mapping(Mapping::new()));
        m.insert(Value::from("review"), added);
        Value::Mapping(m)
    }

    /// The canonical bytes, whatever their size.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        super::canonical_bytes(&self.to_value())
    }

    /// Canonicalize, bound-check and hash.
    ///
    /// The bound is a refusal, not a truncation. A review packet that quietly
    /// dropped its last finding would be the worst possible thing to hand a human
    /// who is about to accept work.
    pub fn emit(&self) -> Result<Emitted, PacketError> {
        super::emit(&self.to_value())
    }

    /// `blake3:<hex>` over the canonical bytes.
    ///
    /// This is the hash a decision is **bound to**. A decision naming a different
    /// one is a decision about a packet this review did not export, and the import
    /// path refuses it — which is what stops an edited packet and a genuine
    /// decision being paired up.
    pub fn hash(&self) -> super::PacketHash {
        super::PacketHash::from_bytes(&self.canonical_bytes())
    }

    /// The packet as YAML — what `review export` writes and a human reads.
    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(&super::render(&self.to_value())).unwrap_or_default()
    }
}

/// Compose §6.5's review packet for one run.
///
/// The base is rebuilt from `run_id` rather than accepted, for the reason
/// [`super::repair::build`] gives: a caller-supplied base could describe a
/// different run, and a packet that reads coherently and is about the wrong run is
/// worse than one that fails to build.
pub fn build(
    store: &mut Store,
    run_id: &RunId,
    added: &ReviewFields,
) -> Result<ComposedReviewPacket, PacketError> {
    Ok(ComposedReviewPacket {
        base: implementation::build(store, run_id)?,
        added: added.clone(),
    })
}
