//! The `review` row — §5.2's three-state review machine, §6.5's packet and
//! imported decision.
//!
//! Three rules, and each of them is a refusal rather than a feature:
//!
//! 1. **Every state write consults [`ReviewState::transition_to`].** That is why
//!    this module exists instead of the CLI writing `UPDATE review SET state = …`
//!    directly: §5.2 draws `PENDING → EXPORTED → DECIDED` and nothing else, and a
//!    rule enforced at each call site is a rule that is missing from the next one.
//!    `task.rs` says the same thing about `task.state`; this is the third column
//!    that can carry a state-machine claim, after `task.state` and
//!    `plan_version.state`.
//! 2. **A decision binds to a packet hash.** §4.3's `REVIEW_ACCEPTANCE`
//!    authorizes *a review packet*, so [`record_decision`] refuses a review whose
//!    `packet_hash` is `NULL` — in the `UPDATE`'s own `WHERE` clause, not only on
//!    a prior `SELECT`. A decision that floats free of the bytes a human read is
//!    an approval of something nobody has seen.
//! 3. **`DECIDED` is terminal, and the refusal is checked at the row.** §6.5
//!    makes importing a decision "a **mutating** operation … never a file an agent
//!    could write", which makes the import path somewhere an attacker would like
//!    to arrive twice. Every write is `UPDATE … WHERE id = ? AND state = ?` and
//!    treats `changed == 0` as the refusal, so a second arrival is told no even if
//!    the state moved between the read and the write.
//!
//! **Unfenced, deliberately, and for a stronger reason than [`crate::task`]'s.**
//! A review is answered by a *person*, not by a lease-holder: §4.7 gives leases
//! to runs "in a claimable state", and a run in `AWAITING_REVIEW` has released
//! its lease and is waiting. Demanding a [`conductor_core::Fence`] here would
//! mean claiming the run in order to record a human's decision about it, which
//! would put the review write inside the very execution context §6.5 keeps it
//! out of. Exclusion comes from `BEGIN IMMEDIATE` plus the guarded `UPDATE`, the
//! same combination [`crate::lease::resume_after_grant`] uses for the same
//! reason.
//!
//! # On the `event` rows this module writes
//!
//! Each transition appends one `event` row, the way [`crate::lease`] does, so a
//! human decision leaves a trace in the same evidence log as everything else.
//! `conductor-core` has no review-shaped [`EventKind`] and S13 does not own that
//! crate, so the closest existing variant is used and the payload names the
//! entity explicitly; see the `// S13:` note at [`append_review_event`].

use conductor_core::{EventKind, PlanVersionId, ReviewDecision, ReviewState, RunId, TaskId};
use rusqlite::{Connection, Transaction, params};
use serde::Serialize;

use crate::error::{StoreError, StoreResult};
use crate::lease::append_event;
use crate::tx::with_immediate;

/// What opening a review needs.
///
/// A named struct rather than five positional `&str`s: three of the five are ids
/// that all render as short lower-case strings, and a transposed pair would
/// produce a review that confidently describes the wrong work. The three ids are
/// typed for the same reason — `run_id: RunId` cannot be handed a task id by
/// accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReview {
    /// `review.id`.
    pub id: String,
    /// The run under review.
    pub run_id: RunId,
    /// The task the run executes. Denormalized on purpose; see
    /// [`crate::schema::SCHEMA_V9`].
    pub task_id: TaskId,
    /// The plan version the work was authorized under — as it was when the
    /// review opened, which is not the same question as "which plan version does
    /// this task belong to now".
    pub plan_version_id: PlanVersionId,
    /// Which review boundary fired (Part 8, S13), in the words of whatever
    /// opened the review.
    pub boundary: String,
}

/// One `review` row, as the export and import paths read it.
///
/// `created_at` is deliberately absent: nothing in §6.5 decides anything from it,
/// and the ordering it exists to support is applied inside
/// [`reviews_for_run`] rather than re-derived by every caller. A field on a public
/// struct is a promise that somebody will read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReviewRow {
    /// `review.id`.
    pub id: String,
    /// The run under review.
    pub run_id: RunId,
    /// The task the run executes.
    pub task_id: TaskId,
    /// The plan version the review was opened against.
    pub plan_version_id: PlanVersionId,
    /// Which boundary fired.
    pub boundary: String,
    /// §5.2's review state.
    pub state: ReviewState,
    /// The exported packet's content hash. `None` **only** while the review is
    /// `PENDING`; a decision can never be recorded while it is `None`.
    pub packet_hash: Option<String>,
    /// Where the exported packet was written. `None` until exported.
    pub packet_path: Option<String>,
    /// The human's answer — one of §6.5's five. `None` until decided.
    pub decision: Option<ReviewDecision>,
    /// Who answered. `None` until decided.
    ///
    /// **Not an identity boundary.** ADR-0002: a `0600` socket alone is not a
    /// human-identity boundary, and neither is a string in this column. It
    /// records what the answering channel asserted, for the audit trail; approval
    /// integrity depends on measured execution containment, not on this field.
    pub decided_by: Option<String>,
    /// When it was answered. `None` until decided.
    pub decided_at: Option<i64>,
    /// Free-text notes from the human. `None` when they wrote none.
    pub notes: Option<String>,
}

const REVIEW_COLUMNS: &str = "id, run_id, task_id, plan_version_id, boundary, state, \
                              packet_hash, packet_path, decision, decided_by, decided_at, notes";

fn to_review_row(row: &rusqlite::Row<'_>) -> StoreResult<ReviewRow> {
    let state: String = row.get(5)?;
    let decision: Option<String> = row.get(8)?;
    Ok(ReviewRow {
        id: row.get(0)?,
        run_id: RunId::new(row.get::<_, String>(1)?)?,
        task_id: TaskId::new(row.get::<_, String>(2)?)?,
        plan_version_id: PlanVersionId::new(row.get::<_, String>(3)?)?,
        boundary: row.get(4)?,
        state: state.parse::<ReviewState>()?,
        packet_hash: row.get(6)?,
        packet_path: row.get(7)?,
        // Parsed, never defaulted: an unrecognised decision string is an error,
        // not the most permissive of §6.5's five. See the `From` impl in
        // `error.rs`.
        decision: decision.map(|d| d.parse::<ReviewDecision>()).transpose()?,
        decided_by: row.get(9)?,
        decided_at: row.get(10)?,
        notes: row.get(11)?,
    })
}

/// Append the `event` row for a review transition.
///
/// [`EventKind::ReviewStateChanged`] rather than
/// [`EventKind::RunStateChanged`]: a review's states are not run states, and one
/// kind covering two machines would force every reader to inspect the payload to
/// learn which machine moved. The payload still carries the review id and
/// `"entity":"review"`, and its keys are deliberately *not* `lease.rs`'s
/// `{"to","route","detail"}` shape, so the two families remain distinguishable
/// even to a reader who only greps.
///
/// The event is written inside the caller's transaction, so a transition that
/// commits without its evidence — or evidence for a transition that was refused —
/// is not a state the database can be in.
fn append_review_event(
    tx: &Transaction<'_>,
    run_id: &RunId,
    review_id: &str,
    from: ReviewState,
    to: ReviewState,
    extra: serde_json::Value,
    now_ms: i64,
) -> StoreResult<()> {
    let mut payload = serde_json::json!({
        "entity": "review",
        "review": review_id,
        "from": from.as_str(),
        "to": to.as_str(),
    });
    if let (Some(payload), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            payload.insert(key.clone(), value.clone());
        }
    }
    append_event(
        tx,
        run_id,
        EventKind::ReviewStateChanged,
        &payload.to_string(),
        now_ms,
    )?;
    Ok(())
}

/// Read one review inside an open transaction, for the read-check-write pairs.
///
/// The read and the write share the transaction so that the state the legality
/// check saw is the state the `UPDATE` writes over — the shape
/// [`crate::task::set_task_state`] uses.
fn review_in_tx(tx: &Transaction<'_>, id: &str) -> StoreResult<ReviewRow> {
    let sql = format!("SELECT {REVIEW_COLUMNS} FROM review WHERE id = ?1");
    let mut stmt = tx.prepare(&sql)?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => to_review_row(row),
        None => Err(StoreError::NoSuchReview(id.to_string())),
    }
}

/// Open a review in `PENDING` — the state §5.2 starts the machine in.
///
/// `PENDING` means "a boundary fired and nobody has looked yet", which before S13
/// was indistinguishable from "a human looked and paused" (§6.5's `pause`) because
/// neither was recorded anywhere.
///
/// **Plain `INSERT`, never `OR IGNORE`.** A second review claiming an existing id
/// is a mistake upstream, and silently returning the first one would give two
/// review boundaries a single identity — and therefore let a decision about one
/// be read as a decision about the other. [`crate::task::create_task`] refuses
/// duplicates for the same reason.
///
/// **One open review per run**, enforced by `ix_review_one_open_per_run` in the
/// schema and pre-checked here so the refusal is a named
/// [`StoreError::ReviewAlreadyOpen`] rather than a bare constraint violation. The
/// pre-check cannot be raced — `BEGIN IMMEDIATE` serialises writers — but it is
/// not the guarantee either: the index is, which is why it lives in the DDL where
/// a future second write path cannot forget it.
pub fn open(conn: &mut Connection, new: &NewReview, now_ms: i64) -> StoreResult<ReviewRow> {
    with_immediate(conn, |tx| {
        let already_open: Option<String> = tx
            .query_row(
                "SELECT id FROM review WHERE run_id = ?1 AND state <> 'DECIDED' LIMIT 1",
                params![new.run_id.as_str()],
                |row| row.get(0),
            )
            .ok();
        if let Some(open_review_id) = already_open {
            return Err(StoreError::ReviewAlreadyOpen {
                run_id: new.run_id.as_str().to_string(),
                open_review_id,
            });
        }

        tx.execute(
            "INSERT INTO review
               (id, run_id, task_id, plan_version_id, boundary, state, packet_hash,
                packet_path, decision, decided_by, decided_at, notes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'PENDING', NULL, NULL, NULL, NULL, NULL, NULL, ?6)",
            params![
                new.id,
                new.run_id.as_str(),
                new.task_id.as_str(),
                new.plan_version_id.as_str(),
                new.boundary,
                now_ms,
            ],
        )?;
        append_review_event(
            tx,
            &new.run_id,
            &new.id,
            // The machine has no state before `PENDING`; `PENDING → PENDING`
            // here reads as "opened at", and it is the one place in this module
            // that does not go through `transition_to` — because opening is a
            // row coming into existence, not an edge.
            ReviewState::Pending,
            ReviewState::Pending,
            serde_json::json!({ "opened": true, "boundary": new.boundary }),
            now_ms,
        )?;
        review_in_tx(tx, &new.id)
    })
}

/// `PENDING → EXPORTED`, recording the packet the human will read.
///
/// Requires `PENDING` twice over: [`ReviewState::transition_to`] refuses the edge
/// from anywhere else, and the `UPDATE`'s `WHERE … AND state = 'PENDING'` refuses
/// it again at the row. The second check is not redundant — it is the one that
/// holds when the state moved after the `SELECT`, and `changed == 0` is therefore
/// treated as the refusal rather than as a successful no-op.
///
/// **The hash and the state move together, in one statement.** An `EXPORTED`
/// review with no `packet_hash` is the row [`record_decision`] must never accept,
/// and writing the state first and the hash second would make that row a real,
/// reachable intermediate state rather than an impossible one.
///
/// A second export is refused, and that is the point: it would mint a second
/// packet hash for one review, so an imported decision could be bound to
/// whichever of the two suited whoever wrote it.
pub fn mark_exported(
    conn: &mut Connection,
    id: &str,
    packet_hash: &str,
    packet_path: &str,
) -> StoreResult<ReviewRow> {
    with_immediate(conn, |tx| {
        let current = review_in_tx(tx, id)?;
        current
            .state
            .transition_to(ReviewState::Exported)
            .map_err(|e| StoreError::IllegalReviewTransition(e.to_string()))?;

        let changed = tx.execute(
            "UPDATE review SET state = 'EXPORTED', packet_hash = ?2, packet_path = ?3
              WHERE id = ?1 AND state = 'PENDING'",
            params![id, packet_hash, packet_path],
        )?;
        if changed == 0 {
            return Err(StoreError::ReviewNotInState {
                review_id: id.to_string(),
                required: ReviewState::Pending,
            });
        }
        append_review_event(
            tx,
            &current.run_id,
            id,
            current.state,
            ReviewState::Exported,
            serde_json::json!({ "packet_hash": packet_hash, "packet_path": packet_path }),
            now_ms_of_export(),
        )?;
        review_in_tx(tx, id)
    })
}

/// The export event's timestamp.
///
/// [`mark_exported`] takes no `now_ms` — its signature is the packet's identity,
/// not a clock reading, and Part 5.1 gives `review` no `exported_at` column, so a
/// `now_ms` parameter would be an argument the row does not record. The `event`
/// row still needs a time, and `event.at` is the only place it goes.
///
/// A monotonic source was rejected: `event.at` is a wall-clock millisecond
/// timestamp everywhere else in this crate, and mixing two clocks in one column
/// makes the log unorderable. A clock that cannot be read yields `0`, which is
/// visibly wrong rather than plausibly recent — the same choice
/// [`crate::migrate`] makes for `applied_at`.
fn now_ms_of_export() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `EXPORTED → DECIDED`, recording §6.5's imported decision.
///
/// Two preconditions, and they are different guarantees:
///
/// * **state is `EXPORTED`** — checked by [`ReviewState::transition_to`] and again
///   by the `UPDATE`'s `WHERE`. A `PENDING` review has never been exported, so
///   there is nothing a human could have read; a `DECIDED` one has been answered,
///   and §6.5's mutating import must not be a place a second answer can arrive.
/// * **`packet_hash IS NOT NULL`** — checked here *and* in the `UPDATE`'s `WHERE`,
///   because this is the binding the whole review-authority story rests on. §4.3's
///   `REVIEW_ACCEPTANCE` authorizes a review packet; a decision recorded against a
///   review with no packet hash would authorize nothing in particular. Putting the
///   condition in the statement means it holds even for a row that reached
///   `EXPORTED` by a path this module does not offer.
///
/// The decision is a [`ReviewDecision`], not a `&str`, so an unrecognised word
/// cannot reach the column: §4.4's fail-closed rule applied to the most
/// typo-exposed value in the system, one of whose five spellings advances a task
/// to `COMPLETE`.
///
/// **This does not move the task.** `review.decision` records what a human
/// decided; `task.state` moves through [`crate::task::set_task_state`], which
/// checks §5.2's table. Writing both here would give S13 a private way to reach
/// `TaskState::Complete` that does not consult the machine — and a second, weaker
/// path to a column is how the guarantees the first one enforces stop being
/// guarantees.
pub fn record_decision(
    conn: &mut Connection,
    id: &str,
    decision: ReviewDecision,
    decided_by: &str,
    notes: Option<&str>,
    now_ms: i64,
) -> StoreResult<ReviewRow> {
    with_immediate(conn, |tx| {
        let current = review_in_tx(tx, id)?;
        current
            .state
            .transition_to(ReviewState::Decided)
            .map_err(|e| StoreError::IllegalReviewTransition(e.to_string()))?;
        if current.packet_hash.is_none() {
            return Err(StoreError::DecisionWithoutPacket {
                review_id: id.to_string(),
            });
        }

        let changed = tx.execute(
            "UPDATE review
                SET state = 'DECIDED', decision = ?2, decided_by = ?3,
                    decided_at = ?4, notes = ?5
              WHERE id = ?1 AND state = 'EXPORTED' AND packet_hash IS NOT NULL",
            params![id, decision.as_str(), decided_by, now_ms, notes],
        )?;
        if changed == 0 {
            return Err(StoreError::ReviewNotInState {
                review_id: id.to_string(),
                required: ReviewState::Exported,
            });
        }
        append_review_event(
            tx,
            &current.run_id,
            id,
            current.state,
            ReviewState::Decided,
            serde_json::json!({
                "decision": decision.as_str(),
                "decided_by": decided_by,
                // The hash the decision is bound to, in the evidence log, so an
                // audit can check that the human answered the packet that was
                // exported rather than some later one.
                "packet_hash": current.packet_hash,
            }),
            now_ms,
        )?;
        review_in_tx(tx, id)
    })
}

/// One review by id.
pub fn review(conn: &Connection, id: &str) -> StoreResult<Option<ReviewRow>> {
    let sql = format!("SELECT {REVIEW_COLUMNS} FROM review WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some(to_review_row(row)?)),
        None => Ok(None),
    }
}

/// The one open review of a run, if it has one.
///
/// "The one" is guaranteed by `ix_review_one_open_per_run`, exactly as
/// [`crate::run::active_run_for_task`]'s "the one" is guaranteed by
/// `ix_run_one_active_per_task`. Both the export path and the import path need
/// this: an operator names a *run*, and the review id is an implementation detail
/// of the boundary that fired.
///
/// The predicate is `state <> 'DECIDED'`, matching the index — including for a
/// state string this binary does not recognise, which counts as open and is
/// returned as a [`StoreError::Domain`] by the row conversion rather than being
/// silently skipped. An unknown state must not read as "nothing to review".
pub fn open_review_for_run(conn: &Connection, run_id: &RunId) -> StoreResult<Option<ReviewRow>> {
    let sql = format!(
        "SELECT {REVIEW_COLUMNS} FROM review
          WHERE run_id = ?1 AND state <> 'DECIDED' LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![run_id.as_str()])?;
    match rows.next()? {
        Some(row) => Ok(Some(to_review_row(row)?)),
        None => Ok(None),
    }
}

/// Every review of a run, oldest first.
///
/// Ordered by `created_at` then `id` so that `--json` output is stable between
/// runs and therefore diffable, the reason [`crate::task::tasks`] gives. A run
/// accumulates these: §6.5's `repair` sends work back, and the next boundary
/// opens a new review rather than reopening a decided one.
pub fn reviews_for_run(conn: &Connection, run_id: &RunId) -> StoreResult<Vec<ReviewRow>> {
    let sql =
        format!("SELECT {REVIEW_COLUMNS} FROM review WHERE run_id = ?1 ORDER BY created_at, id");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![run_id.as_str()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(to_review_row(row)?);
    }
    Ok(out)
}
