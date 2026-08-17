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
    /// Which paths the repository shows as changed, sorted.
    ///
    /// §6.5 lists *"the actual diff so far"*, and [`Diff`] answers "how much" while
    /// this answers "where" — which is the half a continuing agent acts on. A
    /// summary alone tells it three files moved and leaves it to search the tree
    /// for them; the contents stay linked rather than embedded, so this is paths
    /// only and the budget is unaffected.
    ///
    /// Read from git at §4.8's reconciliation, never from a report: the previous
    /// agent died without writing one, and even a report that existed would be
    /// evidence rather than the answer.
    pub changed_paths: Vec<String>,
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
    /// Whether anything survived worth telling the next agent about.
    ///
    /// This is the line between acceptance rows 2 and 3. Row 2 — a crash before
    /// edits — says the next attempt gets *"the same packet"*, and a continuation
    /// packet whose observed half is empty would be that packet plus a section
    /// saying nothing happened. Row 3 is the case where something did.
    ///
    /// A commit counts even with no working-tree change: the agent may have
    /// committed its work before dying, and that is precisely the state the next
    /// agent must not duplicate.
    pub fn is_empty(&self) -> bool {
        self.tree_hash.is_empty()
            && self.changed_paths.is_empty()
            && self.commits.is_empty()
            && self.partial_report.is_none()
            && self.diff == Diff::none()
    }

    /// Nothing observed — a crash before anything happened.
    pub fn none() -> Observed {
        Observed {
            reconciliation_verdict: "NO_CHANGE".to_string(),
            tree_hash: String::new(),
            diff: Diff::none(),
            changed_paths: Vec::new(),
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
        let mut paths = self.observed.changed_paths.clone();
        paths.sort();
        paths.dedup();
        o.insert(
            Value::from("changed_paths"),
            Value::Sequence(paths.into_iter().map(Value::from).collect()),
        );
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
        serde_yaml::to_string(&super::render(&self.to_value())).unwrap_or_default()
    }
}

/// Measure observed reality for a run whose previous attempt never finished.
///
/// # Why every field is measured and none is assumed
///
/// §4.8's table says a crashed attempt whose changes are in scope reconciles
/// `CLEAN_NO_REPORT`, so this function could have written that string and moved
/// on. It does not, because the verdict is the one field a reader would trust
/// most and the one a stale assumption would corrupt most quietly: a crash that
/// also touched `.conductor/**`, or moved a remote, reconciles as something else
/// entirely, and a packet that told the next agent `CLEAN_NO_REPORT` about it
/// would be describing a run that did not happen.
///
/// So it does what §4.7's recovery does — re-observe the workspace against the
/// baseline the attempt stored, and classify — which is also why it needs the
/// artifact root and an ordinal. §4.7 calls this *"startup reconciliation from
/// disk"*; this is the same measurement for a different reader.
///
/// A baseline that cannot be read yields [`Observed::none`] rather than an error.
/// That is the fail-closed direction *for this question*: the caller's fallback is
/// the implementation packet, which tells the agent less than the truth rather
/// than something untrue.
pub fn observe_run(
    store: &Store,
    run_id: &RunId,
    workspace: &std::path::Path,
    artifacts_root: &std::path::Path,
    previous_ordinal: i64,
    scope: &conductor_git::Scope,
    sensitive: &conductor_git::SensitivePatterns,
) -> Observed {
    let baseline_path = crate::paths::ArtifactRoot::new(artifacts_root)
        .attempt_dir(run_id, previous_ordinal)
        .join("baseline.json");
    let Ok(bytes) = std::fs::read(&baseline_path) else {
        return Observed::none();
    };
    let Ok(baseline) = serde_json::from_slice::<conductor_git::Baseline>(&bytes) else {
        return Observed::none();
    };
    let Ok(observed) = conductor_git::observe(workspace, &baseline) else {
        return Observed::none();
    };
    let reconciliation =
        conductor_git::reconcile(&baseline, &observed, scope, sensitive, None, None);

    // Which criteria already verify green **at the tree that is there now**. Read
    // from `verification_check` rather than recomputed: §4.5 binds a result to a
    // tree hash, and re-running the checks here would be a second verification
    // nobody asked for.
    let mut criteria_green = stored_passing_checks(store, run_id);
    criteria_green.sort();
    criteria_green.dedup();

    // The partial report, if the dead agent left one. §6.5 lists it, and §4.8 is
    // the reason it is only ever *evidence*: a report with no attempt behind it
    // says what the agent believed, not what happened.
    let partial_report = std::fs::read_to_string(
        crate::paths::ArtifactRoot::new(artifacts_root)
            .attempt_dir(run_id, previous_ordinal)
            .join("report.json"),
    )
    .ok();

    Observed {
        reconciliation_verdict: reconciliation.verdict.to_string(),
        tree_hash: observed.repo.tree_hash.clone(),
        diff: Diff::summary(reconciliation.changed_paths.len(), 0, 0),
        changed_paths: reconciliation.changed_paths.clone(),
        criteria_green,
        // Oid and subject, in the order they were made — §6.5 asks for *"commits
        // in the run clone"*, and a bare oid tells the next agent nothing about
        // what its predecessor thought it was doing. Parents are dropped: the
        // topology is not something the agent acts on, and §6.5 bounds the packet.
        commits: observed
            .new_commits
            .iter()
            .map(|c| format!("{} {}", short_oid(&c.oid), c.subject))
            .collect(),
        partial_report,
    }
}

/// A commit id, shortened the way git does for a human.
///
/// Twelve, not seven: §6.5's packet is read by an agent that may `git show` what
/// it finds, and a prefix short enough to be ambiguous in a busy repository is a
/// command that fails for a reason the agent cannot diagnose.
fn short_oid(oid: &str) -> &str {
    &oid[..oid.len().min(12)]
}

/// Check ids this run has a `PASS` for.
fn stored_passing_checks(store: &Store, run_id: &RunId) -> Vec<String> {
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT DISTINCT check_id FROM verification_check
          WHERE run_id = ?1 AND outcome = 'PASS'",
    ) else {
        return Vec::new();
    };
    stmt.query_map(rusqlite::params![run_id.as_str()], |row| row.get(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
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
