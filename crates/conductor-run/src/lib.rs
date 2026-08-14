//! Conductor's runtime crate.
//!
//! S2.5 created it for the containment probe harness. S3 adds the spine:
//! supervision, leases and fencing, startup recovery, and the ownership rules
//! that keep two workers from writing the same generated path.
//!
//! S4 adds verification; S5 adds the Conductor-owned git effects and the
//! vertical that drives one task from `PENDING` to `COMPLETE`.
//!
//! Policy (S7), approvals (S8), repair (S6) and packets (S12) belong to the
//! slices that own them and are deliberately absent.

pub mod containment;
pub mod effects;
pub mod lease;
pub mod paths;
pub mod recovery;
pub mod spec;
pub mod supervise;
pub mod verify;
pub mod vertical;
pub mod worker;

pub use effects::{
    Integration, IntegrationConfig, IntegrationObserver, IntegrationPoint, PreconditionAnswer,
    check_precondition, integrate,
};
pub use lease::{HeartbeatOutcome, heartbeat};
pub use paths::{ArtifactRoot, OwnedDir, Owner, OwnershipError, Provenance};
pub use recovery::{RecoveryConfig, RecoveryDecision, RecoveryReport, recover};
pub use supervise::{
    ChildAlive, Heartbeat, Liveness, SpawnedAgent, Supervised, SupervisionEnd, SupervisorConfig,
    probe, spawn, start_time_us,
};
pub use vertical::{Resumed, Vertical, VerticalConfig, VerticalOutcome, resume_task, run_task};
pub use worker::{AttemptOutcomeRecord, WorkerConfig, WorkerError, run_one_attempt};
