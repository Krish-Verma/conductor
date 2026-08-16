//! Persisting the attempt machine — §5.2, schema v2.
//!
//! Every function here takes a [`Fence`]: an attempt is a write about a run, and
//! §4.7 admits no unfenced writes. The typestate does the rest — the argument
//! types mean a `STALE` attempt cannot be handed an exit code and an `ACTIVE`
//! one cannot be recorded as reconciled.

use conductor_core::attempt::{Attempt, AttemptState, TerminalPhase, phase};
use conductor_core::{AttemptId, AttemptOutcome, EventKind, Fence, RunId};
use rusqlite::{Connection, Row, params};
use serde::Serialize;

use crate::error::StoreResult;
use crate::lease::{append_event, check_fence};
use crate::tx::with_immediate;

/// What must be known before an attempt row can exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttempt {
    /// `attempt.id`.
    pub id: AttemptId,
    /// `attempt.ordinal`, 1-based and unique within the run.
    pub ordinal: i64,
    /// `IMPLEMENT` | `REPAIR` | `CONTINUE`.
    pub kind: String,
    /// The adapter that will be used.
    pub adapter: String,
    /// `none` | `codex-sandbox` | `sandbox-exec`.
    pub launcher: String,
    /// The measured `ExecutionCapabilities` in force, as JSON (§4.2).
    pub caps_snapshot: String,
    /// The agent session id, when the adapter lets Conductor assign one.
    pub agent_session_id: Option<String>,
}

/// One `attempt` row as read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttemptRow {
    /// `attempt.id`.
    pub id: AttemptId,
    /// The run it belongs to.
    pub run_id: RunId,
    /// `attempt.ordinal`.
    pub ordinal: i64,
    /// `attempt.kind`.
    pub kind: String,
    /// `attempt.adapter`.
    pub adapter: String,
    /// `attempt.launcher`.
    pub launcher: String,
    /// `attempt.state` — the column schema v2 adds.
    pub state: AttemptState,
    /// `attempt.outcome`, once there is one.
    pub outcome: Option<AttemptOutcome>,
    /// The child's pid, when one was spawned.
    pub pid: Option<i32>,
    /// The child's start time, microseconds since the epoch.
    ///
    /// Recorded with the pid because §4.7 step 3 requires "alive **and**
    /// start-time matches": a recycled pid is not the same process.
    pub pid_start_time: Option<i64>,
    /// When the process was observed running.
    pub started_at: Option<i64>,
    /// When it stopped.
    pub ended_at: Option<i64>,
    /// The observed exit code, when one was observed.
    pub exit_code: Option<i32>,
    /// The fatal signal, when there was one.
    pub signal: Option<i32>,
    /// The agent session this attempt belongs to, when there is one.
    ///
    /// Either the session Conductor assigned before the run, or — for an
    /// adapter whose identity arrives on the wire, which §6.2 says Codex is —
    /// the one the agent announced and the run path recorded. §4.6's repair
    /// loop reads it to decide what a retry may resume.
    pub agent_session_id: Option<String>,
}

/// One `attempt` row exactly as SQLite hands it over, before any domain parsing.
///
/// A named struct rather than a fourteen-element tuple: the columns are read in
/// one place and interpreted in another, and a tuple that wide makes an
/// off-by-one between the two invisible.
struct RawAttempt {
    id: String,
    run_id: String,
    ordinal: i64,
    kind: String,
    adapter: String,
    launcher: String,
    state: String,
    outcome: Option<String>,
    pid: Option<i32>,
    pid_start_time: Option<i64>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    agent_session_id: Option<String>,
}

const SELECT_COLUMNS: &str = "id, run_id, ordinal, kind, adapter, launcher, state, outcome, \
                              pid, pid_start_time, started_at, ended_at, exit_code, signal, \
                              agent_session_id";

fn read_row(row: &Row<'_>) -> rusqlite::Result<RawAttempt> {
    Ok(RawAttempt {
        id: row.get(0)?,
        run_id: row.get(1)?,
        ordinal: row.get(2)?,
        kind: row.get(3)?,
        adapter: row.get(4)?,
        launcher: row.get(5)?,
        state: row.get(6)?,
        outcome: row.get(7)?,
        pid: row.get(8)?,
        pid_start_time: row.get(9)?,
        started_at: row.get(10)?,
        ended_at: row.get(11)?,
        exit_code: row.get(12)?,
        signal: row.get(13)?,
        agent_session_id: row.get(14)?,
    })
}

/// Record the session an agent announced for itself — S10.
///
/// §6.2: *"Session identity arrives in `thread.started`, so it cannot be
/// pre-assigned"*. For such an adapter this is the **only** way
/// `attempt.agent_session_id` is ever populated, and §4.6's
/// `previous_session` reads that column to decide what a retry may resume — so
/// without this, `resume` is unreachable for every adapter of that kind.
///
/// **The announcement wins.** If Conductor assigned a session and the agent
/// announced a different one, the announced id is the session that actually
/// exists — the assignment was a request, and only the agent knows what it
/// created. Recording the request over the reality would send a resume at a
/// session id no agent ever had.
///
/// **It never clears**, and not because of a `COALESCE`: an earlier version had
/// one, and a mutation that removed it failed no test — because the caller only
/// invokes this when there *is* an announcement, so the clearing case is
/// unreachable by construction. A defence whose removal breaks nothing is a
/// defence against nothing, and leaving it in would have left a doc comment
/// claiming a guarantee the code was not providing.
///
/// Fenced like every other attempt write: a worker that lost its lease does not
/// get to relabel the successor's attempt.
pub fn record_agent_session(
    conn: &mut Connection,
    fence: &Fence,
    attempt_id: &AttemptId,
    session_id: &str,
) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "UPDATE attempt SET agent_session_id = ?2
              WHERE id = ?1",
            params![attempt_id.as_str(), session_id],
        )?;
        Ok(())
    })
}

fn to_attempt_row(raw: RawAttempt) -> StoreResult<AttemptRow> {
    let outcome = match raw.outcome {
        Some(text) => Some(text.parse::<AttemptOutcome>()?),
        None => None,
    };
    Ok(AttemptRow {
        id: AttemptId::new(raw.id)?,
        run_id: RunId::new(raw.run_id)?,
        ordinal: raw.ordinal,
        kind: raw.kind,
        adapter: raw.adapter,
        launcher: raw.launcher,
        state: raw.state.parse::<AttemptState>()?,
        outcome,
        pid: raw.pid,
        pid_start_time: raw.pid_start_time,
        started_at: raw.started_at,
        ended_at: raw.ended_at,
        exit_code: raw.exit_code,
        signal: raw.signal,
        agent_session_id: raw.agent_session_id,
    })
}

/// Open an attempt in `CREATED`.
///
/// `UNIQUE(run_id, ordinal)` is what refuses a duplicate attempt: two
/// supervisors that both believe they own the run cannot both open ordinal 1,
/// and the second one finds out at INSERT time.
pub fn create_attempt(
    conn: &mut Connection,
    fence: &Fence,
    new: NewAttempt,
    now_ms: i64,
) -> StoreResult<Attempt<phase::Created>> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "INSERT INTO attempt
               (id, run_id, ordinal, kind, adapter, launcher, caps_snapshot,
                agent_session_id, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                new.id.as_str(),
                fence.run_id().as_str(),
                new.ordinal,
                new.kind,
                new.adapter,
                new.launcher,
                new.caps_snapshot,
                new.agent_session_id,
                AttemptState::Created.as_str(),
            ],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::AttemptCreated,
            &format!("{{\"attempt\":\"{}\",\"ordinal\":{}}}", new.id, new.ordinal),
            now_ms,
        )?;
        Ok(Attempt::create(
            new.id.clone(),
            fence.run_id().clone(),
            new.ordinal,
        ))
    })
}

/// `CREATED → STARTING`.
pub fn record_attempt_starting(
    conn: &mut Connection,
    fence: &Fence,
    attempt: &Attempt<phase::Starting>,
    now_ms: i64,
) -> StoreResult<()> {
    set_state(conn, fence, attempt.id(), AttemptState::Starting, now_ms)
}

/// `STARTING → ACTIVE`, recording the process identity recovery will probe.
///
/// A start time the supervisor could not read is written as `NULL`. §4.7 step 3
/// asks "alive **and** start-time matches?", and a column that cannot say "I do
/// not know" forces the answer to be a lie in one direction or the other — the
/// direction a sentinel picks is "adopt whatever holds the pid".
pub fn record_attempt_active(
    conn: &mut Connection,
    fence: &Fence,
    attempt: &Attempt<phase::Active>,
    now_ms: i64,
) -> StoreResult<()> {
    let spawn = attempt.spawn();
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "UPDATE attempt
                SET state = ?2, pid = ?3, pid_start_time = ?4, started_at = ?5
              WHERE id = ?1",
            params![
                attempt.id().as_str(),
                AttemptState::Active.as_str(),
                spawn.pid,
                spawn.pid_start_time,
                now_ms,
            ],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::AttemptStarted,
            &format!(
                "{{\"attempt\":\"{}\",\"pid\":{},\"pid_start_time\":{}}}",
                attempt.id(),
                spawn.pid,
                json_opt_i64(spawn.pid_start_time)
            ),
            now_ms,
        )?;
        Ok(())
    })
}

/// Record a terminal outcome: `EXITED`, `CRASHED`, `TIMED_OUT` or `STALE`.
///
/// The exit code and signal come from the typestate, so a `STALE` attempt writes
/// `NULL` into both — not because this function remembers to, but because a
/// `STALE` attempt has nothing else to give it.
pub fn record_attempt_terminal(
    conn: &mut Connection,
    fence: &Fence,
    attempt: &TerminalPhase,
    now_ms: i64,
) -> StoreResult<()> {
    let spawn = attempt.spawn();
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "UPDATE attempt
                SET state = ?2, outcome = ?3, exit_code = ?4, signal = ?5, ended_at = ?6,
                    pid = COALESCE(pid, ?7), pid_start_time = COALESCE(pid_start_time, ?8)
              WHERE id = ?1",
            params![
                attempt.id().as_str(),
                attempt.state().as_str(),
                attempt.outcome().as_str(),
                attempt.exit_code(),
                attempt.signal(),
                now_ms,
                spawn.map(|s| s.pid),
                // `and_then`, not `map`: an attempt that had a process but no
                // readable start time contributes a pid and nothing else. `map`
                // would hand SQLite an `Option<Option<i64>>`; flattening it here
                // keeps "no spawn" and "no identity" writing the same `NULL`,
                // which is right — `COALESCE` must not overwrite a recorded
                // start time either way.
                spawn.and_then(|s| s.pid_start_time),
            ],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::AttemptFinished,
            &format!(
                "{{\"attempt\":\"{}\",\"state\":\"{}\",\"outcome\":\"{}\",\
                  \"exit_code\":{},\"signal\":{},\"timeout_reason\":{}}}",
                attempt.id(),
                attempt.state().as_str(),
                attempt.outcome().as_str(),
                json_opt_i32(attempt.exit_code()),
                json_opt_i32(attempt.signal()),
                match attempt.timeout_reason() {
                    Some(r) => format!("\"{r}\""),
                    None => "null".to_string(),
                }
            ),
            now_ms,
        )?;
        Ok(())
    })
}

/// The last transition: Conductor has looked at the repository.
///
/// `attempt.outcome` is deliberately **not** overwritten with `RECONCILED`.
/// Schema v1 had to, because `outcome` was the only column; with `state`
/// present, keeping the classification is free and losing it would destroy the
/// one fact reconciliation exists to act on.
pub fn record_attempt_reconciled(
    conn: &mut Connection,
    fence: &Fence,
    attempt: &Attempt<phase::Reconciled>,
    now_ms: i64,
) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "UPDATE attempt SET state = ?2 WHERE id = ?1",
            params![attempt.id().as_str(), AttemptState::Reconciled.as_str()],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::AttemptReconciled,
            &format!(
                "{{\"attempt\":\"{}\",\"outcome\":\"{}\"}}",
                attempt.id(),
                attempt.outcome()
            ),
            now_ms,
        )?;
        Ok(())
    })
}

fn set_state(
    conn: &mut Connection,
    fence: &Fence,
    id: &AttemptId,
    state: AttemptState,
    now_ms: i64,
) -> StoreResult<()> {
    with_immediate(conn, |tx| {
        check_fence(tx, fence)?;
        tx.execute(
            "UPDATE attempt SET state = ?2 WHERE id = ?1",
            params![id.as_str(), state.as_str()],
        )?;
        append_event(
            tx,
            fence.run_id(),
            EventKind::AttemptCreated,
            &format!("{{\"attempt\":\"{id}\",\"state\":\"{}\"}}", state.as_str()),
            now_ms,
        )?;
        Ok(())
    })
}

/// Every attempt that was in flight when somebody last wrote to it.
///
/// §4.7 step 3 reads exactly this: an attempt in `CREATED`, `STARTING` or
/// `ACTIVE` had a supervisor that is no longer there.
pub fn in_flight_attempts(conn: &Connection) -> StoreResult<Vec<AttemptRow>> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS} FROM attempt
          WHERE state IN ('CREATED','STARTING','ACTIVE')
          ORDER BY run_id, ordinal"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map([], read_row)?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;
    raws.into_iter().map(to_attempt_row).collect()
}

/// Every attempt of one run, oldest first.
pub fn attempts_for_run(conn: &Connection, run_id: &RunId) -> StoreResult<Vec<AttemptRow>> {
    let sql = format!("SELECT {SELECT_COLUMNS} FROM attempt WHERE run_id = ?1 ORDER BY ordinal");
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![run_id.as_str()], read_row)?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;
    raws.into_iter().map(to_attempt_row).collect()
}

/// The next free ordinal for a run.
pub fn next_ordinal(conn: &Connection, run_id: &RunId) -> StoreResult<i64> {
    let ordinal: i64 = conn.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM attempt WHERE run_id = ?1",
        params![run_id.as_str()],
        |row| row.get(0),
    )?;
    Ok(ordinal)
}

fn json_opt_i32(value: Option<i32>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "null".to_string(),
    }
}

/// `null`, not `0`, for a number the system does not have.
///
/// The event log is read by humans reconstructing what happened. A `0` start
/// time in an `AttemptStarted` event would read as a process that began at the
/// Unix epoch; `null` reads as what it is — the identity was never established.
fn json_opt_i64(value: Option<i64>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "null".to_string(),
    }
}
