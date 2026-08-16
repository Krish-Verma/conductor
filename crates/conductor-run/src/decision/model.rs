//! `.conductor/decisions/D-*.md` — one Markdown-with-YAML-frontmatter
//! document, parsed and content-hashed — master plan §3.1, §3.6, §5.1 (slice
//! S11 task 5).
//!
//! # A decision is an argument, and the format says so
//!
//! §3.6 draws the plan/decision split in one sentence, twice: *"A **plan is a
//! data structure**"*, so plans are canonical YAML; a *"**decision is an
//! argument**"*, so decisions are *"[s]mall fixed metadata (`id`, `status`,
//! `supersedes`, `date`) in schema-validated frontmatter; the value is prose a
//! human reads and a packet quotes."* This module is that sentence, typed:
//! [`Decision`] carries exactly those four metadata fields plus [`Decision::body`],
//! and nothing here interprets the prose. S12's review packets quote it
//! verbatim, so a parser that reformatted, trimmed or re-wrapped it would be
//! quoting a document that never existed — see "The body is preserved
//! byte-for-byte" below.
//!
//! # Schema-validated, unlike the plan
//!
//! [`crate::plan::model`] has no `deny_unknown_fields`, on purpose: a plan
//! document written for a later Conductor must still load on this one,
//! because §3.2 requires an approved plan to travel with the repository to
//! another machine. That reasoning does not carry over here. §3.6 calls the
//! frontmatter *"[s]mall fixed"* — four named fields, not an extensible
//! document — and a decision has no equivalent of the plan's content-hash
//! safety net for an unmodelled key (`plan::hash::content_hash` hashes the
//! *whole* parsed document precisely because an ignored key would otherwise be
//! free to an agent with `.conductor/` write access). A `supersedes:` typo'd
//! as `superseeds:` would silently do nothing under a permissive parser, and
//! the supersession chain — the one thing that makes "append-only decisions"
//! true — would quietly break with no error to say so. So [`Frontmatter`] is
//! `#[serde(deny_unknown_fields)]`: an unknown key here is not tomorrow's
//! business, it is today's bug.
//!
//! # `status` is validated, not obeyed
//!
//! The frontmatter's `status:` field is checked against
//! [`conductor_store::DecisionStatus`] — Ruling 10's *"the decision status
//! machine is S11's invention and is already written"*, reused rather than
//! duplicated, the way `plan::model` reuses `conductor_core::PlanVersionState`
//! instead of carrying its own copy (see that module's "why there is no
//! `state:` field"). But *validating* the field is not the same as *acting on
//! it*, and this module deliberately stops at validation. §3.3's argument
//! against a plan carrying its own `state:` — a file an agent can write must
//! not become a second, easier way to reach a state the store's own machine
//! exists to gate — applies here too: if loading a decision pushed its
//! declared `status` straight into `decision.status`, then hand-editing
//! `status: ACCEPTED` into a committed Markdown file would *be* the mechanism
//! for accepting a decision, bypassing whatever human-mediated act is actually
//! supposed to decide one. `conductor_store::ledger::upsert_decision` already
//! enforces the other half of this: it inserts a decision `OPEN` and *never*
//! writes `status` again, on any resync. This module's own
//! `crate::decision::register_decisions` drives exactly one edge of the
//! store's machine (`→ SUPERSEDED`, via a *different* decision's
//! `supersedes:`, never a decision's own claim about itself) and leaves
//! `ACCEPTED`/`REJECTED` to whatever mechanism a later task wires up. A
//! decision's frontmatter `status` is therefore carried on [`Decision`] as a
//! fact about the document — what its author currently claims, useful to a
//! reader and to a future packet — never as an instruction this module
//! executes.
//!
//! # The content hash reuses the plan's canonicalizer
//!
//! *"Content hash over canonical frontmatter + body, same digest discipline as
//! the plan (ADR-0007: blake3 is the only digest)."* [`Decision::content_hash`]
//! is `blake3(DECISION_HASH_DOMAIN ‖ canonical_bytes(frontmatter) ‖ body)`,
//! length-prefixed exactly as [`crate::approval::binding`] and
//! `plan::hash::content_hash` absorb their own components, so `("ab", "c")`
//! and `("a", "bc")` cannot share a preimage. `canonical_bytes` is
//! [`crate::plan::canonical_bytes`] itself, **reused rather than
//! re-implemented** — `plan::project::Project::config_hash` makes the same
//! call for the same reason: a second YAML canonicalizer is a second thing
//! that can disagree with the first about what two documents mean.
//! `DECISION_HASH_DOMAIN` keeps a decision's digest out of a plan's or a
//! project's digest space, the way `CONFIG_HASH_DOMAIN` keeps a project's out
//! of a plan's.
//!
//! The body is hashed as raw bytes, not canonicalized — it is prose, not
//! data, and §3.6 draws no canonical form for prose. Only the frontmatter goes
//! through `canonical_bytes`, so re-indenting the four metadata fields does
//! not move the hash, but reformatting the argument itself does: reformatting
//! an argument can change what it says in a way reformatting `version: 1`
//! cannot, so the safety `plan::hash` grants to layout is deliberately not
//! extended to prose.
//!
//! # The body is preserved byte-for-byte
//!
//! No trimming, no re-wrapping, no normalising interior whitespace. §3.6 says
//! the body is *"the value a packet quotes"*, and a quote that silently edited
//! its source on the way in is not a quote.

use serde::Deserialize;
use thiserror::Error;

use conductor_store::DecisionStatus;

use crate::plan;

/// Domain separator for [`Decision::content_hash`] — see this module's docs
/// for why a decision, a plan and a project must never share a digest space.
pub const DECISION_HASH_DOMAIN: &str = "conductor.decision.content.v1";

/// Anything that stops a decision document from loading.
///
/// Every variant is a refusal, on `plan::model::PlanError`'s reasoning: there
/// is no partial decision, because a document missing its `id` or carrying an
/// unrecognised `status` is not a decision with defaults filled in, it is a
/// different document.
///
/// No variant carries a path: [`parse`] is pure over text and has no path to
/// report, the same split `plan::model::parse` draws — path context, when a
/// caller wants it, is added by whoever did the reading
/// (`crate::decision::load_all`).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecisionError {
    /// The frontmatter is not valid YAML.
    #[error("decision frontmatter is not valid YAML: {0}")]
    Yaml(String),
    /// The frontmatter is valid YAML but does not match the schema — missing
    /// a required field, or carrying one §3.6 does not name.
    #[error("decision frontmatter is invalid: {0}")]
    Invalid(String),
    /// No `---`-delimited frontmatter block was found at all.
    #[error(
        "the document has no YAML frontmatter; §3.6 requires \"small fixed \
         metadata … in schema-validated frontmatter\", delimited by `---` \
         lines at the start of the file, and none was found"
    )]
    MissingFrontmatter,
    /// `status:` is not one of the four the store's machine defines.
    #[error(
        "decision declares status {status:?}, which is not one of OPEN, \
         ACCEPTED, REJECTED, SUPERSEDED"
    )]
    UnknownStatus {
        /// What the file said.
        status: String,
    },
    /// A field that addresses something — currently only `id` — is blank.
    #[error(
        "decision frontmatter field `{field}` is blank; it names {names}, \
         and a blank name addresses nothing"
    )]
    Blank {
        /// Which field.
        field: &'static str,
        /// What a value in it would have identified.
        names: &'static str,
    },
}

/// One `.conductor/decisions/D-*.md` document, parsed.
///
/// `content_hash` is private; see `plan::project::Project`'s doc comment for
/// why — the same argument holds verbatim: a public field would let a caller
/// build a `Decision` whose digest describes a document that never existed,
/// so [`parse`] is the only way to obtain the hash. `source_path` is plain and
/// public — provenance metadata, not something the hash guards — and is
/// empty until `crate::decision::load_all` sets it; see [`parse`]'s docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// `decision.id` — `D-0007`. Stable, assigned once (§3.6).
    pub id: String,
    /// `decision.status`'s spelling in the document — validated against the
    /// store's own machine, but **not obeyed**; see this module's docs.
    pub status: DecisionStatus,
    /// When the decision was made. Carried as text; §3.6 draws no format for
    /// it and no reader in this slice needs one.
    pub date: String,
    /// `decision.supersedes` — the decision this one replaces, if any.
    pub supersedes: Option<String>,
    /// The argument. Preserved byte-for-byte — see this module's docs.
    pub body: String,
    /// Where this decision was read from, relative to the repository root —
    /// `.conductor/decisions/D-0007-clone-not-worktree.md`. Empty until
    /// [`crate::decision::load_all`] sets it: `parse` is pure over text and
    /// has no path to report.
    pub source_path: String,
    content_hash: String,
}

impl Decision {
    /// `blake3:<hex>` over the canonical frontmatter and the raw body — see
    /// this module's docs for the exact preimage.
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }
}

/// Parse one decision document.
///
/// Pure over `text`: no path, no filesystem. `crate::decision::load_all` does
/// the reading and attaches [`Decision::source_path`] afterwards — the same
/// split `plan::ledger` draws between `plan::model::parse` (the document) and
/// the caller that knows where it came from.
pub fn parse(text: &str) -> Result<Decision, DecisionError> {
    let (frontmatter_yaml, body) = split_frontmatter(text)?;
    let frontmatter: Frontmatter = serde_yaml::from_str(frontmatter_yaml).map_err(|error| {
        let text = error.to_string();
        // Same split `plan::model::parse` and `plan::project::parse` both
        // make: "this is not YAML" and "this YAML is not a decision" are
        // different mistakes for the reader. `deny_unknown_fields` reports an
        // unknown key as "unknown field", which is a shape mistake, not a
        // syntax one.
        if text.contains("missing field")
            || text.contains("invalid type")
            || text.contains("unknown field")
        {
            DecisionError::Invalid(text)
        } else {
            DecisionError::Yaml(text)
        }
    })?;

    require_named(
        "id",
        "the decision every supersedes chain and every packet quote hangs off",
        &frontmatter.id,
    )?;
    let status =
        frontmatter
            .status
            .parse::<DecisionStatus>()
            .map_err(|_| DecisionError::UnknownStatus {
                status: frontmatter.status.clone(),
            })?;

    let content_hash = content_hash(frontmatter_yaml, body);

    Ok(Decision {
        id: frontmatter.id,
        status,
        date: frontmatter.date,
        supersedes: frontmatter.supersedes,
        body: body.to_string(),
        source_path: String::new(),
        content_hash,
    })
}

/// The frontmatter's fixed four-field schema (§3.6). `deny_unknown_fields` —
/// see this module's docs for why, unlike `plan::model::PlanDocument`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    id: String,
    status: String,
    date: String,
    #[serde(default)]
    supersedes: Option<String>,
}

/// Split `---\n<frontmatter>\n---\n<body>`.
///
/// Both delimiter lines must be exactly `---`, so the boundary between
/// metadata and prose is unambiguous. A document with no opening delimiter, or
/// no closing one, is [`DecisionError::MissingFrontmatter`] rather than "no
/// frontmatter found, treat the whole file as the body" — a decision that
/// silently lost its `id` and `status` to a typo'd delimiter must not load as
/// if it declared none.
fn split_frontmatter(text: &str) -> Result<(&str, &str), DecisionError> {
    let after_open = text
        .strip_prefix("---\n")
        .ok_or(DecisionError::MissingFrontmatter)?;
    const CLOSE: &str = "\n---\n";
    let at = after_open
        .find(CLOSE)
        .ok_or(DecisionError::MissingFrontmatter)?;
    Ok((&after_open[..at], &after_open[at + CLOSE.len()..]))
}

/// Refuse a blank value in a field whose whole job is to name something — the
/// same predicate `plan::project::require_named` uses, for the same reason.
fn require_named(
    field: &'static str,
    names: &'static str,
    value: &str,
) -> Result<(), DecisionError> {
    if value.trim().is_empty() {
        return Err(DecisionError::Blank { field, names });
    }
    Ok(())
}

/// `blake3(DECISION_HASH_DOMAIN ‖ canonical_bytes(frontmatter) ‖ body)`,
/// length-prefixed — see this module's docs.
///
/// Infallible in practice: `frontmatter_yaml` has already parsed once (as
/// [`Frontmatter`], above) by the time this runs, so
/// `plan::canonical_bytes`'s only failure mode — invalid YAML — cannot occur
/// on text already known to be valid YAML.
fn content_hash(frontmatter_yaml: &str, body: &str) -> String {
    let canonical = plan::canonical_bytes(frontmatter_yaml)
        .expect("frontmatter_yaml already parsed as YAML above");
    let mut hasher = blake3::Hasher::new();
    absorb(&mut hasher, DECISION_HASH_DOMAIN.as_bytes());
    absorb(&mut hasher, &canonical);
    absorb(&mut hasher, body.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Length-prefixed absorption: `<byte length> 0x1f <bytes>` —
/// [`crate::approval::binding`]'s encoding, for the reason it gives: plain
/// concatenation makes `("ab", "c")` and `("a", "bc")` one preimage.
fn absorb(hasher: &mut blake3::Hasher, component: &[u8]) {
    hasher.update(component.len().to_string().as_bytes());
    hasher.update(&[0x1f]);
    hasher.update(component);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "---\nid: D-0001\nstatus: OPEN\ndate: 2026-08-15\n---\nThe argument.\n";

    #[test]
    fn a_well_formed_decision_parses_with_its_prose_intact() {
        // POSITIVE CONTROL.
        let decision = parse(MINIMAL).expect("parses");
        assert_eq!(decision.id, "D-0001");
        assert_eq!(decision.status, DecisionStatus::Open);
        assert_eq!(decision.date, "2026-08-15");
        assert_eq!(decision.supersedes, None);
        assert_eq!(decision.body, "The argument.\n");
        assert!(decision.content_hash().starts_with("blake3:"));
    }

    #[test]
    fn a_document_with_no_frontmatter_is_refused() {
        let error = parse("Just prose, no frontmatter at all.\n").expect_err("refused");
        assert_eq!(error, DecisionError::MissingFrontmatter);
    }

    #[test]
    fn a_document_whose_frontmatter_never_closes_is_refused() {
        let error = parse("---\nid: D-0001\nstatus: OPEN\ndate: 2026-08-15\nno closer here\n")
            .expect_err("refused");
        assert_eq!(error, DecisionError::MissingFrontmatter);
    }

    #[test]
    fn an_unknown_status_is_refused_and_names_it() {
        let yaml = MINIMAL.replace("status: OPEN", "status: MAYBE");
        let error = parse(&yaml).expect_err("refused");
        assert!(error.to_string().contains("MAYBE"), "{error}");
        assert!(matches!(error, DecisionError::UnknownStatus { .. }));
    }

    #[test]
    fn an_unknown_frontmatter_key_is_refused_unlike_a_plans_unknown_key() {
        // The contrast this module's docs draw with `plan::model`: a plan
        // silently loads an unmodelled key; a decision does not.
        let yaml = MINIMAL.replace("date: 2026-08-15\n", "date: 2026-08-15\nauthor: krish\n");
        let error = parse(&yaml).expect_err("refused");
        assert!(matches!(error, DecisionError::Invalid(_)), "{error}");
    }

    #[test]
    fn a_blank_id_is_refused() {
        let yaml = MINIMAL.replace("id: D-0001", "id: \"  \"");
        let error = parse(&yaml).expect_err("refused");
        assert!(
            matches!(error, DecisionError::Blank { field: "id", .. }),
            "{error}"
        );
    }

    #[test]
    fn a_missing_required_field_is_a_shape_error_and_not_a_syntax_error() {
        let error = parse("---\nid: D-0001\nstatus: OPEN\n---\nno date\n").expect_err("refused");
        assert!(matches!(error, DecisionError::Invalid(_)), "{error}");
    }

    #[test]
    fn a_supersedes_field_round_trips() {
        let yaml = MINIMAL.replace(
            "date: 2026-08-15\n",
            "date: 2026-08-15\nsupersedes: D-0000\n",
        );
        let decision = parse(&yaml).expect("parses");
        assert_eq!(decision.supersedes, Some("D-0000".to_string()));
    }

    #[test]
    fn reformatting_the_frontmatter_does_not_change_the_content_hash() {
        let reformatted =
            "---\nstatus:   OPEN\nid:    'D-0001'\ndate: 2026-08-15\n---\nThe argument.\n";
        assert_eq!(
            parse(MINIMAL).expect("control").content_hash(),
            parse(reformatted).expect("reformatted").content_hash(),
        );
    }

    #[test]
    fn a_changed_body_changes_the_content_hash() {
        let changed = MINIMAL.replace("The argument.\n", "A different argument.\n");
        assert_ne!(
            parse(MINIMAL).expect("control").content_hash(),
            parse(&changed).expect("changed").content_hash(),
        );
    }

    #[test]
    fn a_changed_status_changes_the_content_hash() {
        let changed = MINIMAL.replace("status: OPEN", "status: ACCEPTED");
        assert_ne!(
            parse(MINIMAL).expect("control").content_hash(),
            parse(&changed).expect("changed").content_hash(),
        );
    }

    #[test]
    fn the_body_is_preserved_byte_for_byte_including_interior_whitespace() {
        let text = "---\nid: D-0001\nstatus: OPEN\ndate: 2026-08-15\n---\nFirst line.\n\n  \
                     Indented.\nTrailing spaces.   \n";
        let decision = parse(text).expect("parses");
        assert_eq!(
            decision.body,
            "First line.\n\n  Indented.\nTrailing spaces.   \n"
        );
    }

    #[test]
    fn a_decision_hash_domain_separator_actually_reaches_the_digest() {
        // `DECISION_HASH_DOMAIN` earns its place only if it is actually
        // absorbed — same check `plan::ledger`'s `REPO_IDENTITY_DOMAIN` test
        // makes for its own domain separator.
        let mut with_domain = blake3::Hasher::new();
        absorb(&mut with_domain, DECISION_HASH_DOMAIN.as_bytes());
        absorb(&mut with_domain, b"a");
        absorb(&mut with_domain, b"b");
        let mut without = blake3::Hasher::new();
        absorb(&mut without, b"a");
        absorb(&mut without, b"b");
        assert_ne!(with_domain.finalize(), without.finalize());
    }

    #[test]
    fn the_components_of_a_decision_hash_cannot_be_slid_past_one_another() {
        // Without a length prefix these two share a preimage.
        let hash = |a: &[u8], b: &[u8]| {
            let mut hasher = blake3::Hasher::new();
            absorb(&mut hasher, DECISION_HASH_DOMAIN.as_bytes());
            absorb(&mut hasher, a);
            absorb(&mut hasher, b);
            hasher.finalize()
        };
        assert_ne!(hash(b"ab", b"c"), hash(b"a", b"bc"));
    }

    #[test]
    fn a_decision_hash_and_a_plan_hash_never_share_a_digest_space() {
        // Both `content_hash` here and `plan::content_hash` run
        // `canonical_bytes` over the same text; only `DECISION_HASH_DOMAIN`
        // (wrapped around it) keeps a decision's digest out of a plan's — the
        // same property `plan::project`'s own cross-digest test asserts for
        // `CONFIG_HASH_DOMAIN`.
        let frontmatter = "id: D-0001\nstatus: OPEN\ndate: 2026-08-15\n";
        let decision_hash = content_hash(frontmatter, "");
        let plan_hash = plan::content_hash(frontmatter)
            .expect("valid yaml")
            .as_str()
            .to_string();
        assert_ne!(decision_hash, plan_hash);
    }
}
