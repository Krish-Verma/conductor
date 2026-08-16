//! The continuation packet — master plan §6.5.
//!
//! > **Continuation packet** = implementation packet **plus observed reality**:
//! > `reconciliation_verdict`, current tree hash vs base, the actual diff so
//! > far, which criteria already verify green at the current tree, commits in
//! > the run clone, the partial report if any, and explicitly:
//! >
//! > *"The previous agent's reasoning is not available. Treat its intent as
//! > inferable only from the diff."*
//!
//! # What that last sentence is for
//!
//! It is S12's stop point — *"recovery does not depend on hidden state"* —
//! written as a line the agent reads. Every other field here is evidence about
//! the world; that one is a statement about what is **missing**, and it exists
//! because the alternative failure is silent. An agent handed a half-finished
//! diff with no explanation will otherwise reason as though the plan that
//! produced it were recoverable, and either wait for context that does not
//! exist or invent one and call it continuity.
//!
//! Conductor cannot supply the previous agent's reasoning even in principle:
//! §6.1 launches agents as subprocesses precisely so they can be killed, and a
//! killed process's chain of thought is not a thing the store holds. Session
//! resume, where an adapter has it, is an **optimization** — never a
//! correctness dependency (§6.1's capability table calls it
//! `session_resume`, and nothing in the recovery path branches on it).
//!
//! # Observed reality is a parameter, not something this module measures
//!
//! [`Observed`] is passed in. The reconciler produces the verdict, the tree
//! hasher produces the tree hash, git produces the commits — each already owns
//! its measurement, and a packet builder that re-derived any of them would be a
//! second answer to a question something else already answered. What this module
//! owns is the *shape*: which of those facts travel, in what order, under what
//! budget.

use conductor_core::RunId;
use conductor_store::Store;
use serde_yaml::{Mapping, Value};

use super::implementation::{self, ImplementationPacket};
use super::{Emitted, PacketError};

/// §6.5's verbatim sentence. A constant, so the wording cannot drift between
/// the packet and the section that specifies it.
pub const NO_PRIOR_REASONING: &str = "The previous agent's reasoning is not available. \
                                      Treat its intent as inferable only from the diff.";

/// The diff so far — summarised inline, or linked when it is large.
///
/// §6.5 keeps *"the actual diff so far"* in the continuation packet while §6.6
/// bounds the packet, and those pull in opposite directions for exactly one
/// field. Resolved the way §6.5 resolves it everywhere else: the shape travels,
/// the bytes are linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    files: Option<usize>,
    insertions: Option<usize>,
    deletions: Option<usize>,
    path: Option<std::path::PathBuf>,
    digest: Option<String>,
}

impl Diff {
    /// A stat line: how many files, how many lines each way.
    pub fn summary(files: usize, insertions: usize, deletions: usize) -> Diff {
        Diff {
            files: Some(files),
            insertions: Some(insertions),
            deletions: Some(deletions),
            path: None,
            digest: None,
        }
    }

    /// A path and a digest, for a diff too large to inline.
    pub fn linked(path: impl AsRef<std::path::Path>) -> Result<Diff, PacketError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| PacketError::Io {
            what: "the run diff",
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Diff {
            files: None,
            insertions: None,
            deletions: None,
            path: Some(path.to_path_buf()),
            digest: Some(format!("blake3:{}", blake3::hash(&bytes).to_hex())),
        })
    }

    /// Nothing observed yet.
    pub fn none() -> Diff {
        Diff {
            files: None,
            insertions: None,
            deletions: None,
            path: None,
            digest: None,
        }
    }

    fn to_value(&self) -> Value {
        let mut m = Mapping::new();
        if let Some(f) = self.files {
            m.insert(Value::from("files"), Value::from(f));
        }
        if let Some(i) = self.insertions {
            m.insert(Value::from("insertions"), Value::from(i));
        }
        if let Some(d) = self.deletions {
            m.insert(Value::from("deletions"), Value::from(d));
        }
        if let Some(p) = &self.path {
            m.insert(Value::from("path"), Value::from(p.display().to_string()));
        }
        if let Some(d) = &self.digest {
            m.insert(Value::from("digest"), Value::from(d.clone()));
        }
        Value::Mapping(m)
    }
}

/// What the world looks like now — §6.5's "observed reality".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// §4.8's verdict for the attempt that stopped.
    pub reconciliation_verdict: String,
    /// The tree hash the workspace is at now. Compared against the run's
    /// `base_commit`, which the packet already carries.
    pub tree_hash: String,
    /// The diff so far.
    pub diff: Diff,
    /// Acceptance criterion ids that already verify green **at this tree**.
    ///
    /// "At this tree" is load-bearing: §4.5 binds a result to the exact tree it
    /// observed, so a criterion that was green two commits ago is not evidence
    /// about the tree the next agent will start from.
    pub criteria_green: Vec<String>,
    /// Commits in the run clone.
    pub commits: Vec<String>,
    /// The partial report, if the previous agent wrote one before it stopped.
    pub partial_report: Option<String>,
}

impl Observed {
    /// Nothing observed — a crash before anything happened.
    pub fn none() -> Observed {
        Observed {
            reconciliation_verdict: "NO_CHANGE".to_string(),
            tree_hash: String::new(),
            diff: Diff::none(),
            criteria_green: Vec::new(),
            commits: Vec::new(),
            partial_report: None,
        }
    }
}

/// §6.5's continuation packet.
#[derive(Debug, Clone)]
pub struct ContinuationPacket {
    base: ImplementationPacket,
    observed: Observed,
}

impl ContinuationPacket {
    fn to_value(&self) -> Value {
        // Start from the implementation packet, so "plus observed reality" is
        // literally what this is rather than a second document that happens to
        // repeat most of it.
        let Value::Mapping(mut m) = self.base.to_value_for_continuation() else {
            unreachable!("an implementation packet is a mapping")
        };
        m.insert(Value::from("packet"), Value::from("continuation"));

        let mut o = Mapping::new();
        o.insert(
            Value::from("reconciliation_verdict"),
            Value::from(self.observed.reconciliation_verdict.clone()),
        );
        o.insert(
            Value::from("tree_hash"),
            Value::from(self.observed.tree_hash.clone()),
        );
        o.insert(Value::from("diff"), self.observed.diff.to_value());
        let mut green = self.observed.criteria_green.clone();
        green.sort();
        o.insert(
            Value::from("criteria_already_green"),
            Value::Sequence(green.into_iter().map(Value::from).collect()),
        );
        // Commit order is content — it is the order they were made in — so this
        // one is deliberately not sorted.
        o.insert(
            Value::from("commits"),
            Value::Sequence(
                self.observed
                    .commits
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        if let Some(report) = &self.observed.partial_report {
            o.insert(Value::from("partial_report"), Value::from(report.clone()));
        }
        m.insert(Value::from("observed"), Value::Mapping(o));
        m.insert(
            Value::from("prior_session"),
            Value::from(NO_PRIOR_REASONING),
        );
        Value::Mapping(m)
    }

    /// The canonical bytes, whatever their size.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        super::canonical_bytes(&self.to_value())
    }

    /// Canonicalize, bound-check and hash.
    pub fn emit(&self) -> Result<Emitted, PacketError> {
        super::emit(&self.to_value())
    }

    /// `blake3:<hex>` over the canonical bytes.
    pub fn hash(&self) -> super::PacketHash {
        super::PacketHash::from_bytes(&self.canonical_bytes())
    }

    /// The packet as YAML — what the next agent is handed.
    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(&self.to_value()).unwrap_or_default()
    }
}

/// Build the continuation packet for one run — §6.5.
pub fn build(
    store: &mut Store,
    run_id: &RunId,
    observed: &Observed,
) -> Result<ContinuationPacket, PacketError> {
    Ok(ContinuationPacket {
        base: implementation::build(store, run_id)?,
        observed: observed.clone(),
    })
}
