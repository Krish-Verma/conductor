//! Verification — master plan §4.5.
//!
//! **Verification is authoritative. The agent's report is not.**

pub mod classify;
pub mod profile;
pub mod runner;
pub mod secrets;
pub mod toolchain;

pub use classify::{
    Classified, Execution, Termination, TreeWitness, VerificationFinding, classify, combine_flaky,
};
pub use profile::{
    Check, Command, Conditional, LoadedProfile, OnTimeout, Profile, ProfileError, ProfileWarning,
    When,
};
pub use runner::{CheckKind, CheckResult, RunnerConfig, VerificationReport, run_profile};
