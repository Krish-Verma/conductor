//! §4.3's tier table, and the operator nonce that tier B rests on.
//!
//! > | Tier | Mechanism | Integrity |
//! > |---|---|---|
//! > | **A. Sandboxed** | seatbelt AF_UNIX default-deny (M10); socket under `$HOME/.conductor/`, which the agent cannot write (M6), so it cannot squat or replace it | **Enforced.** Measured, with positive control (M11). |
//! > | **B. Unsandboxed + operator nonce** | grant requires a nonce printed **only** to the controlling terminal; only `hash(nonce)` is persisted, so reading `conductor.db` does not yield it | **Raises cost substantially. Not a kernel boundary.** |
//! > | **C. Unsandboxed, no nonce** | socket permissions and env scrubbing only | **Not a boundary. Approvals are advisory.** |
//!
//! The table is the point, and it is a statement about honesty. A `0600` socket
//! does not distinguish a human from a same-user subprocess (ADR-0002), so the
//! only tier that is a boundary is the one where the **kernel** refuses the
//! connect — and whether it does is measured, never declared
//! (`control_surface`, §4.2).
//!
//! [`Tier::of`] therefore takes the *measured* `control_surface` and returns
//! what is actually true of this host. There is no way to construct
//! [`Tier::A`] from a configuration file.
//!
//! # The nonce defaults off
//!
//! §4.3's S8 scope: *"operator-nonce mechanism, **default off**, activated when
//! `control_surface < Hard`"*. Off under a measured sandbox because the kernel
//! is already refusing the connect and a second secret adds ceremony, not
//! security. On below that, where it is the only thing raising the cost.
//!
//! # Why the reveal goes to `/dev/tty` and nowhere else
//!
//! §4.3: *"printed **only** to the controlling terminal"*. Not stdout: stdout is
//! routinely redirected into a file, and a file is a thing an unsandboxed agent
//! can read — which would hand the agent the secret whose entire purpose is that
//! the agent cannot have it. If there is no controlling terminal, arming fails
//! and the host is **tier C**, stated as such. Failing to a weaker tier loudly
//! is the whole design; silently printing elsewhere would be tier C wearing
//! tier B's label.

use std::fmt;
use std::io::{Read, Write};

use conductor_core::containment::Enforcement;
use serde::Serialize;

/// §4.3's three tiers of approval integrity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Sandboxed: the kernel refuses the connect. **Enforced.**
    A,
    /// Unsandboxed with an armed operator nonce. Raises cost. **Not a kernel
    /// boundary.**
    B,
    /// Unsandboxed, no nonce. **Not a boundary. Approvals are advisory.**
    C,
}

impl Tier {
    /// What this host actually delivers.
    ///
    /// The only input that can produce [`Tier::A`] is a **measured**
    /// `control_surface` of `Hard` — §4.2's fail-closed vector gives `None` for
    /// an absent or stale probe, so an unmeasured host is never tier A.
    pub fn of(control_surface: Enforcement, nonce: NonceState) -> Tier {
        match (control_surface, nonce) {
            (Enforcement::Hard, _) => Tier::A,
            (_, NonceState::Armed) => Tier::B,
            (_, NonceState::Off) => Tier::C,
        }
    }

    /// §4.3's "Integrity" column, verbatim. Never paraphrased upward.
    pub fn integrity(&self) -> &'static str {
        match self {
            Tier::A => "Enforced. Measured, with positive control (M11).",
            Tier::B => "Raises cost substantially. Not a kernel boundary.",
            Tier::C => "Not a boundary. Approvals are advisory.",
        }
    }

    /// §4.3's "Mechanism" column.
    pub fn mechanism(&self) -> &'static str {
        match self {
            Tier::A => {
                "seatbelt AF_UNIX default-deny; socket under $HOME/.conductor/, \
                 which the agent cannot write, so it cannot squat or replace it"
            }
            Tier::B => {
                "grant requires a nonce printed only to the controlling terminal; \
                 only hash(nonce) is persisted"
            }
            Tier::C => "socket permissions and env scrubbing only",
        }
    }

    /// Whether this tier is a boundary at all. **Only A.**
    ///
    /// A separate accessor rather than `== Tier::A` at each call site, so that
    /// the one place that decides what "boundary" means is here.
    pub fn is_a_boundary(&self) -> bool {
        matches!(self, Tier::A)
    }

    /// Whether a mutating verb must present the operator nonce.
    pub fn requires_nonce(&self) -> bool {
        matches!(self, Tier::B)
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tier::A => f.write_str("A (sandboxed)"),
            Tier::B => f.write_str("B (unsandboxed + operator nonce)"),
            Tier::C => f.write_str("C (unsandboxed, no nonce)"),
        }
    }
}

/// Whether an operator nonce exists for this control surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NonceState {
    /// No nonce. The default (§4.3's S8 scope).
    Off,
    /// A nonce was generated and revealed to the controlling terminal.
    Armed,
}

/// §4.3: the nonce is activated when `control_surface < Hard`, and off
/// otherwise.
///
/// A free function rather than a method on [`Enforcement`] because the rule is
/// §4.3's, not the capability model's — `conductor-core` must not learn about
/// approvals.
pub fn nonce_wanted(control_surface: Enforcement) -> bool {
    control_surface < Enforcement::Hard
}

/// Why arming the nonce failed.
#[derive(Debug, thiserror::Error)]
pub enum NonceError {
    /// `/dev/urandom` would not yield bytes.
    #[error("could not read {} bytes of entropy: {source}", NONCE_BYTES)]
    Entropy {
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// There is no controlling terminal to print to, so tier B is unreachable.
    ///
    /// **Not** a fallback to stdout: §4.3 says the nonce is printed *only* to
    /// the controlling terminal, and a redirected stdout is a file an
    /// unsandboxed agent can read.
    #[error(
        "no controlling terminal ({source}); §4.3 prints the operator nonce only \
         there, so this host is tier C — approvals are advisory, and saying \
         otherwise would be a claim the mechanism does not support"
    )]
    NoTerminal {
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// How many bytes of entropy the nonce carries.
///
/// 32, matching BLAKE3's output. The nonce is typed by a human, so it is
/// rendered as hex; the length is chosen for the guessing cost §4.3 is buying,
/// not for convenience.
pub const NONCE_BYTES: usize = 32;

/// The operator nonce — §4.3 tier B.
///
/// Holds the secret in memory for the life of the server process and **never**
/// writes it anywhere. Only [`OperatorNonce::hash`] is persisted, "so reading
/// `conductor.db` does not yield it".
pub struct OperatorNonce {
    secret: String,
    hash: String,
}

impl OperatorNonce {
    /// Generate a nonce from the kernel's entropy pool.
    ///
    /// `/dev/urandom` rather than a crate: §2.2's dependency list has no RNG,
    /// and adding one to read 32 bytes would be a dependency the slice cannot
    /// justify.
    pub fn generate() -> Result<OperatorNonce, NonceError> {
        let mut bytes = [0u8; NONCE_BYTES];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|source| NonceError::Entropy { source })?;
        let secret = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let hash = conductor_core::effect::content_hash(secret.as_bytes());
        Ok(OperatorNonce { secret, hash })
    }

    /// `blake3(nonce)` — the only representation that is ever persisted.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Whether a presented nonce matches a stored hash.
    ///
    /// Compares digests, not secrets, so a caller that has only the hash can
    /// still check — which is what the socket server does after a restart.
    pub fn matches(stored_hash: &str, presented: &str) -> bool {
        conductor_core::effect::content_hash(presented.as_bytes()) == stored_hash
    }

    /// Print the nonce to the controlling terminal, and to nothing else.
    ///
    /// Opening `/dev/tty` is the mechanism: it is the *controlling terminal*
    /// regardless of what stdout and stderr were redirected to, and it fails
    /// outright when there is none — which is the answer §4.3 needs, because
    /// the alternative is printing the secret somewhere an agent can read.
    pub fn reveal_to_controlling_terminal(&self) -> Result<(), NonceError> {
        let mut tty = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .map_err(|source| NonceError::NoTerminal { source })?;
        writeln!(
            tty,
            "conductor: operator nonce for this session (tier B, §4.3):\n  {}\n\
             present it with `conductor approval approve --nonce <value>`. \
             It is not stored: only blake3(nonce) is.",
            self.secret
        )
        .map_err(|source| NonceError::NoTerminal { source })?;
        tty.flush()
            .map_err(|source| NonceError::NoTerminal { source })
    }

    /// The secret itself. **Test-only**, because a production caller that needs
    /// the secret is a production caller that will eventually log it.
    #[cfg(test)]
    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }
}

/// Prints the hash, never the secret. A `Debug` that leaked it would put the
/// nonce in every panic message and every `{:?}` in a log.
impl fmt::Debug for OperatorNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorNonce")
            .field("secret", &"<redacted>")
            .field("hash", &self.hash)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_measured_hard_control_surface_is_tier_a() {
        for enforcement in Enforcement::ALL {
            for nonce in [NonceState::Off, NonceState::Armed] {
                let tier = Tier::of(*enforcement, nonce);
                assert_eq!(
                    tier.is_a_boundary(),
                    *enforcement == Enforcement::Hard,
                    "{enforcement} + {nonce:?} must not be a boundary unless measured Hard"
                );
            }
        }
    }

    #[test]
    fn the_nonce_is_off_by_default_and_wanted_only_below_hard() {
        assert!(!nonce_wanted(Enforcement::Hard));
        assert!(nonce_wanted(Enforcement::Restricted));
        assert!(nonce_wanted(Enforcement::AuditOnly));
        assert!(nonce_wanted(Enforcement::None));
    }

    #[test]
    fn an_unmeasured_host_without_a_nonce_is_tier_c_and_says_so() {
        let tier = Tier::of(Enforcement::None, NonceState::Off);
        assert_eq!(tier, Tier::C);
        assert!(tier.integrity().contains("Not a boundary"));
        assert!(tier.integrity().contains("advisory"));
        assert!(!tier.requires_nonce());
    }

    #[test]
    fn a_nonce_verifies_by_hash_and_the_secret_is_never_in_debug_output() {
        let nonce = OperatorNonce::generate().expect("/dev/urandom");
        assert_eq!(nonce.secret().len(), NONCE_BYTES * 2);
        assert!(OperatorNonce::matches(nonce.hash(), nonce.secret()));
        assert!(!OperatorNonce::matches(nonce.hash(), "not the nonce"));
        let rendered = format!("{nonce:?}");
        assert!(
            !rendered.contains(nonce.secret()),
            "Debug must not carry the secret: {rendered}"
        );
        assert!(rendered.contains(nonce.hash()));
    }

    #[test]
    fn two_nonces_differ() {
        let a = OperatorNonce::generate().expect("/dev/urandom");
        let b = OperatorNonce::generate().expect("/dev/urandom");
        assert_ne!(a.hash(), b.hash());
    }
}
