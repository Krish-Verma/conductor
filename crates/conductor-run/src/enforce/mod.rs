//! S9 — enforcement: making the environment, not the prompt, the boundary.
//!
//! Master plan §4.9 ranks the layers, and the ranking is the design:
//!
//! | Layer | Prevents |
//! |---|---|
//! | 1. Prompt instructions | **nothing** |
//! | 6. **Credential absence** | **push, deploy, cloud API, DB access** |
//! | 8. Post-run audit | nothing — but detects almost everything |
//!
//! This module owns layers 6 and 8. [`env`] builds the agent's entire
//! environment from an allowlist and creates the per-run `HOME` and `TMPDIR` it
//! names. [`audit`] reads what actually happened afterwards and turns every
//! unexplained delta into a `Finding` that never auto-resolves.
//!
//! # What is deliberately not here
//!
//! The master plan lists `enforce/secrets.rs`. There is no such file, because
//! S4 already built the scanner at [`crate::verify::secrets`] — with its
//! detection rules, its redaction, and, most importantly, its published
//! [`NOT_DETECTED`](crate::verify::secrets::NOT_DETECTED) list of what it cannot
//! see. A second scanner would be a second answer to "is this text safe to
//! show", and the two would drift. [`audit`] calls the existing one.
//!
//! The eligibility gate is likewise not duplicated here: it is
//! [`crate::policy::eligibility::check`], written at S7 as a pure function. S9's
//! job was never to re-decide eligibility — it was to make the decision
//! *reachable from a real launch*, which is a call site in
//! [`crate::worker`], not a new module.

pub mod audit;
pub mod env;
pub mod launch;
pub mod policy_gate;
