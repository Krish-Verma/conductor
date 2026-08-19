//! Store errors.

use std::path::PathBuf;

/// Anything the store can fail with.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite said no.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Filesystem operation around the store path failed.
    #[error("io at {path}: {source}")]
    Io {
        /// Path being operated on.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },

    /// The store file does not exist and this open path is not allowed to
    /// create it.
    #[error("no store at {0}")]
    NotFound(PathBuf),

    /// The home directory could not be determined, so the default store path
    /// cannot be computed.
    #[error("cannot determine the default store path: ${0} is not set")]
    NoHome(&'static str),

    /// A pragma did not read back as configured. Fails closed: a durability
    /// pragma that was silently dropped is exactly the failure ADR-0004 left
    /// open for S1.
    #[error("PRAGMA {pragma} reads back as {actual:?}, expected {expected:?}")]
    PragmaMismatch {
        /// Pragma name.
        pragma: &'static str,
        /// Expected readback.
        expected: &'static str,
        /// Observed readback.
        actual: String,
    },

    /// `PRAGMA integrity_check` did not return `ok` after a migration.
    #[error("integrity_check after migration {version} returned {report:?}")]
    IntegrityCheckFailed {
        /// Migration whose commit was followed by the failing check.
        version: i64,
        /// What `integrity_check` reported.
        report: Vec<String>,
    },

    /// The database was written by a newer binary. Forward-only migrations mean
    /// there is no way down.
    #[error("store schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Version found in the database.
        found: i64,
        /// Highest version this binary knows.
        supported: i64,
    },

    /// The claim's `UPDATE … RETURNING` matched more than one row, which would
    /// mean two runs were claimed by one statement.
    #[error("claim matched {0} rows, expected at most 1")]
    ClaimMatchedMultipleRows(usize),

    /// A transaction failed and the rollback failed too. Both are reported: the
    /// rollback failure must not hide what went wrong first.
    #[error("rollback failed ({source}) while handling: {original}")]
    RollbackFailed {
        /// The error that caused the rollback, rendered.
        original: String,
        /// The rollback's own failure.
        source: rusqlite::Error,
    },

    /// The write carried a fencing epoch that is no longer current (§4.7).
    ///
    /// This is the rejection acceptance row 27 requires: a worker that stalled
    /// past its lease and woke up cannot write over its successor's work. A
    /// missing run reports `actual: None` — writing to a run that is not there
    /// is fenced out too.
    #[error("fenced out: write carried lease_epoch {expected}, run holds {actual:?}")]
    FencedOut {
        /// The epoch the caller believed it held.
        expected: i64,
        /// The epoch the database actually holds, or `None` if the run is gone.
        actual: Option<i64>,
    },

    /// A run was routed onwards from a state it is not in.
    ///
    /// §5.2: "`RECONCILING` is mandatory and unskippable", and `COMPLETE` sits
    /// downstream of `VERIFYING`. The type system stops a caller *naming*
    /// `COMPLETE` without evidence; this stops one skipping a state entirely.
    #[error("run {run_id} is not in {required}, so it cannot be routed onwards")]
    NotInState {
        /// The run.
        run_id: String,
        /// The state the transition required it to be in.
        required: conductor_core::RunState,
    },

    /// A task-state write §5.2's machine does not draw.
    ///
    /// The counterpart to [`StoreError::NotReconciling`] for the `task` row:
    /// S3's type-level guarantee covers `run.state`, and `task.state` is a
    /// second column that can carry the same claim.
    #[error("illegal task transition: {0}")]
    IllegalTaskTransition(String),

    /// A review transition §5.2's three-state machine does not draw.
    ///
    /// The counterpart to [`StoreError::IllegalTaskTransition`] for the `review`
    /// row. Modelled on it deliberately: `review.state` is a third column that
    /// can carry a state-machine claim, and the claim has to be refused by
    /// whatever writes the column rather than by each call site remembering to
    /// ask.
    #[error("illegal review transition: {0}")]
    IllegalReviewTransition(String),

    /// A review write required the review to be in a state it is not in.
    ///
    /// Reported when the guarded `UPDATE` changed no row, which is the authority
    /// — a prior `SELECT` says what was true when it ran. Distinct from
    /// [`StoreError::IllegalReviewTransition`] because the two describe different
    /// mistakes: an illegal transition is a caller asking for an edge §5.2 does
    /// not draw, while this is a caller asking for a legal edge from the wrong
    /// end of it.
    #[error("review {review_id} is not in {required}, so it cannot be moved")]
    ReviewNotInState {
        /// The review.
        review_id: String,
        /// The state the transition required it to be in.
        required: conductor_core::ReviewState,
    },

    /// A decision was recorded against a review that has no packet hash.
    ///
    /// §4.3's `REVIEW_ACCEPTANCE` authorizes **a review packet**, so a decision
    /// with nothing to bind to would be an approval of something nobody has read.
    /// The whole review-authority story rests on this refusal, which is why it is
    /// its own variant rather than a `Domain` string: a caller can match on it,
    /// and a test can prove it fired for this reason and not another.
    #[error("review {review_id} has no packet hash, so no decision can be recorded against it")]
    DecisionWithoutPacket {
        /// The review.
        review_id: String,
    },

    /// A second review was opened for a run that already has an open one.
    ///
    /// The guarantee is `ix_review_one_open_per_run`, the unique partial index;
    /// this variant is what [`crate::review::open`]'s pre-check reports so the
    /// refusal has a name instead of arriving as a bare constraint violation. The
    /// index is still there and still authoritative — a future second write path
    /// that forgets the pre-check is refused by the database, which is the point
    /// of putting the rule in the schema and not only in this crate.
    #[error("run {run_id} already has an open review ({open_review_id})")]
    ReviewAlreadyOpen {
        /// The run.
        run_id: String,
        /// The review that is already open for it.
        open_review_id: String,
    },

    /// An update named a review that does not exist.
    ///
    /// Separated from "the update changed nothing" for the reason
    /// [`StoreError::NoSuchTask`] gives: a silent no-op would let an operator
    /// believe a human decision had been recorded somewhere when it had been
    /// recorded nowhere.
    #[error("no review {0}")]
    NoSuchReview(String),

    /// A second resolution for a finding a human has already resolved.
    ///
    /// §4.8: findings never auto-resolve, and a resolution carries *why* a person
    /// accepted the finding. Overwriting one would delete the first human's
    /// reason and leave no trace that it had ever been given, so the second write
    /// is refused rather than applied.
    #[error("finding {finding_id} is already resolved; a second resolution would erase the first")]
    FindingAlreadyResolved {
        /// The finding.
        finding_id: String,
    },

    /// A resolution named a finding this run does not have.
    #[error("no finding {finding_id} on run {run_id}")]
    NoSuchFinding {
        /// The finding.
        finding_id: String,
        /// The run it was expected on.
        run_id: String,
    },

    /// A `TEXT` column held a value the domain does not recognise.
    #[error("invalid domain value read from the store: {0}")]
    Domain(String),

    /// An update named a task that does not exist.
    ///
    /// Distinguished from "the update changed nothing" on purpose: a silent
    /// no-op would let a caller believe it had recorded a task's execution
    /// requirements when it had recorded them nowhere, and §4.2's gate reads
    /// that column to decide whether a launch is permitted.
    #[error("no task {0}")]
    NoSuchTask(String),

    /// Event payload serialisation failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<conductor_core::IdError> for StoreError {
    fn from(value: conductor_core::IdError) -> Self {
        StoreError::Domain(value.to_string())
    }
}

impl From<conductor_core::ParseStateError> for StoreError {
    fn from(value: conductor_core::ParseStateError) -> Self {
        StoreError::Domain(value.to_string())
    }
}

/// A `review.decision` that is not one of §6.5's five reads as a domain error,
/// never as a default.
///
/// §6.5's decision is the most typo-exposed value in the system — a human types
/// it by hand — and one of the five advances a task to `COMPLETE`. §4.4's rule
/// for an action nobody recognises is that it *fails closed*, so an unreadable
/// column becomes an error on the read path rather than the most permissive known
/// decision on the write path.
impl From<conductor_core::ParseReviewDecisionError> for StoreError {
    fn from(value: conductor_core::ParseReviewDecisionError) -> Self {
        StoreError::Domain(value.to_string())
    }
}

/// Result alias for store operations.
pub type StoreResult<T> = Result<T, StoreError>;
