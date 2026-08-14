//! The policy engine — master plan §4.4, plus §4.2's eligibility gate.
//!
//! > A policy engine decides **what Conductor will do**. It does not decide what
//! > an agent with a shell can do — §4.2 handles that.
//!
//! Six modules, in the order a decision passes through them:
//!
//! | module | what it owns |
//! |---|---|
//! | [`model`] | the algebra: effects, actions, facts, rules, exceptions, the four built-in invariants |
//! | [`load`] | YAML → model, the canonical snapshot, its BLAKE3 hash, and run-lifetime pinning |
//! | [`facts`] | §4.4's deterministic fact extractors, and the type that keeps `architecture.change` model-assisted |
//! | [`evaluate`] | the two stages: ceiling, then join |
//! | [`explain`] | rendering a decision, including every rule that did *not* apply |
//! | [`eligibility`] | §4.2's "compare a vector, refuse or proceed" |
//!
//! # What S7 deliberately does not do
//!
//! * **Approvals (S8).** A `require_approval` here is a *decision that a human
//!   must be asked*. Nothing in this module asks one, and there is no socket.
//! * **Enforcement (S9).** Nothing here stops an agent doing anything; it
//!   decides what Conductor will do, and the wiring into the attempt-launch path
//!   belongs to the slice that owns enforcement.
//! * **Producing model-assisted facts.** [`facts::ProxyObservation`] exists so
//!   that a future model-assisted fact *cannot* carry a `deny`. No model runs
//!   here.

pub mod eligibility;
pub mod evaluate;
pub mod explain;
pub mod facts;
pub mod load;
pub mod model;

pub use eligibility::{Eligibility, ExecutionRequirements, ProbeStatus, Shortfall};
pub use evaluate::{Decision, DriftDecision, Request, drift, evaluate};
pub use load::{Pinned, PolicyError, Snapshot, pinned_for_run, snapshot};
pub use model::{
    Action, ActionPattern, BuiltinInvariant, Effect, Fact, FactSet, FactSource, Origin,
    PolicyDocument, PolicyException, ResolvedPolicy, Rule, Scope,
};
