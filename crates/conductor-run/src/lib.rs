//! Conductor's runtime crate.
//!
//! S2.5 created it for the containment probe harness. S3 adds the spine:
//! supervision, leases and fencing, startup recovery, and the ownership rules
//! that keep two workers from writing the same generated path.
//!
//! Verification (S4), policy (S7), approvals (S8) and packets (S12) belong to
//! the slices that own them and are deliberately absent.

pub mod containment;
pub mod lease;
pub mod paths;
pub mod recovery;
pub mod supervise;
pub mod worker;

pub use lease::{HeartbeatOutcome, heartbeat};
pub use paths::{ArtifactRoot, OwnedDir, Owner, OwnershipError, Provenance};
pub use recovery::{RecoveryConfig, RecoveryDecision, RecoveryReport, recover};
pub use supervise::{
    ChildAlive, Heartbeat, Liveness, SpawnedAgent, Supervised, SupervisionEnd, SupervisorConfig,
    probe, spawn, start_time_us,
};
pub use worker::{AttemptOutcomeRecord, WorkerConfig, WorkerError, run_one_attempt};
