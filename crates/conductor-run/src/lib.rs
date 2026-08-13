//! Conductor's runtime crate.
//!
//! **S2.5 scope: the containment probe harness and nothing else.** Supervision,
//! verification, policy, approvals and packets belong to S3+ and are created by
//! the slices that own them — an empty module here would be a false
//! architectural commitment (CLAUDE.md).

pub mod containment;
