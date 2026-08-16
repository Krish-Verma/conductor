//! The plan content hash — master plan §3.6, §3.4, ADR-0007.
//!
//! # The requirement, in one sentence
//!
//! §3.6: *"**Hash semantics, not bytes.** The plan hash is computed over
//! parsed-and-canonically-reserialized content: keys sorted, LF endings, no
//! trailing whitespace, no timestamps. Therefore reformatting does **not**
//! invalidate approval; changing any field **does**. YAML comments are excluded
//! from the hash."*
//!
//! Both halves are load-bearing and they pull against each other. A hash over
//! file bytes satisfies the second half and fails the first — re-indenting an
//! approved plan would revoke its approval, and §5.2 makes that *"a hard error
//! requiring `conductor plan reapprove`"*. A hash over too little satisfies the
//! first half and fails the second — a field change that slips past the hash is
//! an edit to an approved plan that keeps its approval, which is precisely what
//! §3.3 says an agent with write access to `.conductor/` would try.
//!
//! # What is hashed: the whole document, not the modelled subset
//!
//! [`content_hash`] parses the YAML into a [`serde_yaml::Value`] tree and hashes
//! **that**, rather than hashing a [`crate::plan::Plan`]. Two consequences, both
//! deliberate:
//!
//! * **A key this version does not model still changes the hash.**
//!   [`crate::plan::model`] has no `deny_unknown_fields`, so an unknown key
//!   loads and is ignored. If the hash were taken over the model, an agent
//!   could append a key that a *later* Conductor honours to a plan whose
//!   approval was granted by *this* one, and the approval would still verify.
//!   §3.3 is explicit that the agent can write this file.
//!
//! * **A plan this version cannot fully model can still be hashed.** §3.3's
//!   third control — *"the store records the approval independently at grant
//!   time. If file and store disagree, **execution halts**"* — is a comparison
//!   of hashes. Making the hash depend on successful modelling would mean a
//!   plan from a newer Conductor produces no hash at all, and a check that
//!   cannot run is a check that does not halt anything.
//!
//! # Why the canonical form is a tagged encoding rather than pretty YAML
//!
//! "Canonically reserialize to YAML and hash the text" is the obvious reading of
//! §3.6, and it has a hole: YAML's own scalar rules mean distinct documents can
//! print identically. The string `"1"` and the integer `1` both print as `1`;
//! `null`, `~` and an empty value all print the same way. Two plans that differ
//! would then share a hash — and a shared hash is a shared approval.
//!
//! So the canonical form here is an unambiguous, self-delimiting encoding: every
//! value carries a type tag, every string and collection carries its length, and
//! mapping entries are sorted by their encoded key. It is not meant to be read
//! by a human — [`canonical_bytes`] is public so that a disagreement can be
//! *diffed*, which is the only thing reading it is for.
//!
//! The length-prefixing follows `approval::binding`, for the same reason it
//! gives: without it, `["ab", "c"]` and `["a", "bc"]` absorb identically.
//!
//! # What is *not* canonicalized: element order
//!
//! §3.6 says "keys sorted". It does not say "elements sorted", and this
//! implementation does not sort them. A sequence is content: reordering two
//! tasks changes the order a reviewer reads them in and the order a `plan show`
//! renders, and — since §3.7 refuses forward dependencies — declaration order is
//! something the plan format itself constrains. Sorting sequences would make
//! those changes invisible to an approval.
//!
//! The cost is that swapping two independent tasks invalidates an approval that
//! arguably could have survived. That is the safe direction of the trade: the
//! failure this hash exists to prevent is a change that keeps its approval, not
//! a change that loses one.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use super::model::PlanError;

/// The canonical encoding's version.
///
/// Absorbed first, so a digest produced by a different encoding can never
/// collide with one produced by this encoding. Changing the encoding changes
/// every plan hash, which is a re-approval event and must be visible as one.
pub const CANONICAL_VERSION: &str = "conductor.plan.canonical.v1";

/// A plan's content hash, rendered as §3.4's trailer spells it.
///
/// ```text
/// Conductor-Plan: v3@blake3:9ac2…
/// ```
///
/// ADR-0007 makes blake3 the only digest in Conductor, so the `blake3:` prefix
/// is a contract rather than a display choice: a bare hex string would leave a
/// future second algorithm no way to be distinguished from this one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanHash(String);

impl PlanHash {
    /// The rendered digest, `blake3:<64 hex chars>`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The content hash of a plan document.
///
/// Takes the file's text, not a parsed [`crate::plan::Plan`] — see this
/// module's docs for why the whole document is hashed.
pub fn content_hash(yaml: &str) -> Result<PlanHash, PlanError> {
    let bytes = canonical_bytes(yaml)?;
    Ok(PlanHash(format!(
        "blake3:{}",
        blake3::hash(&bytes).to_hex()
    )))
}

/// The canonical bytes [`content_hash`] digests.
///
/// Public for one purpose: when two plans that should agree do not, the answer
/// is in the difference between these two byte strings, and a hash function
/// nobody can look inside is a hash function nobody can debug.
pub fn canonical_bytes(yaml: &str) -> Result<Vec<u8>, PlanError> {
    let value: Value = serde_yaml::from_str(yaml).map_err(|e| PlanError::Yaml(e.to_string()))?;
    let mut out = Vec::new();
    out.extend_from_slice(CANONICAL_VERSION.as_bytes());
    out.push(b'\n');
    encode(&value, &mut out);
    Ok(out)
}

/// Append one value's canonical encoding.
///
/// Every branch emits a distinct leading tag byte, so no two YAML values of
/// different kinds can encode alike.
fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"~\n"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"b1\n" } else { b"b0\n" }),
        Value::Number(number) => {
            // A YAML number is one of three Rust types and they are not
            // interchangeable: `i` and `u` overlap in range but not in
            // representation, and `18446744073709551615` has no `i64` form at
            // all. Tagging each keeps the encoding injective.
            if let Some(i) = number.as_i64() {
                out.extend_from_slice(format!("i{i}\n").as_bytes());
            } else if let Some(u) = number.as_u64() {
                out.extend_from_slice(format!("u{u}\n").as_bytes());
            } else if let Some(f) = number.as_f64() {
                // `{:?}` is Rust's shortest representation that round-trips,
                // so two spellings of one float (`1.5`, `1.50`) encode alike
                // while two different floats never do. Non-finite values have
                // no such representation and get fixed tokens.
                let rendered = if f.is_nan() {
                    "nan".to_string()
                } else if f.is_infinite() {
                    if f.is_sign_negative() { "-inf" } else { "inf" }.to_string()
                } else {
                    format!("{f:?}")
                };
                out.extend_from_slice(format!("f{rendered}\n").as_bytes());
            } else {
                // serde_yaml::Number is exhaustively one of the three above.
                // Reaching here would mean the crate grew a fourth, and
                // silently hashing it as null is how two plans come to share a
                // digest — so it gets its own tag instead.
                out.extend_from_slice(b"?\n");
            }
        }
        Value::String(text) => encode_string(text, out),
        Value::Sequence(items) => {
            out.extend_from_slice(format!("q{}:\n", items.len()).as_bytes());
            for item in items {
                encode(item, out);
            }
        }
        Value::Mapping(map) => {
            // Sorted by the key's *encoded* bytes rather than by a rendered
            // key, so that keys of different types (a string `1` and an integer
            // 1) order deterministically instead of comparing equal.
            let mut entries: Vec<(Vec<u8>, &Value)> = map
                .iter()
                .map(|(k, v)| {
                    let mut key = Vec::new();
                    encode(k, &mut key);
                    (key, v)
                })
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            out.extend_from_slice(format!("m{}:\n", entries.len()).as_bytes());
            for (key, value) in entries {
                out.extend_from_slice(&key);
                encode(value, out);
            }
        }
        Value::Tagged(tagged) => {
            // A YAML tag (`!Secret foo`) changes what a value means to whoever
            // resolves it. Dropping it would let two documents that a later
            // reader treats differently share one approval.
            out.extend_from_slice(b"g");
            encode_string(&tagged.tag.to_string(), out);
            encode(&tagged.value, out);
        }
    }
}

/// `s<byte length>:<bytes>\n` — length-prefixed, so concatenation is not
/// ambiguous (`approval::binding` prefixes for the same reason).
fn encode_string(text: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(format!("s{}:", text.len()).as_bytes());
    out.extend_from_slice(text.as_bytes());
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_order_and_indentation_do_not_reach_the_digest() {
        let a = "plan:\n  id: p-x\n  version: 1\n";
        let b = "plan:\n    version: 1\n    id: 'p-x'\n";
        assert_eq!(content_hash(a).expect("a"), content_hash(b).expect("b"));
    }

    #[test]
    fn a_comment_does_not_reach_the_digest() {
        let a = "plan:\n  id: p-x\n  version: 1\n";
        let b =
            "# load-bearing prose in the wrong place\nplan:\n  id: p-x # here too\n  version: 1\n";
        assert_eq!(content_hash(a).expect("a"), content_hash(b).expect("b"));
    }

    #[test]
    fn a_changed_value_reaches_the_digest() {
        let a = "plan:\n  id: p-x\n  version: 1\n";
        let b = "plan:\n  id: p-x\n  version: 2\n";
        assert_ne!(content_hash(a).expect("a"), content_hash(b).expect("b"));
    }

    #[test]
    fn a_string_and_a_number_that_print_alike_do_not_hash_alike() {
        // The whole reason the canonical form is tagged rather than pretty
        // YAML. Both of these reserialize to `version: 1`.
        let number = "plan:\n  id: p-x\n  version: 1\n";
        let string = "plan:\n  id: p-x\n  version: \"1\"\n";
        assert_ne!(
            content_hash(number).expect("number"),
            content_hash(string).expect("string")
        );
    }

    #[test]
    fn concatenation_is_not_ambiguous() {
        // Without length prefixes these two encode to the same bytes.
        let a = "plan: {id: ab, version: 1, x: c}";
        let b = "plan: {id: a, version: 1, x: bc}";
        assert_ne!(content_hash(a).expect("a"), content_hash(b).expect("b"));
    }

    #[test]
    fn sequence_order_is_content_and_reaches_the_digest() {
        let a = "plan: {id: p-x, version: 1, xs: [1, 2]}";
        let b = "plan: {id: p-x, version: 1, xs: [2, 1]}";
        assert_ne!(content_hash(a).expect("a"), content_hash(b).expect("b"));
    }

    #[test]
    fn the_canonical_form_names_its_own_version() {
        let bytes = canonical_bytes("plan: {id: p-x, version: 1}").expect("bytes");
        assert!(bytes.starts_with(CANONICAL_VERSION.as_bytes()));
    }

    #[test]
    fn text_that_is_not_yaml_has_no_canonical_form() {
        assert!(canonical_bytes("\tnope: [").is_err());
    }
}
