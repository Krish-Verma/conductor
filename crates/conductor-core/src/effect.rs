//! The side-effect ledger's identity and vocabulary — master plan §4.7.
//!
//! ```text
//! operation_id = blake3(kind ‖ run_id ‖ attempt_ordinal ‖ tree_hash)
//!
//! BEGIN IMMEDIATE; INSERT side_effect(…, 'INTENDED', precondition); COMMIT;
//!     perform the effect                                  ← crash window
//! BEGIN IMMEDIATE; UPDATE side_effect SET state='CONFIRMED', receipt=?; COMMIT;
//! ```
//!
//! On restart an `INTENDED` row is resolved by **re-checking the precondition
//! against the world, never by blind retry**. That is why [`Precondition`] is a
//! typed, serialisable question rather than a comment: the row has to carry
//! enough to answer "did it happen?" on a machine that has forgotten everything
//! else. When the answer is indeterminate the row becomes
//! [`SideEffectState::Ambiguous`], the run halts and a human decides. **Never
//! guess.**

use crate::ids::RunId;
use crate::state::state_enum;
use serde::{Deserialize, Serialize};

state_enum! {
    /// `side_effect.state` — Part 5.1.
    SideEffectState {
        Intended => "INTENDED",
        Confirmed => "CONFIRMED",
        Failed => "FAILED",
        Ambiguous => "AMBIGUOUS",
    }
    terminal: [Confirmed, Failed]
}

impl SideEffectState {
    /// Whether reaching this state stops the run until a human decides.
    ///
    /// Only `AMBIGUOUS`. `INTENDED` is resolvable by re-checking; `CONFIRMED`
    /// and `FAILED` are decided answers.
    pub fn halts_the_run(&self) -> bool {
        matches!(self, SideEffectState::Ambiguous)
    }
}

state_enum! {
    /// `side_effect.kind` — the effects Conductor performs itself.
    ///
    /// The set is closed on purpose. §4.7: "an effect Conductor cannot verify
    /// afterwards is an effect Conductor may not own", which is what keeps
    /// deployment out of v1 by construction rather than by scope decision.
    /// `git.commit.local` and `git.fetch_into_main` are listed by §4.7 and are
    /// implemented by S5; they are absent here because a kind whose
    /// intent/confirm path does not exist is a lie the ledger would tell on
    /// restart.
    SideEffectKind {
        WorkspaceCreate => "workspace.create",
        ArtifactWrite => "artifact.write",
    }
    terminal: []
}

impl SideEffectKind {
    /// The question a restart asks the world about an `INTENDED` row of this
    /// kind (§4.7's table).
    pub fn did_it_happen_question(&self) -> &'static str {
        match self {
            SideEffectKind::WorkspaceCreate => "does the path exist with the expected HEAD?",
            SideEffectKind::ArtifactWrite => "does the file exist with the expected content hash?",
        }
    }
}

/// What must be true of the world for the effect to have happened.
///
/// Stored as JSON in `side_effect.precondition`. No `deny_unknown_fields`: a
/// row written by a newer Conductor must still be *readable* by an older one,
/// because refusing to parse a recovery record is the one failure mode this
/// table cannot afford.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum Precondition {
    /// A file exists at `path` and its content hashes to `content_hash`.
    FileWithHash {
        /// Absolute path to the file.
        path: String,
        /// `blake3:<hex>` of the file's bytes.
        content_hash: String,
    },
    /// A git working tree exists at `path` and its `HEAD` resolves to `head`.
    WorkspaceAtHead {
        /// Absolute path to the workspace.
        path: String,
        /// The commit `HEAD` must resolve to.
        head: String,
    },
}

impl Precondition {
    /// The path this precondition is about, for reporting.
    pub fn path(&self) -> &str {
        match self {
            Precondition::FileWithHash { path, .. } => path,
            Precondition::WorkspaceAtHead { path, .. } => path,
        }
    }
}

/// `side_effect.operation_id`.
///
/// Rendered `blake3:<hex>`, matching every other hash Part 5.1 stores.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OperationId(String);

impl OperationId {
    /// `blake3(kind ‖ run_id ‖ attempt_ordinal ‖ tree_hash)`, §4.7.
    ///
    /// The components are joined with a byte that cannot occur in any of them
    /// (`0x1f`, unit separator). §4.7 writes the concatenation with `‖` and does
    /// not say what it is; plain concatenation would make
    /// `("r-0041", ordinal 1)` and `("r-004", ordinal 11)` the same operation,
    /// which in a ledger whose job is to prevent duplicate effects is not a
    /// theoretical objection.
    pub fn compute(
        kind: SideEffectKind,
        run_id: &RunId,
        attempt_ordinal: i64,
        tree_hash: &str,
    ) -> Self {
        const SEP: &[u8] = &[0x1f];
        let mut hasher = blake3::Hasher::new();
        hasher.update(kind.as_str().as_bytes());
        hasher.update(SEP);
        hasher.update(run_id.as_str().as_bytes());
        hasher.update(SEP);
        hasher.update(attempt_ordinal.to_string().as_bytes());
        hasher.update(SEP);
        hasher.update(tree_hash.as_bytes());
        OperationId(format!("blake3:{}", hasher.finalize().to_hex()))
    }

    /// The stored text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Rebuild an id from what the database holds.
    ///
    /// Deliberately not a general constructor: the name says where the value is
    /// expected to come from, so a caller cannot mistake it for a way to invent
    /// an operation identity that [`OperationId::compute`] would not produce.
    pub fn from_stored(stored: impl Into<String>) -> Self {
        OperationId(stored.into())
    }
}

impl std::fmt::Display for OperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `blake3:<hex>` of a byte slice — the content hash Conductor stores.
pub fn content_hash(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}
