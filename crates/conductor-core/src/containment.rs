//! The execution-security capability model (master plan §4.2).
//!
//! Conductor must know what an execution mode actually *enforces*, and must
//! never treat a weaker mode as equivalent to a stronger one. Two properties of
//! this module carry that:
//!
//! 1. **Fail closed.** [`ExecutionCapabilities::fail_closed`] is `None` on every
//!    gating dimension. An absent or stale probe yields exactly this value, so a
//!    host that has not been measured can satisfy no requirement at all.
//! 2. **`tool_interception` cannot gate.** §4.2 keeps it — hooks are worth
//!    reporting — but forbids it from satisfying a requirement, because hooks
//!    have known bypasses (`sh -c`, script-then-execute, alternate spellings)
//!    and letting a policy *require* one would let a non-boundary satisfy a
//!    boundary requirement. That is enforced here by the type system rather than
//!    by a comment: it is stored as [`Informational`], which has no ordering and
//!    no way to yield an [`Enforcement`] back, and [`GatingDimension`] — the
//!    only thing an eligibility check can range over — has no variant for it.
//!
//! `repository_isolation` is deliberately **not** a dimension: after §4.1 it is a
//! constant Conductor guarantees identically for every adapter, and a dimension
//! whose value never varies is an invariant, not a dimension.
//!
//! The eligibility *rule* ("refuse if any required dimension outranks the
//! measured one") is S7 and does not live here. This crate is pure: it defines
//! the lattice and who may enter it.

use std::cmp::Ordering;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What an execution mode does about one dimension.
///
/// Ordered by strength, so `required <= measured` is the eligibility test:
/// `None` < `AuditOnly` < `Restricted` < `Hard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Enforcement {
    /// Kernel-enforced deny, verified by probe **with a positive control**.
    Hard,
    /// Enforced, with a known and enumerated exception set.
    Restricted,
    /// Not prevented; reliably detected after the fact.
    AuditOnly,
    /// Neither prevented nor detected. Also the fail-closed value.
    None,
}

impl Enforcement {
    /// Every variant, strongest first.
    pub const ALL: &'static [Enforcement] = &[
        Enforcement::Hard,
        Enforcement::Restricted,
        Enforcement::AuditOnly,
        Enforcement::None,
    ];

    /// The string persisted in `containment_probe.capabilities`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Enforcement::Hard => "HARD",
            Enforcement::Restricted => "RESTRICTED",
            Enforcement::AuditOnly => "AUDIT_ONLY",
            Enforcement::None => "NONE",
        }
    }

    /// Rank used for ordering. Declaration order is strongest-first to match
    /// §4.2, so the ordering is written out rather than derived.
    fn strength(&self) -> u8 {
        match self {
            Enforcement::None => 0,
            Enforcement::AuditOnly => 1,
            Enforcement::Restricted => 2,
            Enforcement::Hard => 3,
        }
    }
}

impl Ord for Enforcement {
    fn cmp(&self, other: &Self) -> Ordering {
        self.strength().cmp(&other.strength())
    }
}

impl PartialOrd for Enforcement {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Enforcement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An observation that is **reported but may never gate**.
///
/// This is not an [`Enforcement`] and cannot become one. It has no
/// [`PartialOrd`], so it cannot be compared against a requirement, and it
/// exposes no accessor returning an `Enforcement`, so it cannot be unwrapped
/// into something that can. Equality is available because two probe rows must
/// be diffable; equality is not a gate.
///
/// A gating comparison does not compile:
///
/// ```compile_fail
/// use conductor_core::containment::{Enforcement, ExecutionCapabilities};
/// let caps = ExecutionCapabilities::fail_closed();
/// // no PartialOrd<Enforcement> for Informational
/// let _ = caps.tool_interception >= Enforcement::Restricted;
/// ```
///
/// Neither does comparing it against another `Informational`:
///
/// ```compile_fail
/// use conductor_core::containment::ExecutionCapabilities;
/// let caps = ExecutionCapabilities::fail_closed();
/// let _ = caps.tool_interception > caps.tool_interception;
/// ```
///
/// Nor does smuggling it into a position that expects an `Enforcement`:
///
/// ```compile_fail
/// use conductor_core::containment::{Enforcement, ExecutionCapabilities};
/// fn requires(_measured: Enforcement) {}
/// let caps = ExecutionCapabilities::fail_closed();
/// requires(caps.tool_interception);
/// ```
///
/// Reporting it does. This case is also the non-vacuity control for the three
/// above: every path, field and method they use is exercised here, so they can
/// only be failing on the comparison itself and not on a typo — the way S0's
/// first containment round failed (ADR-0002, "Methodology record").
///
/// ```
/// use conductor_core::containment::{Enforcement, ExecutionCapabilities, Informational};
/// let caps = ExecutionCapabilities::fail_closed();
/// let observed = Informational::new(Enforcement::Restricted);
/// assert_eq!(observed.label(), "RESTRICTED");
/// // the field is reachable, and equality — which is not a gate — compiles
/// assert_eq!(caps.tool_interception, Informational::new(Enforcement::None));
/// // while the same comparison on a gating dimension does compile
/// assert!(caps.filesystem_write < Enforcement::Hard);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Informational(Enforcement);

impl Informational {
    /// Record an observation. Construction is not gating.
    pub fn new(observed: Enforcement) -> Self {
        Informational(observed)
    }

    /// How to print it. The only way out of this type is text.
    pub fn label(&self) -> &'static str {
        self.0.as_str()
    }
}

impl fmt::Display for Informational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The dimensions an eligibility check may range over — §4.2's four gating
/// dimensions, and nothing else. There is deliberately no `ToolInterception`
/// variant: a loop over [`GatingDimension::ALL`] structurally cannot reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatingDimension {
    /// Writes outside the workspace.
    FilesystemWrite,
    /// Outbound network.
    NetworkEgress,
    /// Conductor's own control socket (§4.3).
    ControlSurface,
    /// Reading credentials the task was not given.
    CredentialRead,
}

impl GatingDimension {
    /// The four gating dimensions, in §4.2's order.
    pub const ALL: &'static [GatingDimension] = &[
        GatingDimension::FilesystemWrite,
        GatingDimension::NetworkEgress,
        GatingDimension::ControlSurface,
        GatingDimension::CredentialRead,
    ];

    /// Field name, as it appears in the serialized capabilities.
    pub fn as_str(&self) -> &'static str {
        match self {
            GatingDimension::FilesystemWrite => "filesystem_write",
            GatingDimension::NetworkEgress => "network_egress",
            GatingDimension::ControlSurface => "control_surface",
            GatingDimension::CredentialRead => "credential_read",
        }
    }
}

impl fmt::Display for GatingDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one (adapter × launcher) pair enforces on this host — master plan §4.2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionCapabilities {
    /// Writes outside the workspace. Gates.
    pub filesystem_write: Enforcement,
    /// Outbound network. Gates.
    pub network_egress: Enforcement,
    /// Reaching Conductor's control socket. Gates.
    pub control_surface: Enforcement,
    /// Reading credentials. Gates.
    pub credential_read: Enforcement,
    /// Hook-based tool interception. **Informational only — never gates.**
    pub tool_interception: Informational,
    /// The enumerated exception set behind a `Restricted` verdict.
    #[serde(default)]
    pub exceptions: Vec<PathBuf>,
}

impl ExecutionCapabilities {
    /// The value of an absent, stale or broken probe: nothing is enforced, so
    /// nothing can be authorized on the strength of it (§4.2).
    pub fn fail_closed() -> Self {
        ExecutionCapabilities {
            filesystem_write: Enforcement::None,
            network_egress: Enforcement::None,
            control_surface: Enforcement::None,
            credential_read: Enforcement::None,
            tool_interception: Informational::new(Enforcement::None),
            exceptions: Vec::new(),
        }
    }

    /// The measured value of one gating dimension. This is the only lookup an
    /// eligibility check needs, and it cannot name `tool_interception`.
    pub fn gating(&self, dimension: GatingDimension) -> Enforcement {
        match dimension {
            GatingDimension::FilesystemWrite => self.filesystem_write,
            GatingDimension::NetworkEgress => self.network_egress,
            GatingDimension::ControlSurface => self.control_surface,
            GatingDimension::CredentialRead => self.credential_read,
        }
    }

    /// Every gating dimension with its measured value, in §4.2's order.
    pub fn gating_dimensions(&self) -> Vec<(GatingDimension, Enforcement)> {
        GatingDimension::ALL
            .iter()
            .map(|dimension| (*dimension, self.gating(*dimension)))
            .collect()
    }

    /// True when nothing is enforced — the fail-closed shape.
    pub fn is_fail_closed(&self) -> bool {
        GatingDimension::ALL
            .iter()
            .all(|dimension| self.gating(*dimension) == Enforcement::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforcement_is_ordered_from_none_up_to_hard() {
        assert!(Enforcement::Hard > Enforcement::Restricted);
        assert!(Enforcement::Restricted > Enforcement::AuditOnly);
        assert!(Enforcement::AuditOnly > Enforcement::None);
    }

    #[test]
    fn fail_closed_is_none_on_every_gating_dimension() {
        let caps = ExecutionCapabilities::fail_closed();
        for dimension in GatingDimension::ALL {
            assert_eq!(
                caps.gating(*dimension),
                Enforcement::None,
                "{dimension} must be None when no probe is available"
            );
        }
        assert!(caps.is_fail_closed());
        assert!(caps.exceptions.is_empty());
    }

    #[test]
    fn gating_ranges_over_exactly_the_four_dimensions() {
        assert_eq!(GatingDimension::ALL.len(), 4);
        let names: Vec<&str> = GatingDimension::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(
            names,
            [
                "filesystem_write",
                "network_egress",
                "control_surface",
                "credential_read"
            ]
        );
    }

    #[test]
    fn gating_reads_the_dimension_it_names() {
        let caps = ExecutionCapabilities {
            filesystem_write: Enforcement::Restricted,
            network_egress: Enforcement::Hard,
            control_surface: Enforcement::Hard,
            credential_read: Enforcement::None,
            tool_interception: Informational::new(Enforcement::Restricted),
            exceptions: vec![PathBuf::from("/tmp")],
        };
        assert_eq!(
            caps.gating(GatingDimension::FilesystemWrite),
            Enforcement::Restricted
        );
        assert_eq!(
            caps.gating(GatingDimension::NetworkEgress),
            Enforcement::Hard
        );
        assert_eq!(
            caps.gating(GatingDimension::ControlSurface),
            Enforcement::Hard
        );
        assert_eq!(
            caps.gating(GatingDimension::CredentialRead),
            Enforcement::None
        );
        assert!(!caps.is_fail_closed());
    }

    #[test]
    fn capabilities_round_trip_through_json_with_the_documented_names() {
        let caps = ExecutionCapabilities {
            filesystem_write: Enforcement::Restricted,
            network_egress: Enforcement::Hard,
            control_surface: Enforcement::Hard,
            credential_read: Enforcement::None,
            tool_interception: Informational::new(Enforcement::AuditOnly),
            exceptions: vec![PathBuf::from("/tmp")],
        };
        let json = serde_json::to_value(&caps).expect("serialize");
        assert_eq!(json["filesystem_write"], "RESTRICTED");
        assert_eq!(json["credential_read"], "NONE");
        // Informational is transparent: the cached JSON stays readable by a
        // human diffing two probe rows.
        assert_eq!(json["tool_interception"], "AUDIT_ONLY");
        assert_eq!(json["exceptions"], serde_json::json!(["/tmp"]));

        let back: ExecutionCapabilities = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, caps);
    }
}
