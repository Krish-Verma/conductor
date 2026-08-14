//! The execution-eligibility gate — master plan §4.2.
//!
//! ```text
//! before launching an attempt:
//!     caps = probe_cache.get(adapter, launcher, host)      // None if stale/absent
//!     if any required dimension > caps dimension:
//!         refuse to launch unattended
//!         emit the dimension, the requirement, and the measured value
//!         offer: attended mode | different adapter | a sandbox launcher
//! ```
//!
//! §4.2: *"~50 lines and one table. It does not rank adapters and does not
//! choose between eligible options. Any growth beyond 'compare a vector, refuse
//! or proceed' is scope creep."* [`check`] is that comparison and nothing else —
//! no scoring, no ordering of adapters, no fallback selection. The three
//! [`OFFERS`] are fixed strings, not a search.
//!
//! # Why `tool_interception` cannot appear here
//!
//! §4.2 and §6.3 forbid a hook-based observation from satisfying a gating
//! requirement — hooks have known bypasses (`sh -c`, script-then-execute,
//! alternate spellings), so letting a policy *require* one would let a
//! non-boundary satisfy a boundary requirement.
//!
//! That is enforced by the type system, in three places, none of which is a
//! runtime check anyone can delete:
//!
//! 1. [`ExecutionRequirements`] is keyed by [`GatingDimension`], which has **no
//!    `ToolInterception` variant** — so a requirement for it is not
//!    representable.
//! 2. `ExecutionCapabilities::tool_interception` is an `Informational`, which has
//!    no [`PartialOrd`] and no accessor returning an `Enforcement`, so it cannot
//!    be compared against a requirement even if one existed.
//! 3. `ExecutionCapabilities::gating` — the only lookup this module performs —
//!    takes a `GatingDimension` and therefore cannot name it either.
//!
//! The YAML loader refuses the name explicitly as well, but that is a courtesy
//! to whoever typed it, not the mechanism. These do not compile:
//!
//! ```compile_fail
//! use conductor_core::containment::ExecutionCapabilities;
//! use conductor_run::policy::eligibility::ExecutionRequirements;
//! let mut requirements = ExecutionRequirements::new();
//! // no such variant
//! requirements.require(conductor_core::containment::GatingDimension::ToolInterception,
//!                      conductor_core::containment::Enforcement::Hard);
//! ```
//!
//! ```compile_fail
//! use conductor_core::containment::{Enforcement, ExecutionCapabilities};
//! let caps = ExecutionCapabilities::fail_closed();
//! // Informational has no PartialOrd<Enforcement>
//! let _satisfied = caps.tool_interception >= Enforcement::Restricted;
//! ```
//!
//! # Stale means unmeasured means nothing is enforced
//!
//! A stale or absent probe yields `ExecutionCapabilities::fail_closed`, whose
//! every gating dimension is `None`. So every requirement above `None` fails and
//! the launch is refused — §4.2's rule applied to the fail-closed vector rather
//! than a second, separately-written refusal that could drift from it.

use std::collections::BTreeMap;

use conductor_core::containment::{Enforcement, ExecutionCapabilities, GatingDimension};
use serde::Serialize;
use serde_yaml::Value;

use super::model::PolicyError;
use crate::containment::cache::CacheLookup;

/// What §4.2 offers when it refuses.
///
/// Fixed strings, deliberately. Choosing between them is a human decision and
/// ranking them would be the adapter-selection logic §4.2 rules out.
pub const OFFERS: &[&str] = &[
    "run attended, so a human is present for every gated action",
    "use a different adapter",
    "use a sandbox launcher, which is what raises the measured value",
];

/// `execution_requirements` — §4.2's per-project or per-task vector.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ExecutionRequirements(BTreeMap<GatingDimension, Enforcement>);

impl ExecutionRequirements {
    /// No requirements: nothing is gated.
    pub fn new() -> ExecutionRequirements {
        ExecutionRequirements(BTreeMap::new())
    }

    /// Require one dimension. The parameter type is what makes
    /// `tool_interception` unrepresentable.
    pub fn require(&mut self, dimension: GatingDimension, enforcement: Enforcement) {
        self.0.insert(dimension, enforcement);
    }

    /// What is required of one dimension, if anything.
    pub fn get(&self, dimension: GatingDimension) -> Option<Enforcement> {
        self.0.get(&dimension).copied()
    }

    /// Every requirement, in §4.2's dimension order.
    pub fn iter(&self) -> impl Iterator<Item = (GatingDimension, Enforcement)> + '_ {
        self.0.iter().map(|(d, e)| (*d, *e))
    }

    /// Whether anything is required.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Parse §4.2's YAML block:
    ///
    /// ```yaml
    /// execution_requirements:
    ///   filesystem_write: restricted
    ///   control_surface:  hard
    /// ```
    ///
    /// An unrecognised dimension is a **hard error**. An ignored requirement is
    /// a requirement that silently does not apply, which is the failure this
    /// whole subsystem exists to prevent.
    pub fn parse_yaml(yaml: &str) -> Result<ExecutionRequirements, PolicyError> {
        let root: Value =
            serde_yaml::from_str(yaml).map_err(|e| PolicyError::Yaml(e.to_string()))?;
        let root = root.as_mapping().ok_or_else(|| PolicyError::Invalid {
            location: "execution_requirements".to_string(),
            detail: "the file must be a mapping".to_string(),
        })?;
        let Some(body) = root.get(Value::from("execution_requirements")) else {
            return Ok(ExecutionRequirements::new());
        };
        let body = body.as_mapping().ok_or_else(|| PolicyError::Invalid {
            location: "execution_requirements".to_string(),
            detail: "must be a mapping of dimension to enforcement".to_string(),
        })?;

        let mut requirements = ExecutionRequirements::new();
        for (key, value) in body {
            let name = key.as_str().ok_or_else(|| PolicyError::Invalid {
                location: "execution_requirements".to_string(),
                detail: "dimension names must be strings".to_string(),
            })?;
            if name == "tool_interception" {
                return Err(PolicyError::NotAGatingDimension(
                    "execution_requirements names `tool_interception`, which is \
                     informational and never gates: hooks have known bypasses \
                     (`sh -c`, script-then-execute, alternate spellings), so a \
                     hook may be reported but may never satisfy a requirement \
                     (§4.2, §6.3)"
                        .to_string(),
                ));
            }
            let dimension = GatingDimension::ALL
                .iter()
                .copied()
                .find(|d| d.as_str() == name)
                .ok_or_else(|| {
                    PolicyError::NotAGatingDimension(format!(
                        "execution_requirements names `{name}`, which is not one \
                         of §4.2's gating dimensions: {}",
                        GatingDimension::ALL
                            .iter()
                            .map(|d| d.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            let text = value.as_str().ok_or_else(|| PolicyError::Invalid {
                location: format!("execution_requirements.{name}"),
                detail: "must be one of none, audit_only, restricted, hard".to_string(),
            })?;
            let enforcement = Enforcement::ALL
                .iter()
                .copied()
                .find(|e| e.as_str().eq_ignore_ascii_case(text))
                .ok_or_else(|| PolicyError::Invalid {
                    location: format!("execution_requirements.{name}"),
                    detail: format!(
                        "`{text}` is not an enforcement level; expected none, \
                         audit_only, restricted or hard"
                    ),
                })?;
            requirements.require(dimension, enforcement);
        }
        Ok(requirements)
    }
}

/// What the probe cache had to say, in a form a refusal can print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "probe", rename_all = "snake_case")]
pub enum ProbeStatus {
    /// An exact match on the version triple.
    Measured {
        /// When the measurement was taken.
        probed_at_ms: i64,
    },
    /// Never probed, or probed before an upgrade — indistinguishable, and
    /// treated identically (§4.2).
    Absent,
    /// A row exists but cannot be read.
    Unusable {
        /// Why not.
        reason: String,
    },
}

/// One dimension whose requirement the measured value does not meet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Shortfall {
    /// Which dimension.
    pub dimension: GatingDimension,
    /// What the task requires.
    pub required: Enforcement,
    /// What was measured — `None` when the probe is stale or absent.
    pub measured: Enforcement,
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} requires {} but this host measures {}",
            self.dimension, self.required, self.measured
        )
    }
}

/// The gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "eligibility", rename_all = "snake_case")]
pub enum Eligibility {
    /// Every requirement is met. The attempt may be launched unattended.
    Eligible {
        /// What was measured.
        measured: ExecutionCapabilities,
        /// Where that measurement came from.
        probe: ProbeStatus,
    },
    /// At least one requirement is not met. §4.2: refuse to launch unattended.
    Refused {
        /// Every unmet requirement, in §4.2's dimension order.
        shortfalls: Vec<Shortfall>,
        /// Where the measurement came from — usually the actual problem.
        probe: ProbeStatus,
        /// §4.2's three offers.
        offers: &'static [&'static str],
    },
}

/// §4.2's gate: compare a vector, refuse or proceed.
pub fn check(lookup: &CacheLookup, requirements: &ExecutionRequirements) -> Eligibility {
    // A miss and an unreadable row both yield `fail_closed`, whose every gating
    // dimension is `None`. That single call is the whole of "stale → refuse":
    // there is no second code path that could be deleted without also deleting
    // the measured path.
    let measured = lookup.capabilities();
    let probe = match lookup {
        CacheLookup::Hit { probed_at_ms, .. } => ProbeStatus::Measured {
            probed_at_ms: *probed_at_ms,
        },
        CacheLookup::Miss => ProbeStatus::Absent,
        CacheLookup::Unusable { reason } => ProbeStatus::Unusable {
            reason: reason.clone(),
        },
    };

    let mut shortfalls = Vec::new();
    for dimension in GatingDimension::ALL {
        let Some(required) = requirements.get(*dimension) else {
            continue;
        };
        let actual = measured.gating(*dimension);
        if required > actual {
            shortfalls.push(Shortfall {
                dimension: *dimension,
                required,
                measured: actual,
            });
        }
    }

    if shortfalls.is_empty() {
        Eligibility::Eligible { measured, probe }
    } else {
        Eligibility::Refused {
            shortfalls,
            probe,
            offers: OFFERS,
        }
    }
}

/// Render a refusal for a human — §4.2: "emit the dimension, the requirement,
/// and the measured value".
pub fn render(eligibility: &Eligibility) -> String {
    match eligibility {
        Eligibility::Eligible { probe, .. } => {
            format!("eligible ({probe:?})\n")
        }
        Eligibility::Refused {
            shortfalls,
            probe,
            offers,
        } => {
            let mut out = String::from("refused to launch unattended (§4.2)\n");
            match probe {
                ProbeStatus::Measured { probed_at_ms } => {
                    out.push_str(&format!("  probe: measured at {probed_at_ms}\n"));
                }
                ProbeStatus::Absent => out.push_str(
                    "  probe: absent or stale for this (adapter × launcher × host); \
                     an unmeasured host enforces nothing\n",
                ),
                ProbeStatus::Unusable { reason } => {
                    out.push_str(&format!("  probe: unreadable ({reason})\n"));
                }
            }
            for shortfall in shortfalls {
                out.push_str(&format!("  {shortfall}\n"));
            }
            out.push_str("  options:\n");
            for offer in *offers {
                out.push_str(&format!("    - {offer}\n"));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_requirements_gates_nothing_even_without_a_probe() {
        // §4.2's rule is `required > measured`. With nothing required there is
        // nothing to compare, and inventing a refusal here would be inventing
        // policy the plan does not state.
        assert!(matches!(
            check(&CacheLookup::Miss, &ExecutionRequirements::new()),
            Eligibility::Eligible { .. }
        ));
    }

    #[test]
    fn the_shortfalls_come_out_in_the_specifications_dimension_order() {
        let mut requirements = ExecutionRequirements::new();
        requirements.require(GatingDimension::CredentialRead, Enforcement::Hard);
        requirements.require(GatingDimension::FilesystemWrite, Enforcement::Hard);
        match check(&CacheLookup::Miss, &requirements) {
            Eligibility::Refused { shortfalls, .. } => {
                assert_eq!(
                    shortfalls.iter().map(|s| s.dimension).collect::<Vec<_>>(),
                    [
                        GatingDimension::FilesystemWrite,
                        GatingDimension::CredentialRead
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_absent_execution_requirements_block_is_no_requirements() {
        let parsed = ExecutionRequirements::parse_yaml("project:\n  adapter: codex\n")
            .expect("a file without the block is not an error");
        assert!(parsed.is_empty());
    }
}
