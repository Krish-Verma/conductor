//! Durable, exactly-scoped, expiring approvals — master plan §4.3.
//!
//! # What this module is, and what it deliberately is not
//!
//! §4.3 opens by refusing the obvious design: *"A `0600` unix socket does not
//! distinguish a human from a same-user subprocess, and removing an environment
//! variable is obscurity."* So nothing here claims that a grant proves a human
//! granted it. What the module provides is the part that **is** mechanical —
//! that a grant authorizes exactly one operation, expires, cannot be spent
//! twice, survives restart, and can be revoked with a defined outcome in every
//! state it might be in. Whether the granter was a person is a property of the
//! execution mode (ADR-0002), measured per host by S2.5 and reported by
//! [`nonce::Tier`], never asserted by this code.
//!
//! # The four kinds are separate types, not a boolean
//!
//! §4.3: *"Four distinct approval kinds — never collapse into `approved: bool`
//! … Collapsing them would let a plan approval satisfy a deployment gate."*
//! [`kind::ApprovalKind`] carries its [`kind::Subject`] and its
//! [`kind::ExpiryRule`] together, so a plan approval and a policy approval are
//! not merely two values of one type — they authorize different *kinds of
//! thing*, and there is no path that reads one as the other.
//!
//! # Two state machines, not one chain
//!
//! [`store::RequestState`] is `REQUESTED | GRANTED | DENIED | EXPIRED`;
//! [`store::GrantState`] is `GRANTED | CONSUMED | EXPIRED | REVOKED`. A request
//! is never consumed and a grant is never denied. `GRANTED` is where the two
//! meet — the grant row references the request — not a transition between them.
//! Modelling them as one chain would make revocation-after-consumption
//! unrepresentable, and §4.3 gives that case a defined outcome.

pub mod authorize;
pub mod binding;
pub mod gate;
pub mod kind;
pub mod nonce;
pub mod revoke;
pub mod store;

pub use authorize::{Authorization, Refusal, authorize};
pub use binding::{Binding, BindingHash};
pub use kind::{ApprovalKind, Expiry, ExpiryRule, Subject};
pub use nonce::{NonceState, OperatorNonce, Tier, nonce_wanted};
pub use revoke::{Halt, InFlight, RevocationOutcome, revoke};
pub use store::{
    ApprovalError, ApprovalResult, Consumption, GrantOptions, GrantState, NewApprovalRequest,
    RequestState, consume, deny, grant, request,
};
