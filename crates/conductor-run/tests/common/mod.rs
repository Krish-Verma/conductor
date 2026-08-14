//! Shared test setup for `conductor-run`'s tests.
//!
//! One copy is compiled into every test binary in the crate, and each binary
//! uses a subset — the containment helpers below are for `host_probe.rs` and
//! `failure_injection.rs`, the supervision helpers in `agent` are for the rest.
#![allow(dead_code)]

pub mod agent;
pub mod vertical;

use std::path::PathBuf;
use std::time::Duration;

use conductor_run::containment::probe::ProbeConfig;

/// The payload, from this build rather than from `PATH`.
pub const PAYLOAD: &str = env!("CARGO_BIN_EXE_conductor-probe-action");

/// A scratch root that is genuinely outside every region the sandbox permits.
///
/// **Deliberately not `tempfile::tempdir()`.** `$TMPDIR` is writable under
/// `workspace-write` (M7), so a probe rooted there would find every "outside"
/// write permitted and report no containment — which is precisely how S0's
/// first round produced false permissive results (ADR-0002). `ProbeConfig`
/// rejects such a root; see `failure_injection.rs`.
pub fn root(label: &str) -> PathBuf {
    let home = std::env::var_os("HOME").expect("HOME must be set to run the containment tests");
    PathBuf::from(home).join(".conductor").join(format!(
        "probe-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ))
}

/// A probe configuration for tests, rooted outside the permitted regions.
pub fn config() -> ProbeConfig {
    ProbeConfig::new(
        root("probe"),
        PathBuf::from(PAYLOAD),
        Duration::from_secs(30),
    )
    .expect("a root under $HOME/.conductor is a valid probe root")
}
