//! Deciding whether a grant authorizes the operation about to happen.
//!
//! §4.3: *"A grant authorizes an operation only if the recomputed hash matches
//! at use time."* Two words carry the design.
//!
//! **Recomputed.** The binding is built here, from the [`Decision`] that is
//! about to be acted on — its action, its facts, its `policy_hash` — and
//! compared against what the row stores. Nothing reads a stored binding to
//! decide anything, so a row whose digest disagrees with its own inputs (a hand
//! edit, a partial write, a serializer change) authorizes nothing. Same doctrine
//! schema v5 records for `repair_observation.fingerprint`.
//!
//! **At use time.** Not at grant time, and not once per run. The policy
//! snapshot is in the preimage, so a grant stops applying the moment the run's
//! snapshot differs — §4.3 calls that "correct, not inconvenient".
//!
//! # A `deny` is not approvable, and that is ADR-0010's carry-forward
//!
//! ADR-0010's third revisit trigger is *"S8 granting approvals in a way that
//! lets a capped deny be satisfied more cheaply than an uncapped one"*.
//!
//! §4.4's cap turns a `deny` resting on a `model_assisted` fact into a
//! `require_approval` — deliberately, so that a model is never the sole reason
//! Conductor blocks work. That capped effect is satisfiable by one human grant.
//! The uncapped `deny` must therefore be satisfiable by **none**, or the
//! deterministic rule would be the weaker of the two and the cap would have
//! become a discount rather than a routing decision.
//!
//! So [`authorize`] answers on the **effect**, before it looks at any grant:
//! only `require_approval` is approvable. `deny` is refused with a grant
//! present, absent, or issued for that exact operation. (The other half is in
//! [`super::binding`]: fact sources are in the preimage, so a grant obtained
//! under the capped reading does not bind to the deterministic one.)

use rusqlite::Connection;

use super::binding::{Binding, BindingHash};
use super::store::{self, ApprovalResult};
use crate::policy::evaluate::Decision;
use crate::policy::model::{Effect, Scope};

/// What [`authorize`] concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authorization {
    /// A live grant binds to exactly this operation. **Consume it immediately
    /// before the side effect** — [`super::store::consume`] — not here.
    Authorized {
        /// The grant that authorizes it.
        grant_id: String,
    },
    /// The operation is not authorized, with the reason.
    Refused(Refusal),
}

/// Why an operation is not authorized.
///
/// Every variant names the specific value that failed, for §4.4's reason:
/// *"negative results are what people debug"*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// §4.4 resolved `deny`, and no human grant reaches a deny (ADR-0010).
    DenyIsNotApprovable {
        /// The action.
        action: String,
    },
    /// §4.4 resolved something other than `require_approval`, so there is no
    /// gate to satisfy. Consuming a grant here would spend a one-shot on an
    /// operation nobody gated.
    NotGated {
        /// What the policy actually resolved.
        effect: Effect,
    },
    /// No live grant binds to this operation. The usual case, and the one the
    /// binding exists to produce.
    NoMatchingGrant {
        /// The binding that was recomputed and not found.
        binding: BindingHash,
    },
    /// A grant exists for a different one of §4.3's four kinds.
    WrongKind {
        /// What the grant is for.
        held: super::kind::ApprovalKind,
        /// What the operation needs.
        required: super::kind::ApprovalKind,
    },
    /// The grant's TTL has passed.
    Expired {
        /// When it expired.
        expires_at_ms: i64,
        /// The clock it was compared against.
        now_ms: i64,
    },
    /// A one-shot grant that has already been spent.
    AlreadyConsumed {
        /// Which grant.
        grant_id: String,
    },
    /// A human took it back (§4.3's revocation table).
    Revoked {
        /// Which grant.
        grant_id: String,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::DenyIsNotApprovable { action } => write!(
                f,
                "{action} resolves to deny, and a deny is not approvable: a grant \
                 that cleared one would make the deterministic rule weaker than \
                 the model-assisted one it caps (§4.4, ADR-0010)"
            ),
            Refusal::NotGated { effect } => {
                write!(f, "the policy resolves {effect}, so no approval is needed")
            }
            Refusal::NoMatchingGrant { binding } => {
                write!(f, "no live grant binds to {binding}")
            }
            Refusal::WrongKind { held, required } => write!(
                f,
                "the grant is a {held} and this needs a {required} (§4.3: the four \
                 kinds never collapse)"
            ),
            Refusal::Expired {
                expires_at_ms,
                now_ms,
            } => write!(f, "the grant expired at {expires_at_ms} (now {now_ms})"),
            Refusal::AlreadyConsumed { grant_id } => {
                write!(f, "grant {grant_id} has already been consumed")
            }
            Refusal::Revoked { grant_id } => write!(f, "grant {grant_id} was revoked"),
        }
    }
}

/// May this operation happen? — §4.3's question, asked of a live decision.
///
/// `scope` is the operation's scope, e.g. `{run: r-0041}`. It is a parameter
/// rather than something read out of the decision because §4.3 lets a grant be
/// scoped more narrowly or more broadly than the evaluation that produced the
/// request, and the caller is what knows which operation it is about to
/// perform.
pub fn authorize(
    conn: &Connection,
    decision: &Decision,
    scope: &Scope,
    now_ms: i64,
) -> ApprovalResult<Authorization> {
    // The effect first, and deliberately before any grant is looked at. See the
    // module docs: this is the line that keeps a capped deny from being the
    // cheap path.
    match decision.effect {
        Effect::Deny => {
            return Ok(Authorization::Refused(Refusal::DenyIsNotApprovable {
                action: decision.action.as_str().to_string(),
            }));
        }
        Effect::Allow => {
            return Ok(Authorization::Refused(Refusal::NotGated {
                effect: decision.effect,
            }));
        }
        Effect::RequireApproval => {}
    }

    let binding = Binding::for_decision(decision, scope);
    let required = binding.kind();
    let hash = binding.hash();

    let mut refusal = Refusal::NoMatchingGrant {
        binding: hash.clone(),
    };
    for row in store::live_grants_for_binding(conn, &hash)? {
        // The stored binding got us here; recomputing it is what decides. The
        // equality is restated rather than assumed from the query so that a
        // future index change cannot quietly turn this into a trust of the row.
        if row.stored_binding != hash {
            continue;
        }
        // The kind travels in the preimage, so a mismatch here means a hash
        // collision or a hand-edited row. Reported rather than ignored.
        let kind = request_kind(conn, &row.request_id)?;
        if kind != Some(required) {
            if let Some(held) = kind {
                refusal = Refusal::WrongKind { held, required };
            }
            continue;
        }
        match store::refuse_unusable(&row, now_ms) {
            None => return Ok(Authorization::Authorized { grant_id: row.id }),
            Some(reason) => refusal = reason,
        }
    }

    // Nothing live matched. Look for a row that *did* bind but is no longer
    // usable, so the refusal says "expired" or "already consumed" rather than
    // the much less useful "no matching grant".
    if matches!(refusal, Refusal::NoMatchingGrant { .. })
        && let Some(reason) = why_not(conn, &hash, now_ms)?
    {
        refusal = reason;
    }
    Ok(Authorization::Refused(refusal))
}

/// The reason a binding that once had a grant no longer authorizes anything.
fn why_not(
    conn: &Connection,
    binding: &BindingHash,
    now_ms: i64,
) -> ApprovalResult<Option<Refusal>> {
    let mut stmt = conn.prepare(
        "SELECT id, state, expires_at FROM approval_grant
          WHERE binding_hash = ?1 ORDER BY granted_at DESC, id DESC LIMIT 1",
    )?;
    let mut rows = stmt.query(rusqlite::params![binding.as_str()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let id: String = row.get(0)?;
    let state: String = row.get(1)?;
    let expires_at: Option<i64> = row.get(2)?;
    Ok(match state.as_str() {
        "CONSUMED" => Some(Refusal::AlreadyConsumed { grant_id: id }),
        "REVOKED" => Some(Refusal::Revoked { grant_id: id }),
        "EXPIRED" => Some(Refusal::Expired {
            expires_at_ms: expires_at.unwrap_or(now_ms),
            now_ms,
        }),
        _ => None,
    })
}

fn request_kind(
    conn: &Connection,
    request_id: &str,
) -> ApprovalResult<Option<super::kind::ApprovalKind>> {
    use rusqlite::OptionalExtension;
    let stored: Option<String> = conn
        .query_row(
            "SELECT kind FROM approval_request WHERE id = ?1",
            rusqlite::params![request_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored.as_deref().and_then(super::kind::ApprovalKind::parse))
}
