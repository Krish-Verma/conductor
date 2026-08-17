//! Packets — master plan §6.5, §6.6 (slice S12).
//!
//! # The rule that decides the design
//!
//! §6.5: *"**Every packet is generated from durable state, content-hashed, and
//! stored as an artifact.** No packet is assembled from conversation history. A
//! packet that cannot be regenerated from the store plus the repository is a
//! bug."*
//!
//! "The store **plus the repository**" is the operative phrase. Neither half is
//! enough on its own: the store knows which plan version a run is pinned to and
//! what its policy hash is, and the repository holds the prose — the objective,
//! the acceptance criteria, the decision bodies — that §3.2 keeps in git so it
//! survives the store. A builder that read only rows would have no objective to
//! give the agent; one that read only files could not tell which version this
//! run is on.
//!
//! # Determinism is a mechanism, not a promise
//!
//! §6.6: *"Packets and policy snapshots must serialize **byte-identically** for
//! identical state … `binding_hash` and `policy_hash` are worthless if
//! serialization is nondeterministic. Sorted keys, LF, no timestamps inside
//! hashed content."*
//!
//! Three things enforce it here, and each corresponds to a way it could
//! silently fail:
//!
//! * **Sorted keys** — the canonical encoder walks mappings in sorted key
//!   order, so no `HashMap` iteration order can reach the bytes.
//! * **No wall clock** — nothing in [`Emitted`] is stamped with a time. When a
//!   packet needs an emission time, it belongs to the artifact record around the
//!   packet, never inside the bytes the hash covers.
//! * **A domain separator of its own.** [`PACKET_CANONICAL_VERSION`] is not
//!   [`crate::plan::hash::CANONICAL_VERSION`]. Sharing one would put plans and
//!   packets in a single digest space, and a value that can be read as either is
//!   a value that can be substituted for the other — the same argument
//!   [`crate::plan::project::CONFIG_HASH_DOMAIN`] makes.
//!
//! # The budget, and what it may not do to stay inside it
//!
//! §6.5 targets *"<4 KB"* for the implementation packet and says evidence is
//! *"linked, never embedded"*. A ceiling that is enforced by **truncation** is
//! worse than no ceiling: a packet that drops the reason for a policy finding in
//! order to fit is a packet that lies about why the agent is constrained. So
//! [`TARGET_PACKET_BYTES`] is advisory and reported, [`MAX_PACKET_BYTES`] is a
//! hard ceiling, and crossing it is a **refusal** naming the overflow — never a
//! quietly shortened field.
//!
//! # Secrets are redacted, not refused (S12)
//!
//! A packet is the one artifact Conductor **re-publishes to a different agent**:
//! §6.5's continuation packet carries the previous attempt's partial report, and
//! its repair packet carries a failing check's output and the previous diff. All
//! three are text nobody vetted — a test that printed an environment variable, a
//! diff that added a credential, an agent that narrated what it exported — and the
//! next agent is a *separate process, often a separate session*. So every string
//! in a packet goes through [`crate::verify::secrets::redact`] on its way out, at
//! [`render`], which is the single point both the YAML and the hashed bytes pass
//! through.
//!
//! **Redacted rather than refused**, and the reason is the same one the budget
//! gives: refusing would discard the evidence the run stopped for, and a packet
//! that cannot be delivered is a run that cannot continue. The marker is visible
//! and names the kind, so a reader can tell text that was removed from text that
//! never existed — and so `github-token` and `aws-access-key-id` do not collapse
//! into one indistinguishable blackout, which is the difference between rotating
//! one credential and rotating two.
//!
//! Redaction is a **pure function of the text**, so §6.6 is untouched: identical
//! state still produces identical bytes. It happens *before* hashing deliberately
//! — a digest over the unredacted original would name a document nobody has, and
//! would be the one copy of the secret that survived.
//!
//! What it does not catch is written down in
//! [`crate::verify::secrets::NOT_DETECTED`], and that list is part of this
//! guarantee rather than a footnote to it.

pub mod continuation;
pub mod implementation;
pub mod repair;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

/// Domain separator for every packet digest.
///
/// Deliberately not the plan's. See this module's docs.
pub const PACKET_CANONICAL_VERSION: &str = "conductor.packet.canonical.v1";

/// §6.5's stated aim for the implementation packet — *"target: <4 KB"*.
///
/// Advisory. A packet over this is reported, not refused: the number is a
/// design pressure toward linking evidence, and a task with genuinely many
/// acceptance criteria is not malformed.
pub const TARGET_PACKET_BYTES: usize = 4 * 1024;

/// The hard ceiling.
///
/// §6.5 gives no number for this one, only the 4 KB target. It is set an order
/// of magnitude above that target because the two limits do different jobs: the
/// target pushes evidence out of the packet and into links, while this one is
/// the point at which something has gone wrong enough that emitting anything
/// would be worse than stopping — a decision body that turned out to be a
/// pasted log, a criteria list generated in a loop.
///
/// It is a refusal rather than a truncation. See this module's docs.
pub const MAX_PACKET_BYTES: usize = 64 * 1024;

/// `blake3:<hex>` over a packet's canonical bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PacketHash(String);

impl PacketHash {
    /// The `blake3:<hex>` text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Hash already-canonical bytes.
    pub fn from_bytes(bytes: &[u8]) -> PacketHash {
        PacketHash(format!("blake3:{}", blake3::hash(bytes).to_hex()))
    }
}

impl std::fmt::Display for PacketHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Anything that stops a packet being built or emitted.
#[derive(Debug, thiserror::Error)]
pub enum PacketError {
    /// The packet is larger than [`MAX_PACKET_BYTES`].
    ///
    /// Names both numbers, because the reader's next question is "by how much",
    /// and a message that reports only the ceiling cannot answer it.
    #[error(
        "packet is {bytes} bytes, over the {ceiling} byte ceiling; §6.5 links \
         evidence rather than embedding it, and truncating to fit would drop \
         the reason for a constraint rather than the constraint — refusing \
         instead"
    )]
    OverBudget {
        /// What it came to.
        bytes: usize,
        /// [`MAX_PACKET_BYTES`].
        ceiling: usize,
    },
    /// A run, task, plan version or project the packet needs is not there.
    #[error("{what} {id} is not in the store; a packet is generated from durable state (§6.5)")]
    Missing {
        /// Which kind of row.
        what: &'static str,
        /// Which one.
        id: String,
    },
    /// The plan document names a decision nothing defines.
    ///
    /// Refused rather than omitted: a task that says it needs an argument and a
    /// packet that silently does not carry it is the context-minimization rule
    /// failing in the direction that loses information.
    #[error(
        "task {task} references decision {decision}, which no document under \
         `.conductor/decisions/` defines; refusing rather than shipping a packet \
         that silently drops an argument the plan says the task needs"
    )]
    UnknownDecision {
        /// The task.
        task: String,
        /// The reference that did not resolve.
        decision: String,
    },
    /// The plan version a run is pinned to does not contain its task.
    #[error("plan version v{version} does not declare task {task}")]
    TaskNotInPlan {
        /// The version.
        version: u32,
        /// The task.
        task: String,
    },
    /// A file the packet needs could not be read.
    #[error("{what} at {path}: {source}")]
    Io {
        /// What was wanted.
        what: &'static str,
        /// Where.
        path: PathBuf,
        /// Why not.
        source: std::io::Error,
    },
    /// The store said no.
    #[error(transparent)]
    Store(#[from] conductor_store::StoreError),
    /// A `.conductor/` document did not load.
    #[error("{0}")]
    Document(String),
}

/// One piece of evidence a packet points at.
///
/// §6.5: *"evidence_links: # linked, never embedded"*, and *"Prior diffs linked
/// by path and hash."* Both halves matter — a path alone is a reference that can
/// change under the reader, and a hash alone is not something they can open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    kind: String,
    path: Option<PathBuf>,
    digest: Option<String>,
    inline: Option<String>,
}

impl Evidence {
    /// Point at a file, carrying its digest.
    ///
    /// Reads the file **to hash it** and not to embed it, which is the whole
    /// distinction §6.5 draws. A 600 KB diff contributes a path and 71 bytes of
    /// digest.
    pub fn linked(
        kind: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<Evidence, PacketError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| PacketError::Io {
            what: "an evidence artifact",
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Evidence {
            kind: kind.into(),
            path: Some(path.to_path_buf()),
            digest: Some(format!("blake3:{}", blake3::hash(&bytes).to_hex())),
            inline: None,
        })
    }

    /// Carry a short value in the packet itself.
    ///
    /// For the things that are the point rather than the attachment — the reason
    /// a policy rule fired, which §6.5's budget must never be allowed to drop.
    pub fn inline(kind: impl Into<String>, text: impl Into<String>) -> Evidence {
        Evidence {
            kind: kind.into(),
            path: None,
            digest: None,
            inline: Some(text.into()),
        }
    }

    fn to_value(&self) -> Value {
        let mut map = serde_yaml::Mapping::new();
        map.insert(Value::from("kind"), Value::from(self.kind.clone()));
        if let Some(path) = &self.path {
            map.insert(Value::from("path"), Value::from(path.display().to_string()));
        }
        if let Some(digest) = &self.digest {
            map.insert(Value::from("digest"), Value::from(digest.clone()));
        }
        if let Some(inline) = &self.inline {
            map.insert(Value::from("value"), Value::from(inline.clone()));
        }
        Value::Mapping(map)
    }
}

/// A packet's canonical form: the bytes, and the digest over them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emitted {
    bytes: Vec<u8>,
    hash: PacketHash,
}

impl Emitted {
    /// The canonical bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `blake3:<hex>` over them.
    pub fn hash(&self) -> &PacketHash {
        &self.hash
    }

    /// Whether §6.5's 4 KB target was met — reported, never enforced.
    pub fn within_target(&self) -> bool {
        self.bytes.len() <= TARGET_PACKET_BYTES
    }
}

/// Canonicalize a packet document: domain separator, then sorted-key encoding.
///
/// The encoding is deliberately the *same shape* as
/// [`crate::plan::hash::canonical_bytes`] — length-prefixed, type-tagged, keys
/// sorted — because two canonicalizers that disagree about what two documents
/// mean is exactly the failure §3.6 warns about. What differs is the separator.
///
/// The document is [`render`]ed first, so the bytes that are hashed are the bytes
/// that were delivered.
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(PACKET_CANONICAL_VERSION.as_bytes());
    out.push(b'\n');
    encode(&render(value), &mut out);
    out
}

/// The document as it leaves Conductor: every string redacted, and every kind
/// found named in a `redacted:` field the reader can see.
///
/// One function, used by both the YAML rendering and the canonical bytes, because
/// the alternative is a hash that covers something other than what was sent. See
/// this module's docs for why this redacts rather than refuses.
pub fn render(value: &Value) -> Value {
    let mut kinds: BTreeSet<&'static str> = BTreeSet::new();
    let mut redacted = redact_value(value, &mut kinds);
    if !kinds.is_empty()
        && let Value::Mapping(map) = &mut redacted
    {
        map.insert(
            Value::from("redacted"),
            Value::Sequence(kinds.into_iter().map(Value::from).collect()),
        );
    }
    redacted
}

/// Walk a document, redacting every string in it.
///
/// Keys as well as values: a mapping key is text a document author chose, and
/// §6.5's packets carry author-supplied keys nowhere — but a key that *did* carry
/// a secret would be the one place a reader would never look.
fn redact_value(value: &Value, kinds: &mut BTreeSet<&'static str>) -> Value {
    match value {
        Value::String(text) => {
            let out = crate::verify::secrets::redact(text);
            for kind in &out.kinds {
                kinds.insert(kind.label());
            }
            Value::String(out.text)
        }
        Value::Sequence(items) => {
            Value::Sequence(items.iter().map(|v| redact_value(v, kinds)).collect())
        }
        Value::Mapping(map) => {
            let mut out = Mapping::new();
            for (k, v) in map {
                out.insert(redact_value(k, kinds), redact_value(v, kinds));
            }
            Value::Mapping(out)
        }
        // Numbers, booleans and nulls cannot carry a secret, and round-tripping
        // them through a redactor would only risk changing them.
        other => other.clone(),
    }
}

/// Canonicalize and hash, refusing anything over [`MAX_PACKET_BYTES`].
pub fn emit(value: &Value) -> Result<Emitted, PacketError> {
    let bytes = canonical_bytes(value);
    if bytes.len() > MAX_PACKET_BYTES {
        return Err(PacketError::OverBudget {
            bytes: bytes.len(),
            ceiling: MAX_PACKET_BYTES,
        });
    }
    let hash = PacketHash(format!("blake3:{}", blake3::hash(&bytes).to_hex()));
    Ok(Emitted { bytes, hash })
}

/// Type-tagged, length-prefixed, sorted-key encoding.
///
/// Tagged so that the string `"1"` and the integer `1` cannot collide, and
/// length-prefixed so that `["a","bc"]` and `["ab","c"]` cannot. Both are the
/// cases a naive concatenation gets wrong.
fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(b'n'),
        Value::Bool(b) => {
            out.push(b'b');
            out.push(if *b { b'1' } else { b'0' });
        }
        Value::Number(n) => {
            out.push(b'i');
            let text = n.to_string();
            out.extend_from_slice(text.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(text.as_bytes());
        }
        Value::String(s) => {
            out.push(b's');
            out.extend_from_slice(s.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(s.as_bytes());
        }
        Value::Sequence(items) => {
            out.push(b'l');
            out.extend_from_slice(items.len().to_string().as_bytes());
            out.push(b':');
            for item in items {
                encode(item, out);
            }
        }
        Value::Mapping(map) => {
            out.push(b'm');
            let mut pairs: Vec<(String, &Value)> =
                map.iter().map(|(k, v)| (scalar_key(k), v)).collect();
            // Sorted keys (§6.6). This is the line that makes map iteration
            // order unable to reach the bytes.
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            out.extend_from_slice(pairs.len().to_string().as_bytes());
            out.push(b':');
            for (key, item) in pairs {
                out.push(b'k');
                out.extend_from_slice(key.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(key.as_bytes());
                encode(item, out);
            }
        }
        Value::Tagged(tagged) => {
            out.push(b't');
            encode(&tagged.value, out);
        }
    }
}

fn scalar_key(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_packet_domain_is_not_the_plans() {
        // Sharing one separator would let a plan digest and a packet digest name
        // the same bytes.
        assert_ne!(
            PACKET_CANONICAL_VERSION,
            crate::plan::hash::CANONICAL_VERSION
        );
    }

    #[test]
    fn map_key_order_cannot_reach_the_bytes() {
        let a: Value = serde_yaml::from_str("b: 2\na: 1\n").expect("yaml");
        let b: Value = serde_yaml::from_str("a: 1\nb: 2\n").expect("yaml");
        assert_eq!(canonical_bytes(&a), canonical_bytes(&b));
    }

    #[test]
    fn adjacent_strings_cannot_be_confused_by_concatenation() {
        let a: Value = serde_yaml::from_str("- a\n- bc\n").expect("yaml");
        let b: Value = serde_yaml::from_str("- ab\n- c\n").expect("yaml");
        assert_ne!(canonical_bytes(&a), canonical_bytes(&b));
    }

    #[test]
    fn a_string_and_a_number_that_look_alike_do_not_collide() {
        let a: Value = serde_yaml::from_str("v: \"1\"\n").expect("yaml");
        let b: Value = serde_yaml::from_str("v: 1\n").expect("yaml");
        assert_ne!(canonical_bytes(&a), canonical_bytes(&b));
    }
}
