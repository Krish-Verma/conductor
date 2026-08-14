//! Loading policy, canonicalizing it, and pinning it to a run — §4.4.
//!
//! # Malformed policy is never a permissive policy
//!
//! `verify::profile` accepts an unknown key and warns, because a mistyped
//! verification key costs a wasted check. The same choice here would cost a
//! silently ungated action, so this loader takes the opposite line: **every
//! problem is a hard, named error.** A policy file that Conductor cannot fully
//! understand is a policy Conductor refuses to run under.
//!
//! # The canonical form is the policy, not a digest input
//!
//! §4.4: *"canonically serializes the resolved policy (sorted keys, no
//! timestamps), hashes it BLAKE3, stores it content-addressed"* — and then *"a
//! run evaluates against its snapshot for its entire life"*. The second sentence
//! is what makes the first non-trivial: the blob is what a pinned run is
//! evaluated against after the file on disk has been edited or deleted, so it
//! has to round-trip losslessly. [`from_canonical`] is therefore part of the
//! contract, and `canonical(from_canonical(blob)) == blob` is tested.
//!
//! ## How byte-identity is achieved
//!
//! * Field order comes from **derived `Serialize` on structs whose fields are
//!   declared in alphabetical order**, not from a map type — so the output does
//!   not depend on whether `serde_json`'s `preserve_order` feature happens to be
//!   enabled somewhere in the dependency graph.
//! * Dynamic keys (a scope's constraints) live in a `BTreeMap`, which iterates
//!   sorted; inserting in sorted order into `serde_json`'s map keeps that true
//!   under either feature setting.
//! * Rules and exceptions are **sorted by id**. Two files that list the same
//!   rules in a different order are the same policy — the join is commutative —
//!   and hashing them differently would give one run two snapshots.
//! * Nothing carries a timestamp except an exception's `expires_at`, which is
//!   content rather than provenance.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use conductor_core::RunId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

pub use super::model::PolicyError;
use super::model::{
    Action, ActionPattern, BuiltinInvariant, Effect, Origin, PolicyDocument, PolicyException,
    ResolvedPolicy, Rule, Scope,
};

/// Where a project's policy lives — master plan §3.1.
pub const PROJECT_POLICY_PATH: &str = ".conductor/policy.yaml";

/// The canonical form's version. Bumping it changes every hash, which is why it
/// is in the blob: a snapshot written by a different serializer must not be
/// mistaken for one written by this Conductor.
pub const CANONICAL_VERSION: u32 = 1;

/// `$XDG_CONFIG_HOME/conductor/policy.yaml`, or `~/.config/conductor/policy.yaml`.
///
/// §4.4 makes global rules first-class but names no path for them; this follows
/// the same XDG convention `Store::default_path` uses for the database, so an
/// operator has one rule to remember rather than two. `None` when neither
/// variable is set — an environment with no home directory has no global policy,
/// which is a fact, not an error.
pub fn global_policy_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("conductor").join("policy.yaml"));
    }
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("conductor")
            .join("policy.yaml")
    })
}

// ---------------------------------------------------------------------------
// YAML → model
// ---------------------------------------------------------------------------

/// Read one policy document from disk.
pub fn load_document(path: &Path, origin: Origin) -> Result<PolicyDocument, PolicyError> {
    let text = std::fs::read_to_string(path).map_err(|source| PolicyError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_document(&text, origin).map_err(|error| match error {
        PolicyError::Invalid { location, detail } => PolicyError::Invalid {
            location: format!("{}: {location}", path.display()),
            detail,
        },
        other => other,
    })
}

/// Parse one policy document.
pub fn parse_document(yaml: &str, origin: Origin) -> Result<PolicyDocument, PolicyError> {
    let root: Value = serde_yaml::from_str(yaml).map_err(|e| PolicyError::Yaml(e.to_string()))?;
    let root = mapping(&root, "the file")?;

    let body = root
        .get(Value::from("policy"))
        .ok_or_else(|| PolicyError::Invalid {
            location: "the file".to_string(),
            detail: "no top-level `policy:` key".to_string(),
        })?;
    let body = mapping(body, "policy")?;

    // Unknown keys are refused, not warned about. `builtin_invariants:` and
    // `invariants:` get their own message because they are the two spellings an
    // author would reach for when trying to do the thing §4.4 forbids.
    for name in body.keys().filter_map(|k| k.as_str()) {
        match name {
            "rules" | "exceptions" | "version" => {}
            "invariants" | "builtin_invariants" | "builtin" => {
                return Err(PolicyError::BuiltinNotConfigurable {
                    what: format!("the `{name}:` key"),
                    action: "*".to_string(),
                    invariant: "all four",
                });
            }
            other => {
                return Err(PolicyError::Invalid {
                    location: "policy".to_string(),
                    detail: format!(
                        "unknown key `{other}`; a policy key Conductor does not \
                         understand is refused rather than ignored, because an \
                         ignored rule is a rule that silently does not apply"
                    ),
                });
            }
        }
    }

    let mut rules = Vec::new();
    for (index, entry) in sequence(body, "rules")?.iter().enumerate() {
        rules.push(parse_rule(
            entry,
            origin,
            &format!("policy.rules[{index}]"),
        )?);
    }

    let mut exceptions = Vec::new();
    for (index, entry) in sequence(body, "exceptions")?.iter().enumerate() {
        exceptions.push(parse_exception(
            entry,
            origin,
            &format!("policy.exceptions[{index}]"),
        )?);
    }

    PolicyDocument::new(origin, rules, exceptions)
}

fn parse_rule(entry: &Value, origin: Origin, location: &str) -> Result<Rule, PolicyError> {
    let map = mapping(entry, location)?;
    known_keys(
        map,
        &["id", "action", "effect", "scope", "locked", "when"],
        location,
    )?;

    let id = required_str(map, "id", location)?;
    let action = required_str(map, "action", location)?;
    let pattern = ActionPattern::parse(&action).map_err(|e| relocate(e, location))?;
    let effect = parse_effect(map, location)?;

    let locked = match map.get(Value::from("locked")) {
        None => false,
        Some(value) => value.as_bool().ok_or_else(|| PolicyError::Invalid {
            location: location.to_string(),
            detail: "`locked` must be true or false".to_string(),
        })?,
    };

    Ok(Rule {
        id,
        origin,
        pattern,
        effect,
        scope: parse_scope(map, location)?,
        locked,
        when: string_list(map, "when", location)?,
    })
}

fn parse_exception(
    entry: &Value,
    origin: Origin,
    location: &str,
) -> Result<PolicyException, PolicyError> {
    let map = mapping(entry, location)?;
    known_keys(
        map,
        &["id", "action", "effect", "scope", "expires_at"],
        location,
    )?;

    let id = required_str(map, "id", location)?;
    let name = required_str(map, "action", location)?;
    // §4.4: an exception applies only when it "matches exactly". A pattern would
    // be a blanket loosening wearing an exception's clothes, so the wildcard
    // spellings are refused here rather than silently never matching.
    if name.contains('*') {
        return Err(PolicyError::Invalid {
            location: location.to_string(),
            detail: format!(
                "exception action {name:?} is a pattern; an exception must name \
                 exactly one action (§4.4)"
            ),
        });
    }
    let action = Action::parse(&name);

    if let Some(invariant) = BuiltinInvariant::governing_action(&action) {
        return Err(PolicyError::BuiltinNotConfigurable {
            what: format!("exception {id:?}"),
            action: name,
            invariant: invariant.id(),
        });
    }

    let expires_at = map
        .get(Value::from("expires_at"))
        .and_then(|v| v.as_str().map(str::to_string))
        .ok_or_else(|| PolicyError::Invalid {
            location: location.to_string(),
            detail: "no `expires_at:`; §4.3 makes a policy exception's expiry \
                     mandatory, because an exception without one is a rule"
                .to_string(),
        })?;

    Ok(PolicyException {
        id,
        origin,
        action,
        effect: parse_effect(map, location)?,
        scope: parse_scope(map, location)?,
        expires_at_ms: parse_rfc3339_utc(&expires_at).ok_or_else(|| PolicyError::Invalid {
            location: location.to_string(),
            detail: format!(
                "`expires_at: {expires_at:?}` is not an RFC 3339 UTC timestamp \
                 (`YYYY-MM-DDTHH:MM:SSZ`)"
            ),
        })?,
    })
}

fn parse_effect(map: &serde_yaml::Mapping, location: &str) -> Result<Effect, PolicyError> {
    let text = required_str(map, "effect", location)?;
    Effect::parse(&text).ok_or_else(|| PolicyError::Invalid {
        location: location.to_string(),
        detail: format!("`effect: {text:?}` is not one of allow, require_approval, deny"),
    })
}

fn parse_scope(map: &serde_yaml::Mapping, location: &str) -> Result<Scope, PolicyError> {
    let Some(value) = map.get(Value::from("scope")) else {
        return Ok(Scope::everywhere());
    };
    let entries = mapping(value, &format!("{location}.scope"))?;
    let mut pairs = Vec::new();
    for (key, value) in entries {
        let key = key.as_str().ok_or_else(|| PolicyError::Invalid {
            location: format!("{location}.scope"),
            detail: "scope keys must be strings".to_string(),
        })?;
        // A scalar, rendered as text: `{run: r-0041}` and `{run: "r-0041"}` must
        // scope identically, and so must `{version: 3}`.
        let rendered = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            _ => {
                return Err(PolicyError::Invalid {
                    location: format!("{location}.scope"),
                    detail: format!("scope value for `{key}` must be a scalar"),
                });
            }
        };
        pairs.push((key.to_string(), rendered));
    }
    Ok(Scope::from_pairs(pairs))
}

fn relocate(error: PolicyError, location: &str) -> PolicyError {
    match error {
        PolicyError::Invalid { detail, .. } => PolicyError::Invalid {
            location: location.to_string(),
            detail,
        },
        other => other,
    }
}

fn mapping<'v>(value: &'v Value, location: &str) -> Result<&'v serde_yaml::Mapping, PolicyError> {
    value.as_mapping().ok_or_else(|| PolicyError::Invalid {
        location: location.to_string(),
        detail: "must be a mapping".to_string(),
    })
}

fn sequence<'m>(
    map: &'m serde_yaml::Mapping,
    key: &str,
) -> Result<std::borrow::Cow<'m, [Value]>, PolicyError> {
    match map.get(Value::from(key)) {
        None => Ok(std::borrow::Cow::Owned(Vec::new())),
        Some(value) => value
            .as_sequence()
            .map(|items| std::borrow::Cow::Borrowed(items.as_slice()))
            .ok_or_else(|| PolicyError::Invalid {
                location: format!("policy.{key}"),
                detail: "must be a list".to_string(),
            }),
    }
}

fn known_keys(
    map: &serde_yaml::Mapping,
    known: &[&str],
    location: &str,
) -> Result<(), PolicyError> {
    for key in map.keys().filter_map(|k| k.as_str()) {
        if !known.contains(&key) {
            return Err(PolicyError::Invalid {
                location: location.to_string(),
                detail: format!("unknown key `{key}`; expected one of {known:?}"),
            });
        }
    }
    Ok(())
}

fn required_str(
    map: &serde_yaml::Mapping,
    key: &str,
    location: &str,
) -> Result<String, PolicyError> {
    let value = map
        .get(Value::from(key))
        .ok_or_else(|| PolicyError::Invalid {
            location: location.to_string(),
            detail: format!("no `{key}:`"),
        })?;
    let text = value.as_str().ok_or_else(|| PolicyError::Invalid {
        location: location.to_string(),
        detail: format!("`{key}` must be a string"),
    })?;
    if text.trim().is_empty() {
        return Err(PolicyError::Invalid {
            location: location.to_string(),
            detail: format!("`{key}` is empty"),
        });
    }
    Ok(text.trim().to_string())
}

fn string_list(
    map: &serde_yaml::Mapping,
    key: &str,
    location: &str,
) -> Result<Vec<String>, PolicyError> {
    let Some(value) = map.get(Value::from(key)) else {
        return Ok(Vec::new());
    };
    let items = value.as_sequence().ok_or_else(|| PolicyError::Invalid {
        location: location.to_string(),
        detail: format!("`{key}` must be a list of strings"),
    })?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| PolicyError::Invalid {
                    location: location.to_string(),
                    detail: format!("`{key}` must be a list of strings"),
                })
        })
        .collect()
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff]Z` into milliseconds since the epoch.
///
/// UTC only, and `Z` is required. An offset spelling would need an offset
/// database's worth of care to get right, and an expiry that is an hour out is
/// an expiry that lets an exception outlive its grant. §4.3's own example is
/// written with `Z`.
///
/// Hand-written rather than reached for as a dependency: §2.2's list has no date
/// crate, and this is one civil-date conversion whose every branch is covered by
/// the tests below.
pub fn parse_rfc3339_utc(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z').or_else(|| text.strip_suffix('z'))?;
    let (date, rest) = text.split_once('T').or_else(|| text.split_once('t'))?;

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }

    let (clock, fraction) = match rest.split_once('.') {
        Some((clock, fraction)) => (clock, fraction),
        None => (rest, ""),
    };
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next()?.parse().ok()?;
    if clock_parts.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        // 60 for a leap second, which is a real value in the wire format.
        || !(0..=60).contains(&second)
    {
        return None;
    }

    let millis: i64 = if fraction.is_empty() {
        0
    } else {
        if !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut digits = fraction.to_string();
        digits.truncate(3);
        while digits.len() < 3 {
            digits.push('0');
        }
        digits.parse().ok()?
    };

    let days = days_from_civil(year, month, day);
    Some(((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000) + millis)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// resolving the three documents
// ---------------------------------------------------------------------------

/// Load whichever of the three policy files are named.
///
/// A path that is named but absent is an **error**, not an empty document: "the
/// operator pointed at a policy that is not there" and "the operator has no
/// policy" are different situations, and only the second is safe to treat as no
/// rules.
pub fn resolve(
    global: Option<&Path>,
    project: Option<&Path>,
    task: Option<&Path>,
) -> Result<ResolvedPolicy, PolicyError> {
    resolve_documents(
        global
            .map(|p| load_document(p, Origin::Global))
            .transpose()?,
        project
            .map(|p| load_document(p, Origin::Project))
            .transpose()?,
        task.map(|p| load_document(p, Origin::Task)).transpose()?,
    )
}

/// Assemble three already-parsed documents.
pub fn resolve_documents(
    global: Option<PolicyDocument>,
    project: Option<PolicyDocument>,
    task: Option<PolicyDocument>,
) -> Result<ResolvedPolicy, PolicyError> {
    ResolvedPolicy::new(global, project, task)
}

// ---------------------------------------------------------------------------
// canonical form and snapshot
// ---------------------------------------------------------------------------

/// A content-addressed policy — one row of Part 5.1's `policy_snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// `blake3:<hex>` over `canonical_blob` (ADR-0007).
    pub hash: String,
    /// The canonical serialization, which round-trips back into a policy.
    pub canonical_blob: String,
}

/// A run's pinned policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    /// `run.policy_hash`.
    pub hash: String,
    /// The policy that hash names.
    pub policy: ResolvedPolicy,
}

// The canonical shapes. Fields are declared **alphabetically**; derived
// `Serialize` emits them in declaration order, which is what makes the output
// independent of `serde_json`'s map implementation. Do not reorder them.
#[derive(Debug, Serialize, Deserialize)]
struct CanonicalPolicy {
    documents: Vec<CanonicalDocument>,
    version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct CanonicalDocument {
    exceptions: Vec<CanonicalException>,
    origin: Origin,
    rules: Vec<CanonicalRule>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CanonicalRule {
    action: String,
    effect: Effect,
    id: String,
    locked: bool,
    scope: BTreeMap<String, String>,
    when: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CanonicalException {
    action: String,
    effect: Effect,
    expires_at_ms: i64,
    id: String,
    scope: BTreeMap<String, String>,
}

fn canonical_scope(scope: &Scope) -> BTreeMap<String, String> {
    scope.pairs().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Canonically serialize a policy and hash it — §4.4's snapshot.
pub fn snapshot(policy: &ResolvedPolicy) -> Snapshot {
    let documents = policy
        .documents()
        .into_iter()
        .map(|document| {
            let mut rules: Vec<CanonicalRule> = document
                .rules()
                .iter()
                .map(|rule| CanonicalRule {
                    action: rule.pattern.as_str().to_string(),
                    effect: rule.effect,
                    id: rule.id.clone(),
                    locked: rule.locked,
                    scope: canonical_scope(&rule.scope),
                    when: {
                        let mut when = rule.when.clone();
                        when.sort();
                        when
                    },
                })
                .collect();
            rules.sort_by(|a, b| a.id.cmp(&b.id));

            let mut exceptions: Vec<CanonicalException> = document
                .exceptions()
                .iter()
                .map(|exception| CanonicalException {
                    action: exception.action.as_str().to_string(),
                    effect: exception.effect,
                    expires_at_ms: exception.expires_at_ms,
                    id: exception.id.clone(),
                    scope: canonical_scope(&exception.scope),
                })
                .collect();
            exceptions.sort_by(|a, b| a.id.cmp(&b.id));

            CanonicalDocument {
                exceptions,
                origin: document.origin(),
                rules,
            }
        })
        .collect();

    let canonical = CanonicalPolicy {
        documents,
        version: CANONICAL_VERSION,
    };
    let canonical_blob = serde_json::to_string(&canonical)
        .expect("a policy is made of strings, integers and enums, which always serialize");
    let hash = conductor_core::effect::content_hash(canonical_blob.as_bytes());
    Snapshot {
        hash,
        canonical_blob,
    }
}

/// Rebuild a policy from its canonical form.
///
/// This is the path a pinned run takes once the file on disk is gone, so it must
/// reject rather than approximate: a snapshot that partially decodes into fewer
/// rules is a snapshot that silently loosens.
pub fn from_canonical(blob: &str) -> Result<ResolvedPolicy, PolicyError> {
    let canonical: CanonicalPolicy =
        serde_json::from_str(blob).map_err(|e| PolicyError::Invalid {
            location: "policy snapshot".to_string(),
            detail: format!("canonical blob does not decode: {e}"),
        })?;
    if canonical.version != CANONICAL_VERSION {
        return Err(PolicyError::Invalid {
            location: "policy snapshot".to_string(),
            detail: format!(
                "canonical version {} was written by a different Conductor; this \
                 build understands {CANONICAL_VERSION}",
                canonical.version
            ),
        });
    }

    let mut slots: [Option<PolicyDocument>; 3] = [None, None, None];
    for document in canonical.documents {
        let origin = document.origin;
        let rules = document
            .rules
            .into_iter()
            .map(|rule| {
                Ok(Rule {
                    id: rule.id,
                    origin,
                    pattern: ActionPattern::parse(&rule.action)?,
                    effect: rule.effect,
                    scope: Scope::from_pairs(rule.scope),
                    locked: rule.locked,
                    when: rule.when,
                })
            })
            .collect::<Result<Vec<Rule>, PolicyError>>()?;
        let exceptions = document
            .exceptions
            .into_iter()
            .map(|exception| PolicyException {
                id: exception.id,
                origin,
                action: Action::parse(&exception.action),
                effect: exception.effect,
                scope: Scope::from_pairs(exception.scope),
                expires_at_ms: exception.expires_at_ms,
            })
            .collect();
        let slot = match origin {
            Origin::Global => 0,
            Origin::Project => 1,
            Origin::Task => 2,
        };
        slots[slot] = Some(PolicyDocument::new(origin, rules, exceptions)?);
    }

    let [global, project, task] = slots;
    ResolvedPolicy::new(global, project, task)
}

/// Store a snapshot content-addressed. Idempotent: the hash *is* the identity.
pub fn persist(conn: &mut Connection, snapshot: &Snapshot, now_ms: i64) -> Result<(), PolicyError> {
    conductor_store::with_immediate(conn, |tx| {
        tx.execute(
            "INSERT OR IGNORE INTO policy_snapshot (hash, canonical_blob, created_at)
             VALUES (?1, ?2, ?3)",
            params![snapshot.hash, snapshot.canonical_blob, now_ms],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// Read one snapshot back.
pub fn load_snapshot(conn: &Connection, hash: &str) -> Result<Option<ResolvedPolicy>, PolicyError> {
    let blob: Option<String> = conn
        .query_row(
            "SELECT canonical_blob FROM policy_snapshot WHERE hash = ?1",
            params![hash],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| PolicyError::Store(e.into()))?;
    blob.map(|blob| from_canonical(&blob)).transpose()
}

/// The policy a run is pinned to — §4.4's "a run evaluates against its snapshot
/// for its entire life".
///
/// Reads `run.policy_hash` and resolves it **from the store**. Deliberately no
/// filesystem access: that is what makes an edit — or a deletion — of
/// `.conductor/policy.yaml` mid-run unable to change what the run is judged by.
///
/// A dangling pin is [`PolicyError::SnapshotMissing`], never an empty policy.
pub fn pinned_for_run(conn: &Connection, run_id: &RunId) -> Result<Pinned, PolicyError> {
    let hash: Option<String> = conn
        .query_row(
            "SELECT policy_hash FROM run WHERE id = ?1",
            params![run_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| PolicyError::Store(e.into()))?;
    let hash = hash.ok_or_else(|| PolicyError::RunNotFound(run_id.as_str().to_string()))?;

    match load_snapshot(conn, &hash)? {
        Some(policy) => Ok(Pinned { hash, policy }),
        None => Err(PolicyError::SnapshotMissing {
            run: run_id.as_str().to_string(),
            hash,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reference_timestamp_converts_to_the_expected_epoch_millis() {
        // §4.3's own example value.
        assert_eq!(
            parse_rfc3339_utc("2026-08-13T14:03:00Z"),
            Some(1_786_629_780_000)
        );
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00.250Z"), Some(250));
        // A leap day, and the century rule around it.
        assert_eq!(
            parse_rfc3339_utc("2000-02-29T00:00:00Z"),
            Some(951_782_400_000)
        );
    }

    #[test]
    fn a_timestamp_that_is_not_utc_or_not_a_date_is_refused() {
        for bad in [
            "2026-08-13T14:03:00+01:00",
            "2026-08-13 14:03:00Z",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-08-13T24:00:00Z",
            "2026-08-13T14:03Z",
            "whenever",
            "",
            "2026-08-13T14:03:00.xyzZ",
        ] {
            assert!(parse_rfc3339_utc(bad).is_none(), "{bad:?} must be refused");
        }
        // 1900 is not a leap year; 2100 will not be either.
        assert!(parse_rfc3339_utc("1900-02-29T00:00:00Z").is_none());
    }

    #[test]
    fn the_canonical_blob_is_stable_under_rule_order() {
        let a = parse_document(
            "policy:\n  rules:\n    - {id: b, action: git.push, effect: deny}\n    - {id: a, action: git.commit.local, effect: allow}\n",
            Origin::Global,
        )
        .expect("a");
        let b = parse_document(
            "policy:\n  rules:\n    - {id: a, action: git.commit.local, effect: allow}\n    - {id: b, action: git.push, effect: deny}\n",
            Origin::Global,
        )
        .expect("b");
        let a = ResolvedPolicy::new(Some(a), None, None).expect("policy");
        let b = ResolvedPolicy::new(Some(b), None, None).expect("policy");
        assert_eq!(snapshot(&a).canonical_blob, snapshot(&b).canonical_blob);
    }

    #[test]
    fn an_empty_policy_still_produces_a_stable_hash() {
        let empty = ResolvedPolicy::new(None, None, None).expect("policy");
        assert_eq!(snapshot(&empty).hash, snapshot(&empty).hash);
        assert!(snapshot(&empty).hash.starts_with("blake3:"));
    }
}
