//! Conductor's runtime crate.
//!
//! S2.5 created it for the containment probe harness. S3 adds the spine:
//! supervision, leases and fencing, startup recovery, and the ownership rules
//! that keep two workers from writing the same generated path.
//!
//! S4 adds verification; S5 adds the Conductor-owned git effects and the
//! vertical that drives one task from `PENDING` to `COMPLETE`; S6 adds bounded
//! repair; S7 adds the policy engine and §4.2's eligibility gate.
//!
//! Approvals (S8), enforcement (S9) and packets (S12) belong to the slices that
//! own them and are deliberately absent.
//!
//! S12 removed `spec`, which loaded S5's `.conductor/task.yaml`. Its own doc
//! comment said "S11 deletes this" and S11 did not; what replaced it is
//! [`plan`] — the ledger, the materializer and [`plan::runnable`] — so a task's
//! definition now has exactly one source. See `conductor_core::task` for the
//! types that went with it.

pub mod approval;
pub mod containment;
pub mod decision;
pub mod effects;
pub mod enforce;
pub mod lease;
pub mod packet;
pub mod paths;
pub mod plan;
pub mod policy;
pub mod recovery;
pub mod repair;
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
pub use repair::driver::{
    EscalationReason, RepairError, RepairOutcome, Step, ceiling, drive, repair_once,
};
pub use supervise::{
    ChildAlive, Heartbeat, Liveness, SpawnedAgent, Supervised, SupervisionEnd, SupervisorConfig,
    probe, spawn, start_time_us,
};
pub use vertical::{
    Resumed, Vertical, VerticalConfig, VerticalOutcome, resume_task, run_task,
    run_task_with_session,
};
pub use worker::{AttemptOutcomeRecord, WorkerConfig, WorkerError, run_one_attempt};
