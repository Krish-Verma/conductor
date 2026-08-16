//! `.conductor/project.yaml` — master plan §3.1, §4.2, §5.1.
//!
//! # What this file is authoritative for
//!
//! §3.1's tree names it in one line: *"identity, adapter, scope defaults, review
//! cadence, execution_requirements"*. It is the git half of §3.1's split for
//! everything about the project that is not a plan, a policy or a verification
//! profile, and §5.1's `project` row is its index — `id`, `default_branch` and
//! `config_hash` are all read from here.
//!
//! # A missing file is a refusal, not a default project
//!
//! [`load`] has no "carry on with defaults" path, for the same reason
//! [`super::model::PlanError`] has none: the default project is the invented
//! one. It would have to invent an id — which every `plan_version`, `task` and
//! `run` row then hangs off (§5.1's foreign keys) — a default branch, which
//! decides what a run integrates into, and an adapter, which decides *which
//! agent runs*. Each of those is a decision a human makes once, in a file they
//! can diff (§3.2's second reason).
//!
//! # Two top-level keys, and why the file is not flat
//!
//! ```yaml
//! project:
//!   id: p-conductor
//!   default_branch: main
//!   adapter: codex
//!   scope_defaults: { allowed_globs: [...], forbidden_globs: [".conductor/**"] }
//!   review_cadence: { ... }
//! execution_requirements:
//!   filesystem_write: restricted
//!   control_surface:  hard
//! ```
//!
//! The `project:` wrapper matches `.conductor/policy.yaml`'s `policy:`,
//! `.conductor/verification.yaml`'s `verification:` and `plan.yaml`'s `plan:`,
//! so an author reading the four files sees one convention —
//! [`super::model::PlanDocument`] gives that argument for the plan.
//!
//! `execution_requirements:` is deliberately **outside** it, at column 0,
//! because that is exactly where §4.2 draws it: its worked example is captioned
//! *"`.conductor/project.yaml`, or per-task override"* and puts the key at the
//! file's top level. Nesting it under `project:` was considered and rejected —
//! it would make the master plan's own example invalid in the file the master
//! plan names it for, and it would make the project default and the per-task
//! override two different dialects when §4.2 writes them as one.
//!
//! # `execution_requirements` and `review_cadence` are carried as text
//!
//! Both are stored as the YAML block that declares them, re-serialized under
//! its own key, and for two different reasons.
//!
//! `execution_requirements` is text so that a project default and
//! [`super::model::Task::execution_requirements`] are *the same value shape*,
//! readable by the one parser that already owns §4.2's dialect —
//! [`ExecutionRequirements::parse_yaml`]. That field's doc comment gives the
//! rule: a second parser is a second place the two can drift apart.
//!
//! `review_cadence` is text because **S13 owns the vocabulary**. The master
//! plan's slice list puts *"review cadence config"* and *"boundary detection"*
//! in S13's scope and nowhere else; there is no section that names a single
//! cadence key. Inventing keys here would be a schema S13 must then either
//! honour or break, which is the speculative abstraction CLAUDE.md forbids.
//! Carrying the block verbatim keeps the author's declaration, keeps it inside
//! [`Project::config_hash`], and commits to nothing.
//!
//! # Unknown keys load; `config_hash` is what closes the hole
//!
//! Same rule as the plan, and the same reason
//! ([`super::model`]'s "No `deny_unknown_fields`"): a project file written for a
//! later Conductor must still load on this one, or §3.2's *"travel with the
//! repository to another machine"* is false. The hole that opens — an ignored
//! key is a key an agent can add for free, and §3.3 gives it write access to
//! `.conductor/` — is closed the same way [`super::hash`] closes it for plans:
//! [`Project::config_hash`] is taken over the **whole document**, so a key this
//! version does not model still moves the digest.

use serde::Deserialize;
use serde_yaml::Value;

use super::hash::canonical_bytes;
use super::model::TaskScope;
use crate::policy::eligibility::ExecutionRequirements;

/// Where a project's configuration lives, relative to the repository root —
/// §3.1.
pub const PROJECT_CONFIG_PATH: &str = ".conductor/project.yaml";

/// Domain separator for [`Project::config_hash`].
///
/// The canonical encoding it wraps is [`canonical_bytes`], which is the plan
/// module's — reused rather than re-implemented, because a second YAML
/// canonicalizer is a second thing that can disagree with the first about what
/// two documents mean. That encoding tags itself
/// `conductor.plan.canonical.v1`, so without an outer separator a project
/// configuration and a plan would share one digest space. They must not: §5.1
/// stores them in different columns for different purposes, and a value that
/// could be read as either is a value that can be substituted for the other.
pub const CONFIG_HASH_DOMAIN: &str = "conductor.project.config.v1";

/// Anything that stops `.conductor/project.yaml` from loading.
///
/// Every variant is a refusal. See this module's docs for why there is no
/// default project.
#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    /// The file could not be read. Includes "it is not there", which is the
    /// case §3.1 makes a hard error.
    #[error("project config at {path}: {source}")]
    Io {
        /// The path that was wanted.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The YAML did not parse.
    #[error("project config is not valid YAML: {0}")]
    Yaml(String),
    /// The YAML parsed but does not describe a project.
    #[error("project config is invalid: {0}")]
    Invalid(String),
    /// A field that addresses something is blank.
    #[error(
        "project config field `{field}` is blank; it names {names}, and a blank \
         name addresses nothing"
    )]
    Blank {
        /// Which field.
        field: &'static str,
        /// What a value in it would have identified.
        names: &'static str,
    },
    /// §4.2's block is present and cannot be read as a requirement.
    ///
    /// Refused rather than read as "no requirements", on
    /// `enforce::launch::requirements_for`'s reasoning: present-and-meaningless
    /// is what a mis-nested override looks like, and a mis-nested override must
    /// not silently read as "nothing is gated".
    #[error(
        "project config declares `execution_requirements` that cannot be read as \
         a §4.2 requirement: {detail}; a block that gates nothing is refused \
         rather than read as \"nothing is gated\" — fix it, or remove the key"
    )]
    ExecutionRequirements {
        /// The parser's complaint, or why an empty result is dangerous here.
        detail: String,
    },
}

/// `.conductor/project.yaml`, as loaded.
///
/// # Why one field is private
///
/// [`config_hash`](Project::config_hash) is a digest of the *text this value
/// came from*, and it covers keys this version does not model — so it cannot be
/// recomputed from the other fields. A public field would let a caller build a
/// `Project` whose digest describes a document that never existed. Keeping it
/// private also makes [`parse`] and [`load`] the only ways to obtain a
/// `Project` at all, which is the same discipline
/// [`super::ValidatedPlan`](crate::plan::ValidatedPlan) uses: "where did this
/// come from?" is answered by the type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// `project.id` — §5.1's `p-<short>`. Constant for the life of the
    /// repository; changing it re-registers the project (§3.5's recovery path).
    pub id: String,
    /// `project.default_branch` — what a run integrates into.
    pub default_branch: String,
    /// Which §6.1 adapter runs this project's tasks.
    pub adapter: String,
    /// §3.1's "scope defaults" — the [`TaskScope`] a task inherits when it
    /// declares none.
    ///
    /// The same type as a task's own `scope:`, deliberately: §6.5 draws one
    /// scope block, and a project-level copy with different field names would
    /// be a second spelling of one idea. As there, the globs are **not**
    /// matched here — §3.7's "scope globs matching no path" needs a working
    /// tree.
    pub scope_defaults: TaskScope,
    /// §4.2's `execution_requirements:` block, in the shape
    /// [`ExecutionRequirements::parse_yaml`] reads and
    /// [`super::model::Task::execution_requirements`] stores. `None` when the
    /// file declares none, which is not the same case as a block that gates
    /// nothing — that one is [`ProjectError::ExecutionRequirements`].
    pub execution_requirements: Option<String>,
    /// §3.1's "review cadence", carried verbatim because S13 owns its
    /// vocabulary — see this module's docs. `None` when the file declares none.
    pub review_cadence: Option<String>,
    config_hash: String,
}

impl Project {
    /// `project.config_hash` (§5.1) — `blake3:<hex>` over the canonical form of
    /// the **whole** document.
    ///
    /// Whole document, not the modelled subset, for exactly [`super::hash`]'s
    /// reason: a key this version ignores must still move the digest, or an
    /// agent that can write `.conductor/` (§3.3) can change the configuration
    /// a later Conductor honours without changing the value that records what
    /// the configuration was.
    ///
    /// Covers `project.yaml` and nothing else. `policy.yaml` has
    /// `run.policy_hash` and `verification.yaml` has its own toolchain
    /// fingerprint; folding three files into one digest would make each of
    /// them invalidate the other two's records for no gain.
    pub fn config_hash(&self) -> &str {
        &self.config_hash
    }
}

/// The modelled half of the `project:` block.
///
/// A derive rather than a hand-walked [`Value`], on
/// [`super::model::parse`]'s reasoning: `policy::load` walks by hand because it
/// must refuse unknown keys and `verify::profile` walks by hand because it must
/// *report* them, and this file does neither — an unknown key here loads and is
/// inert. A derive keeps the field list in one place instead of two.
#[derive(Debug, Deserialize)]
struct ProjectDocument {
    project: ProjectBody,
}

#[derive(Debug, Deserialize)]
struct ProjectBody {
    id: String,
    default_branch: String,
    adapter: String,
    #[serde(default)]
    scope_defaults: TaskScope,
}

/// Read a project configuration from a repository root.
///
/// Takes the **root**, not the file, so that §3.1's layout is encoded in one
/// place — the same argument [`super::plan_path`] makes for plans.
pub fn load(repo_root: &std::path::Path) -> Result<Project, ProjectError> {
    let path = repo_root.join(PROJECT_CONFIG_PATH);
    let text = std::fs::read_to_string(&path).map_err(|source| ProjectError::Io {
        path: path.clone(),
        source,
    })?;
    parse(&text)
}

/// Parse a project configuration.
pub fn parse(yaml: &str) -> Result<Project, ProjectError> {
    let root: Value = serde_yaml::from_str(yaml).map_err(|e| ProjectError::Yaml(e.to_string()))?;
    let document: ProjectDocument = serde_yaml::from_value(root.clone()).map_err(|error| {
        let text = error.to_string();
        // serde reports "this is not YAML" and "this YAML is not a project"
        // through one type. Splitting them matters to whoever reads the
        // error: one is a syntax mistake, the other is a shape mistake —
        // `plan::model::parse` splits them the same way.
        if text.contains("missing field") || text.contains("invalid type") {
            ProjectError::Invalid(text)
        } else {
            ProjectError::Yaml(text)
        }
    })?;

    let body = document.project;
    require_named(
        "id",
        "the project every plan version and run hangs off",
        &body.id,
    )?;
    require_named(
        "default_branch",
        "the branch a completed run integrates into",
        &body.default_branch,
    )?;
    require_named(
        "adapter",
        "which coding agent runs this project's tasks",
        &body.adapter,
    )?;

    // §4.2 draws `execution_requirements:` at the file's top level; §3.1 groups
    // review cadence with identity, so it is read from inside `project:`. See
    // this module's docs for why the file has two top-level keys rather than
    // one.
    let execution_requirements = block(&root, "execution_requirements");
    if let Some(text) = &execution_requirements {
        check_execution_requirements(text)?;
    }
    let review_cadence = child(&root, "project").and_then(|body| block(body, "review_cadence"));

    Ok(Project {
        id: body.id,
        default_branch: body.default_branch,
        adapter: body.adapter,
        scope_defaults: body.scope_defaults,
        execution_requirements,
        review_cadence,
        config_hash: config_hash(yaml)?,
    })
}

/// Refuse a blank value in a field whose whole job is to name something.
///
/// Same predicate and the same reasoning as
/// [`crate::plan::PlanDefect::EmptyId`]: an id is assigned once and never
/// reused, and a blank one addresses nothing.
fn require_named(
    field: &'static str,
    names: &'static str,
    value: &str,
) -> Result<(), ProjectError> {
    if value.trim().is_empty() {
        return Err(ProjectError::Blank { field, names });
    }
    Ok(())
}

/// The node under `key`, if `value` is a mapping that has one.
fn child<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_mapping()?.get(Value::from(key))
}

/// One declared block, re-serialized under its own key.
///
/// The key travels with the block so the result is a self-describing document
/// rather than a fragment: [`ExecutionRequirements::parse_yaml`] looks for a
/// top-level `execution_requirements:` mapping, and a `review_cadence` block
/// that arrived at S13 without its own name would be a mapping nobody could
/// tell from any other.
///
/// `None` when the key is absent. A key that is present but empty is **not**
/// `None` — it round-trips as `key: null`, which is a declaration that says
/// nothing, and the caller decides whether saying nothing is allowed.
fn block(value: &Value, key: &str) -> Option<String> {
    let node = child(value, key)?;
    let mut wrapper = serde_yaml::Mapping::new();
    wrapper.insert(Value::from(key), node.clone());
    // Infallible in practice: `wrapper` came out of a parsed document, so every
    // value in it already serialized once. A failure would mean serde_yaml
    // cannot write what it just read, and dropping the block on the floor there
    // would silently disarm a requirement — so it is reported as an absent key
    // only if the round trip is impossible, and the caller's own check refuses
    // an unreadable block.
    serde_yaml::to_string(&Value::Mapping(wrapper)).ok()
}

/// §4.2's block must gate something, or say nothing at all.
fn check_execution_requirements(text: &str) -> Result<(), ProjectError> {
    match ExecutionRequirements::parse_yaml(text) {
        Ok(parsed) if !parsed.is_empty() => Ok(()),
        Ok(_) => Err(ProjectError::ExecutionRequirements {
            detail: "the key is declared but names no dimension, so nothing would be gated"
                .to_string(),
        }),
        Err(error) => Err(ProjectError::ExecutionRequirements {
            detail: error.to_string(),
        }),
    }
}

/// `blake3(CONFIG_HASH_DOMAIN ‖ canonical_bytes(text))`, length-prefixed.
///
/// The `‖` is spelled the way [`crate::approval::binding`] spells it and for
/// the reason [`super::hash`] gives: plain concatenation makes `("ab", "c")`
/// and `("a", "bc")` one preimage, and a digest whose entire job is to say
/// "this configuration, not that one" cannot afford that.
fn config_hash(yaml: &str) -> Result<String, ProjectError> {
    let canonical = canonical_bytes(yaml).map_err(|error| ProjectError::Yaml(error.to_string()))?;
    let mut hasher = blake3::Hasher::new();
    absorb(&mut hasher, CONFIG_HASH_DOMAIN.as_bytes());
    absorb(&mut hasher, &canonical);
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// Length-prefixed absorption: `<byte length> 0x1f <bytes>`, the encoding
/// [`crate::approval::binding`] uses.
fn absorb(hasher: &mut blake3::Hasher, component: &[u8]) {
    hasher.update(component.len().to_string().as_bytes());
    hasher.update(&[0x1f]);
    hasher.update(component);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "project:\n  id: p-x\n  default_branch: main\n  adapter: codex\n";

    #[test]
    fn a_project_declaring_only_the_three_required_fields_loads() {
        let project = parse(MINIMAL).expect("parses");
        assert_eq!(project.id, "p-x");
        assert_eq!(project.default_branch, "main");
        assert_eq!(project.adapter, "codex");
        assert!(project.scope_defaults.allowed_globs.is_empty());
        assert_eq!(project.execution_requirements, None);
        assert_eq!(project.review_cadence, None);
        assert!(project.config_hash().starts_with("blake3:"));
    }

    #[test]
    fn a_project_with_no_adapter_is_a_shape_error_and_not_a_syntax_error() {
        let error = parse("project:\n  id: p-x\n  default_branch: main\n").expect_err("refused");
        assert!(matches!(error, ProjectError::Invalid(_)), "{error}");
    }

    #[test]
    fn the_execution_requirements_block_round_trips_into_the_dialect_that_reads_it() {
        // The property that makes the project default and the per-task override
        // interchangeable: whatever this field holds, §4.2's own parser reads.
        let yaml = format!("{MINIMAL}execution_requirements:\n  control_surface: hard\n");
        let project = parse(&yaml).expect("parses");
        let text = project.execution_requirements.expect("declared");
        let requirements =
            ExecutionRequirements::parse_yaml(&text).expect("§4.2's parser reads it");
        assert_eq!(
            requirements.get(conductor_core::containment::GatingDimension::ControlSurface),
            Some(conductor_core::containment::Enforcement::Hard)
        );
    }

    #[test]
    fn a_review_cadence_block_is_carried_without_being_interpreted() {
        // S13 owns the vocabulary; this module must neither define it nor lose
        // it. The assertion is that the author's declaration survives, not that
        // it means anything yet.
        let yaml = format!("{MINIMAL}  review_cadence:\n    boundary: milestone\n");
        let project = parse(&yaml).expect("parses");
        let cadence = project.review_cadence.expect("declared");
        assert!(cadence.contains("review_cadence"), "{cadence}");
        assert!(cadence.contains("milestone"), "{cadence}");
    }

    #[test]
    fn a_declared_execution_requirements_block_that_names_no_dimension_is_refused() {
        // Mirrors `plan::validate`'s `malformed_execution_requirements` and
        // `enforce::launch::requirements_for`: present and meaningless is what
        // a mis-nested block looks like, and must not read as "nothing is
        // gated".
        let yaml = format!("{MINIMAL}execution_requirements:\n");
        let error = parse(&yaml).expect_err("refused");
        assert!(
            matches!(error, ProjectError::ExecutionRequirements { .. }),
            "{error}"
        );
    }

    #[test]
    fn the_config_hash_is_over_semantics_and_covers_keys_this_version_ignores() {
        let reformatted = "project:\n    adapter:  codex\n    id:   'p-x'\n    \
                           default_branch: \"main\"\n";
        assert_eq!(
            parse(MINIMAL).expect("control").config_hash(),
            parse(reformatted).expect("reformatted").config_hash(),
            "reformatting is not a configuration change"
        );
        let extended = format!("{MINIMAL}  a_later_conductors_key: true\n");
        assert_ne!(
            parse(MINIMAL).expect("control").config_hash(),
            parse(&extended).expect("extended").config_hash(),
            "a key this version ignores must still reach the digest"
        );
    }

    #[test]
    fn a_project_config_and_a_plan_never_share_a_digest() {
        // The whole reason for `CONFIG_HASH_DOMAIN`. Both encodings run over
        // the same canonical bytes; only the outer separator keeps them apart.
        let text = "project:\n  id: p-x\n  default_branch: main\n  adapter: codex\n";
        assert_ne!(
            parse(text).expect("project").config_hash(),
            super::super::content_hash(text)
                .expect("plan hash")
                .as_str()
        );
    }
}
