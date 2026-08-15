//! The durable half of §4.3: `approval_request` and `approval_grant` rows.
//!
//! # Two state machines, not one chain
//!
//! §5.2 draws one diagram, and it is two machines sharing a label:
//!
//! ```text
//! approval_request : REQUESTED | GRANTED | DENIED | EXPIRED
//! approval_grant   : GRANTED   | CONSUMED | EXPIRED | REVOKED
//! ```
//!
//! A request is never `CONSUMED`; a grant is never `DENIED`. `GRANTED` is the
//! **join point** — the grant row carries `request_id` — and not a transition
//! from one machine into the other. [`RequestState`] and [`GrantState`] are
//! separate types precisely so that a caller cannot write one machine's state
//! into the other's column.
//!
//! # Why the writes here are unfenced
//!
//! Every write *about a run* is fenced (§4.7), and these are not: a grant is
//! made by a **human at the control socket**, in a process that holds no lease
//! on the run. Requiring a fence would mean either handing the socket a lease it
//! has no business holding, or refusing approvals whenever the worker died —
//! and §4.7 step 9 already establishes the opposite rule for this table
//! ("unfenced by design: an approval TTL is a property of the request, not of
//! any worker's lease"). Safety comes from `BEGIN IMMEDIATE` plus **conditional
//! updates**: every state change names the state it is leaving, so two racing
//! writers cannot both win. [`super::revoke`] takes a fence, because the
//! *findings* and *run-state changes* it produces are statements about a run.
//!
//! # What this module does not do
//!
//! It does not decide anything. Whether a grant applies to an operation is
//! [`super::authorize`]'s question, because answering it requires recomputing
//! the binding from a live policy decision. Same split as `repair`: rows here,
//! meaning there.

use conductor_core::RunId;
use conductor_store::with_immediate;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::binding::{Binding, BindingHash};
use super::kind::{ApprovalKind, Expiry, ExpiryRule, Subject};
use crate::policy::model::{Fact, FactSet, FactSource, Scope};

/// Why an approval write was refused.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    /// SQLite said no.
    #[error("approval store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The store layer said no.
    #[error("approval store: {0}")]
    Store(#[from] conductor_store::StoreError),
    /// A JSON column would not encode or decode.
    #[error("approval store: {0}")]
    Json(#[from] serde_json::Error),
    /// §4.3's fourth column was not respected.
    #[error(
        "a {kind} {requirement}, and this request {actual} \
         (§4.3: expiry is a property of the kind, not a free choice)"
    )]
    Expiry {
        /// Which kind.
        kind: ApprovalKind,
        /// What §4.3 requires of it.
        requirement: &'static str,
        /// What was asked for.
        actual: &'static str,
    },
    /// The request is not there.
    #[error("no approval request {id}")]
    NoSuchRequest {
        /// The id asked for.
        id: String,
    },
    /// The request has already left `REQUESTED`. §5.2 draws no way back.
    #[error("approval request {id} is {state}, not REQUESTED; §5.2 draws no way back")]
    NotRequested {
        /// The id asked for.
        id: String,
        /// Where it actually is.
        state: RequestState,
    },
    /// The grant is not there.
    #[error("no approval grant {id}")]
    NoSuchGrant {
        /// The id asked for.
        id: String,
    },
    /// A stored row cannot be interpreted.
    #[error("approval row {id} is unreadable: {detail}")]
    Unreadable {
        /// Which row.
        id: String,
        /// What is wrong with it.
        detail: String,
    },
}

/// Result alias for this module.
pub type ApprovalResult<T> = Result<T, ApprovalError>;

// ---------------------------------------------------------------------------
// the two state machines
// ---------------------------------------------------------------------------

/// `approval_request.state` — §5.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RequestState {
    /// Waiting for a human.
    Requested,
    /// A human granted it. The grant row is the authorization; this is only the
    /// record that the question was answered.
    Granted,
    /// A human refused it.
    Denied,
    /// Its TTL passed with no answer.
    Expired,
}

impl RequestState {
    /// Every state a request can be in. **Four** — a request is never
    /// `CONSUMED` and never `REVOKED`; those belong to the grant.
    pub const ALL: &'static [RequestState] = &[
        RequestState::Requested,
        RequestState::Granted,
        RequestState::Denied,
        RequestState::Expired,
    ];

    /// The exact stored string.
    pub fn as_str(&self) -> &'static str {
        match self {
            RequestState::Requested => "REQUESTED",
            RequestState::Granted => "GRANTED",
            RequestState::Denied => "DENIED",
            RequestState::Expired => "EXPIRED",
        }
    }

    /// Parse a stored state. `None` for anything outside this machine —
    /// including the grant machine's states, which is the point.
    pub fn parse(text: &str) -> Option<RequestState> {
        RequestState::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == text)
    }
}

impl std::fmt::Display for RequestState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `approval_grant.state` — §5.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GrantState {
    /// Live. May authorize an operation whose binding matches.
    Granted,
    /// Used. Terminal (§5.2), which is what makes "no grant consumed twice"
    /// a property of the row rather than of the caller's care.
    Consumed,
    /// Its TTL passed.
    Expired,
    /// A human took it back (§4.3's revocation table).
    Revoked,
}

impl GrantState {
    /// Every state a grant can be in. **Four** — a grant is never `DENIED` and
    /// never `REQUESTED`; those belong to the request.
    pub const ALL: &'static [GrantState] = &[
        GrantState::Granted,
        GrantState::Consumed,
        GrantState::Expired,
        GrantState::Revoked,
    ];

    /// The exact stored string.
    pub fn as_str(&self) -> &'static str {
        match self {
            GrantState::Granted => "GRANTED",
            GrantState::Consumed => "CONSUMED",
            GrantState::Expired => "EXPIRED",
            GrantState::Revoked => "REVOKED",
        }
    }

    /// Parse a stored state. `None` for anything outside this machine.
    pub fn parse(text: &str) -> Option<GrantState> {
        GrantState::ALL
            .iter()
            .copied()
            .find(|state| state.as_str() == text)
    }
}

impl std::fmt::Display for GrantState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// rows
// ---------------------------------------------------------------------------

/// §4.3's `approval_request`, as it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApprovalRequest {
    /// `AR-0031`.
    pub id: String,
    /// What is being authorized. Carries the kind (§4.3's anti-collapse rule).
    pub subject: Subject,
    /// The run, when there is one. §4.3's plan approval and review acceptance
    /// are not run-scoped (schema v6).
    pub run_id: Option<RunId>,
    /// The facts, with their sources.
    pub facts: FactSet,
    /// The snapshot the decision was made against.
    pub policy_hash: String,
    /// Every rule §4.4 matched.
    pub matched_rules: Vec<String>,
    /// Why a human is being asked.
    pub explanation: String,
    /// A pointer to the evidence artifact, when there is one.
    pub evidence_ref: Option<String>,
    /// §4.3's fourth column, checked against the kind at write time.
    pub expires: Expiry,
}

/// One `approval_request` row as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequestRow {
    /// `approval_request.id`.
    pub id: String,
    /// Which of §4.3's four.
    pub kind: ApprovalKind,
    /// What is authorized.
    pub subject: Subject,
    /// The run, when there is one.
    pub run_id: Option<RunId>,
    /// The facts, with their sources.
    pub facts: FactSet,
    /// Summary of the fact sources — see [`summarize_sources`].
    pub facts_source: FactSource,
    /// The snapshot the decision was made against.
    pub policy_hash: String,
    /// Every rule §4.4 matched.
    pub matched_rules: Vec<String>,
    /// Why a human is being asked.
    pub explanation: String,
    /// A pointer to the evidence artifact.
    pub evidence_ref: Option<String>,
    /// Where in §5.2's request machine it is.
    pub state: RequestState,
    /// When it was raised.
    pub requested_at: i64,
    /// Its TTL, or its deliberate absence.
    pub expires: Expiry,
}

/// §4.3's `approval_grant`, as it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantOptions {
    /// `AG-0019`.
    pub id: String,
    /// §4.3's `scope`, e.g. `{run: r-0041}`.
    pub scope: Scope,
    /// §4.3's `reuse`. **Default false** — one-shot unless a human said
    /// otherwise, because a grant that silently persisted would authorize
    /// operations nobody was asked about.
    pub reuse: bool,
    /// Its TTL, checked against the kind.
    pub expires: Expiry,
    /// Who granted it.
    pub granted_by: String,
    /// How they reached Conductor.
    pub channel: String,
    /// §4.3 tier B: `blake3(nonce)` and never the nonce, "so reading
    /// `conductor.db` does not yield it".
    pub nonce_hash: Option<String>,
}

/// One `approval_grant` row as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalGrantRow {
    /// `approval_grant.id`.
    pub id: String,
    /// The request it answers. §5.2's join point.
    pub request_id: String,
    /// The binding **as stored**. Evidence for a human; never an input to a
    /// decision — [`super::authorize`] recomputes and compares.
    pub stored_binding: BindingHash,
    /// §4.3's `scope`.
    pub scope: Scope,
    /// Whether it survives consumption.
    pub reuse: bool,
    /// Where in §5.2's grant machine it is.
    pub state: GrantState,
    /// `blake3(nonce)`, tier B only.
    pub nonce_hash: Option<String>,
    /// How the human reached Conductor.
    pub channel: String,
    /// Who granted it.
    pub granted_by: String,
    /// When.
    pub granted_at: i64,
    /// Its TTL, or its deliberate absence.
    pub expires: Expiry,
    /// When it left `GRANTED`.
    pub resolved_at: Option<i64>,
}

/// What [`expire`] swept.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Expired {
    /// Requests moved `REQUESTED → EXPIRED`.
    pub requests: Vec<String>,
    /// Grants moved `GRANTED → EXPIRED`.
    pub grants: Vec<String>,
}

// ---------------------------------------------------------------------------
// writes
// ---------------------------------------------------------------------------

/// Record one approval request — §4.4's `require_approval`, made durable.
///
/// Refuses when §4.3's expiry rule for the kind is not met. That check is here,
/// at the write, rather than at the socket: a request created by any path at
/// all must satisfy it, and a validation that lives in one caller is a
/// validation the next caller forgets.
pub fn request(
    conn: &mut Connection,
    request: &NewApprovalRequest,
    now_ms: i64,
) -> ApprovalResult<ApprovalRequestRow> {
    let kind = request.subject.kind();
    check_expiry(kind, request.expires)?;

    let facts = encode_facts(&request.facts)?;
    let matched = serde_json::to_string(&request.matched_rules)?;
    with_immediate(conn, |tx| {
        tx.execute(
            "INSERT INTO approval_request
               (id, kind, subject, run_id, action, facts, facts_source, policy_hash,
                matched_rules, explanation, evidence_ref, state, requested_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                request.id,
                kind.as_str(),
                request.subject.subject_column(),
                request.run_id.as_ref().map(|id| id.as_str()),
                request.subject.action_column(),
                facts,
                summarize_sources(&request.facts).as_str(),
                request.policy_hash,
                matched,
                request.explanation,
                request.evidence_ref,
                RequestState::Requested.as_str(),
                now_ms,
                request.expires.as_millis(),
            ],
        )?;
        Ok(())
    })?;
    request_row(conn, &request.id)?.ok_or(ApprovalError::NoSuchRequest {
        id: request.id.clone(),
    })
}

/// Grant a request — §5.2's `REQUESTED → GRANTED`, plus the grant row.
///
/// One transaction, because a request marked `GRANTED` with no grant row is an
/// approval a human made that nothing can use, and a grant row whose request is
/// still `REQUESTED` is an authorization nobody was asked for.
///
/// The `UPDATE` names the state it is leaving, so a second grant of the same
/// request — two operators, one socket, or a retry — cannot succeed twice.
pub fn grant(
    conn: &mut Connection,
    request_id: &str,
    options: &GrantOptions,
    now_ms: i64,
) -> ApprovalResult<ApprovalGrantRow> {
    let row = request_row(conn, request_id)?.ok_or_else(|| ApprovalError::NoSuchRequest {
        id: request_id.to_string(),
    })?;
    check_expiry(row.kind, options.expires)?;

    let binding = Binding {
        subject: row.subject.clone(),
        facts: row.facts.clone(),
        policy_hash: row.policy_hash.clone(),
        scope: options.scope.clone(),
    }
    .hash();

    // `None` means the request moved and the grant was written; `Some(state)`
    // means it was already out of `REQUESTED` and nothing was written. Reported
    // as a value rather than as an error so the transaction commits a no-op
    // instead of rolling back a failure that is not one.
    let refused: Option<String> = with_immediate(conn, |tx| {
        let moved = tx.execute(
            "UPDATE approval_request SET state = ?2 WHERE id = ?1 AND state = ?3",
            params![
                request_id,
                RequestState::Granted.as_str(),
                RequestState::Requested.as_str(),
            ],
        )?;
        if moved != 1 {
            // Read the state back inside the transaction, so the error names
            // where the request actually is rather than guessing.
            let state: String = tx.query_row(
                "SELECT state FROM approval_request WHERE id = ?1",
                params![request_id],
                |row| row.get(0),
            )?;
            return Ok(Some(state));
        }
        tx.execute(
            "INSERT INTO approval_grant
               (id, request_id, binding_hash, scope, reuse, state, nonce_hash, channel,
                granted_by, granted_at, expires_at, resolved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)",
            params![
                options.id,
                request_id,
                binding.as_str(),
                encode_scope(&options.scope),
                i64::from(options.reuse),
                GrantState::Granted.as_str(),
                options.nonce_hash,
                options.channel,
                options.granted_by,
                now_ms,
                options.expires.as_millis(),
            ],
        )?;
        Ok(None)
    })?;
    if let Some(state) = refused {
        return Err(match RequestState::parse(&state) {
            Some(state) => ApprovalError::NotRequested {
                id: request_id.to_string(),
                state,
            },
            None => ApprovalError::Unreadable {
                id: request_id.to_string(),
                detail: format!("request state {state:?} is not one of §5.2's four"),
            },
        });
    }

    grant_row(conn, &options.id)?.ok_or(ApprovalError::NoSuchGrant {
        id: options.id.clone(),
    })
}

/// Refuse a request — §5.2's `REQUESTED → DENIED`. No grant row is produced.
pub fn deny(
    conn: &mut Connection,
    request_id: &str,
    reason: &str,
    now_ms: i64,
) -> ApprovalResult<ApprovalRequestRow> {
    let _ = (reason, now_ms);
    let row = request_row(conn, request_id)?.ok_or_else(|| ApprovalError::NoSuchRequest {
        id: request_id.to_string(),
    })?;
    if row.state != RequestState::Requested {
        return Err(ApprovalError::NotRequested {
            id: request_id.to_string(),
            state: row.state,
        });
    }
    with_immediate(conn, |tx| {
        tx.execute(
            "UPDATE approval_request SET state = ?2 WHERE id = ?1 AND state = ?3",
            params![
                request_id,
                RequestState::Denied.as_str(),
                RequestState::Requested.as_str(),
            ],
        )?;
        Ok(())
    })?;
    request_row(conn, request_id)?.ok_or(ApprovalError::NoSuchRequest {
        id: request_id.to_string(),
    })
}

/// Sweep both machines onto `EXPIRED` — §4.7 step 9, extended to grants.
///
/// A `NULL` expiry never expires, which is how §4.3's plan approval and review
/// acceptance survive the sweep. SQLite gives that for free: `NULL <= now` is
/// `NULL`, so the row is not selected.
pub fn expire(conn: &mut Connection, now_ms: i64) -> ApprovalResult<Expired> {
    let swept = with_immediate(conn, |tx| {
        let requests = {
            let mut stmt = tx.prepare(
                "SELECT id FROM approval_request
                  WHERE state = 'REQUESTED' AND expires_at IS NOT NULL AND expires_at <= ?1
                  ORDER BY id",
            )?;
            stmt.query_map(params![now_ms], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<String>, rusqlite::Error>>()?
        };
        let grants = {
            let mut stmt = tx.prepare(
                "SELECT id FROM approval_grant
                  WHERE state = 'GRANTED' AND expires_at IS NOT NULL AND expires_at <= ?1
                  ORDER BY id",
            )?;
            stmt.query_map(params![now_ms], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<String>, rusqlite::Error>>()?
        };
        if !requests.is_empty() {
            tx.execute(
                "UPDATE approval_request SET state = 'EXPIRED'
                  WHERE state = 'REQUESTED' AND expires_at IS NOT NULL AND expires_at <= ?1",
                params![now_ms],
            )?;
        }
        if !grants.is_empty() {
            tx.execute(
                "UPDATE approval_grant SET state = 'EXPIRED', resolved_at = ?1
                  WHERE state = 'GRANTED' AND expires_at IS NOT NULL AND expires_at <= ?1",
                params![now_ms],
            )?;
        }
        Ok(Expired { requests, grants })
    })?;
    Ok(swept)
}

/// What consuming a grant produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consumption {
    /// One-shot: the grant is now `CONSUMED` and will never authorize again.
    Consumed {
        /// Which grant.
        grant_id: String,
    },
    /// §4.3's `reuse: true`: the grant stays `GRANTED` until its TTL.
    Reusable {
        /// Which grant.
        grant_id: String,
    },
    /// It could not be consumed, and the effect must not happen.
    Refused(super::Refusal),
}

/// Consume a grant **immediately before the side effect** (§4.3).
///
/// The binding is re-checked here as well as at [`super::authorize`] time. Not
/// belt and braces: `authorize` answers "may this happen?", and between that
/// answer and the effect the process may have been killed and restarted, the
/// TTL may have passed, and a human may have revoked. The check that matters is
/// the one adjacent to the effect.
///
/// **No grant is consumed twice.** The `UPDATE` names `state = 'GRANTED'`, so
/// the second caller's update affects zero rows and is refused — inside
/// `BEGIN IMMEDIATE`, so two processes cannot both see `GRANTED`.
pub fn consume(
    conn: &mut Connection,
    grant_id: &str,
    binding: &BindingHash,
    now_ms: i64,
) -> ApprovalResult<Consumption> {
    let row = grant_row(conn, grant_id)?.ok_or_else(|| ApprovalError::NoSuchGrant {
        id: grant_id.to_string(),
    })?;
    if let Some(refusal) = refuse_unusable(&row, now_ms) {
        return Ok(Consumption::Refused(refusal));
    }
    if row.stored_binding != *binding {
        return Ok(Consumption::Refused(super::Refusal::NoMatchingGrant {
            binding: binding.clone(),
        }));
    }
    if row.reuse {
        return Ok(Consumption::Reusable {
            grant_id: grant_id.to_string(),
        });
    }
    let moved = with_immediate(conn, |tx| {
        Ok(tx.execute(
            "UPDATE approval_grant SET state = ?2, resolved_at = ?3
              WHERE id = ?1 AND state = ?4",
            params![
                grant_id,
                GrantState::Consumed.as_str(),
                now_ms,
                GrantState::Granted.as_str(),
            ],
        )?)
    })?;
    if moved == 1 {
        Ok(Consumption::Consumed {
            grant_id: grant_id.to_string(),
        })
    } else {
        // Someone else won the race between the read and the update.
        Ok(Consumption::Refused(super::Refusal::AlreadyConsumed {
            grant_id: grant_id.to_string(),
        }))
    }
}

/// Move a grant to a terminal state, naming the state it must be leaving.
///
/// Returns whether the row moved. Used by [`super::revoke`]; not a general
/// setter, which is why it takes both states.
pub(crate) fn transition_grant(
    conn: &mut Connection,
    grant_id: &str,
    from: GrantState,
    to: GrantState,
    now_ms: i64,
) -> ApprovalResult<bool> {
    let moved = with_immediate(conn, |tx| {
        Ok(tx.execute(
            "UPDATE approval_grant SET state = ?2, resolved_at = ?3 WHERE id = ?1 AND state = ?4",
            params![grant_id, to.as_str(), now_ms, from.as_str()],
        )?)
    })?;
    Ok(moved == 1)
}

// ---------------------------------------------------------------------------
// reads
// ---------------------------------------------------------------------------

/// One request by id.
pub fn request_row(conn: &Connection, id: &str) -> ApprovalResult<Option<ApprovalRequestRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, subject, run_id, action, facts, facts_source, policy_hash,
                matched_rules, explanation, evidence_ref, state, requested_at, expires_at
           FROM approval_request WHERE id = ?1",
    )?;
    let row = stmt
        .query_row(params![id], |row| Ok(read_request(row)))
        .optional()?;
    row.transpose()
}

/// One grant by id.
pub fn grant_row(conn: &Connection, id: &str) -> ApprovalResult<Option<ApprovalGrantRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, request_id, binding_hash, scope, reuse, state, nonce_hash, channel,
                granted_by, granted_at, expires_at, resolved_at
           FROM approval_grant WHERE id = ?1",
    )?;
    let row = stmt
        .query_row(params![id], |row| Ok(read_grant(row)))
        .optional()?;
    row.transpose()
}

/// Every request still waiting, oldest first — what the socket's `list` shows
/// and what §4.7 step 9 restores.
pub fn pending_requests(conn: &Connection) -> ApprovalResult<Vec<ApprovalRequestRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, subject, run_id, action, facts, facts_source, policy_hash,
                matched_rules, explanation, evidence_ref, state, requested_at, expires_at
           FROM approval_request WHERE state = 'REQUESTED' ORDER BY requested_at, id",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(read_request(row)?);
    }
    Ok(out)
}

/// Every live grant whose **stored** binding equals this one, oldest first.
///
/// A *candidate* list, not an answer: the caller still checks the state, the
/// TTL and the kind. The index `ix_grant_binding(binding_hash, state)` exists
/// for this query.
pub fn live_grants_for_binding(
    conn: &Connection,
    binding: &BindingHash,
) -> ApprovalResult<Vec<ApprovalGrantRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, request_id, binding_hash, scope, reuse, state, nonce_hash, channel,
                granted_by, granted_at, expires_at, resolved_at
           FROM approval_grant WHERE binding_hash = ?1 AND state = 'GRANTED'
          ORDER BY granted_at, id",
    )?;
    let mut rows = stmt.query(params![binding.as_str()])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(read_grant(row)?);
    }
    Ok(out)
}

/// The run a grant is about, via its request. `None` for the kinds that have no
/// run.
pub fn run_for_grant(conn: &Connection, grant_id: &str) -> ApprovalResult<Option<RunId>> {
    let stored: Option<Option<String>> = conn
        .query_row(
            "SELECT r.run_id FROM approval_grant g
               JOIN approval_request r ON r.id = g.request_id
              WHERE g.id = ?1",
            params![grant_id],
            |row| row.get(0),
        )
        .optional()?;
    match stored.flatten() {
        Some(id) => Ok(Some(RunId::new(id).map_err(|e| {
            ApprovalError::Unreadable {
                id: grant_id.to_string(),
                detail: e.to_string(),
            }
        })?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The reason a grant row cannot be used, if there is one. Shared by
/// [`consume`] and [`super::authorize`] so the two cannot disagree about what
/// "usable" means.
pub(crate) fn refuse_unusable(row: &ApprovalGrantRow, now_ms: i64) -> Option<super::Refusal> {
    match row.state {
        GrantState::Consumed => Some(super::Refusal::AlreadyConsumed {
            grant_id: row.id.clone(),
        }),
        GrantState::Revoked => Some(super::Refusal::Revoked {
            grant_id: row.id.clone(),
        }),
        GrantState::Expired => Some(super::Refusal::Expired {
            expires_at_ms: row.expires.as_millis().unwrap_or(now_ms),
            now_ms,
        }),
        GrantState::Granted if row.expires.is_expired(now_ms) => Some(super::Refusal::Expired {
            expires_at_ms: row.expires.as_millis().unwrap_or_default(),
            now_ms,
        }),
        GrantState::Granted => None,
    }
}

fn check_expiry(kind: ApprovalKind, expires: Expiry) -> ApprovalResult<()> {
    if expires.satisfies(kind.expiry_rule()) {
        return Ok(());
    }
    Err(match kind.expiry_rule() {
        ExpiryRule::Forbidden => ApprovalError::Expiry {
            kind,
            requirement: "does not expire",
            actual: "carries a TTL",
        },
        ExpiryRule::Mandatory => ApprovalError::Expiry {
            kind,
            requirement: "must expire",
            actual: "carries none",
        },
    })
}

/// The single `facts_source` §4.3's request shape carries.
///
/// A summary, and **only** a summary: `facts` holds each fact's own source and
/// is what the binding and the deny cap read. This column answers "was any of
/// this a model talking?" at a glance, so it reports the weakest source
/// present — the one that would cap a `deny` (ADR-0010).
pub fn summarize_sources(facts: &FactSet) -> FactSource {
    facts
        .iter()
        .map(|fact| fact.source)
        .find(|source| !source.may_carry_a_deny())
        .unwrap_or(FactSource::Deterministic)
}

/// Facts as stored: the whole fact, not just key and value, because the source
/// is load-bearing for the binding and for §4.4's cap.
#[derive(Serialize, serde::Deserialize)]
struct StoredFact {
    key: String,
    value: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
}

fn encode_facts(facts: &FactSet) -> ApprovalResult<String> {
    let stored: Vec<StoredFact> = facts
        .iter()
        .map(|fact| StoredFact {
            key: fact.key.clone(),
            value: fact.value.clone(),
            source: fact.source.as_str().to_string(),
            evidence: fact.evidence.clone(),
        })
        .collect();
    Ok(serde_json::to_string(&stored)?)
}

fn decode_facts(id: &str, encoded: &str) -> ApprovalResult<FactSet> {
    let stored: Vec<StoredFact> = serde_json::from_str(encoded)?;
    stored
        .into_iter()
        .map(|fact| {
            let source = match fact.source.as_str() {
                "deterministic" => FactSource::Deterministic,
                "model_assisted" => FactSource::ModelAssisted,
                "human" => FactSource::Human,
                other => {
                    return Err(ApprovalError::Unreadable {
                        id: id.to_string(),
                        detail: format!("fact source {other:?} is not one of §4.4's three"),
                    });
                }
            };
            let built = match source {
                FactSource::Deterministic => Fact::deterministic(fact.key, fact.value),
                FactSource::ModelAssisted => Fact::model_assisted(fact.key, fact.value),
                FactSource::Human => Fact::human(fact.key, fact.value),
            };
            Ok(match fact.evidence {
                Some(evidence) => built.with_evidence(evidence),
                None => built,
            })
        })
        .collect()
}

fn encode_scope(scope: &Scope) -> String {
    let map: std::collections::BTreeMap<&str, &str> = scope
        .pairs()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
}

fn decode_scope(encoded: &str) -> Scope {
    let map: std::collections::BTreeMap<String, String> =
        serde_json::from_str(encoded).unwrap_or_default();
    Scope::from_pairs(map)
}

fn read_request(row: &rusqlite::Row<'_>) -> ApprovalResult<ApprovalRequestRow> {
    let id: String = row.get(0)?;
    let kind_text: String = row.get(1)?;
    let kind = ApprovalKind::parse(&kind_text).ok_or_else(|| ApprovalError::Unreadable {
        id: id.clone(),
        detail: format!("kind {kind_text:?} is not one of §4.3's four"),
    })?;
    let subject_column: Option<String> = row.get(2)?;
    let action: String = row.get(4)?;
    let subject =
        Subject::from_stored(kind, &action, subject_column.as_deref()).ok_or_else(|| {
            ApprovalError::Unreadable {
                id: id.clone(),
                detail: format!(
                    "a {kind} cannot be rebuilt from action={action:?} subject={subject_column:?}"
                ),
            }
        })?;
    let run_id: Option<String> = row.get(3)?;
    let facts_encoded: String = row.get(5)?;
    let facts_source_text: String = row.get(6)?;
    let matched: String = row.get(8)?;
    let state_text: String = row.get(11)?;
    let state = RequestState::parse(&state_text).ok_or_else(|| ApprovalError::Unreadable {
        id: id.clone(),
        detail: format!("request state {state_text:?} is not one of §5.2's four"),
    })?;
    Ok(ApprovalRequestRow {
        facts: decode_facts(&id, &facts_encoded)?,
        facts_source: match facts_source_text.as_str() {
            "deterministic" => FactSource::Deterministic,
            "model_assisted" => FactSource::ModelAssisted,
            "human" => FactSource::Human,
            other => {
                return Err(ApprovalError::Unreadable {
                    id,
                    detail: format!("facts_source {other:?} is not one of §4.4's three"),
                });
            }
        },
        run_id: run_id
            .map(RunId::new)
            .transpose()
            .map_err(|e| ApprovalError::Unreadable {
                id: id.clone(),
                detail: e.to_string(),
            })?,
        kind,
        subject,
        policy_hash: row.get(7)?,
        matched_rules: serde_json::from_str(&matched)?,
        explanation: row.get(9)?,
        evidence_ref: row.get(10)?,
        state,
        requested_at: row.get(12)?,
        expires: Expiry::from_stored(row.get(13)?),
        id,
    })
}

fn read_grant(row: &rusqlite::Row<'_>) -> ApprovalResult<ApprovalGrantRow> {
    let id: String = row.get(0)?;
    let scope: String = row.get(3)?;
    let state_text: String = row.get(5)?;
    let state = GrantState::parse(&state_text).ok_or_else(|| ApprovalError::Unreadable {
        id: id.clone(),
        detail: format!("grant state {state_text:?} is not one of §5.2's four"),
    })?;
    Ok(ApprovalGrantRow {
        request_id: row.get(1)?,
        stored_binding: BindingHash::from_stored(row.get::<_, String>(2)?),
        scope: decode_scope(&scope),
        reuse: row.get::<_, i64>(4)? != 0,
        state,
        nonce_hash: row.get(6)?,
        channel: row.get(7)?,
        granted_by: row.get(8)?,
        granted_at: row.get(9)?,
        expires: Expiry::from_stored(row.get(10)?),
        resolved_at: row.get(11)?,
        id,
    })
}
