//! Loading `verification.yaml` — master plan §4.5.
//!
//! # Why this is a hand-walked `serde_yaml::Value` and not a derive
//!
//! Two requirements pull against `#[derive(Deserialize)]`.
//!
//! **Unknown keys must warn, not fail — and not vanish.** §2.2 forbids
//! `deny_unknown_fields`, but the reason it gives is about *agent* output:
//! agent CLIs churn, so an unknown field is probably a newer version. A
//! `verification.yaml` is written by a **human**, and an unknown key there is
//! much more likely a typo than a forward-compatible field. The two failure
//! modes are not symmetric: erroring bricks a run over a cosmetic mistake,
//! while ignoring silently lets `timeout_second: 5` become the 600-second
//! default and lets `on_timout: fail` quietly stay `inconclusive`. So the
//! loader takes the third option — accept the file, and hand back every
//! unrecognised key as a [`ProfileWarning`] the runner turns into a `Finding`.
//! Findings never auto-resolve (§4.8), so the typo reaches a human without
//! stopping the work. serde has no hook for "tell me what you ignored".
//!
//! **`command` has two spellings that must hash identically.** §4.5 writes
//! `command: cargo check --all-targets`. That string form cannot express an
//! argument containing a space, and the cache key is over the *resolved* argv,
//! so both spellings have to normalise to the same `Vec<String>` before
//! hashing.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde_yaml::Value;

/// The budget a check gets when the profile does not name one.
///
/// §4.5 gives `timeout_seconds` on its `required` checks and omits it on its
/// `invariants`, so a default is forced. 600 s is §4.5's own `typecheck` value.
/// There is deliberately no "no timeout" spelling: a check with no budget is a
/// check that can hang the run forever, and §4.5's whole point about
/// `INCONCLUSIVE` is that a hang must be *classified*, not waited on.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// A profile as loaded, together with anything questionable about the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedProfile {
    /// The profile itself.
    pub profile: Profile,
    /// Keys the loader did not recognise.
    pub warnings: Vec<ProfileWarning>,
}

/// The §4.5 verification profile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    /// Commands whose output forms the toolchain fingerprint.
    pub toolchain_fingerprint: Vec<Command>,
    /// Checks that always run.
    pub required: Vec<Check>,
    /// Checks that run only when the diff touches matching paths.
    pub conditional: Vec<Conditional>,
    /// Cheap checks that always run and are never gated.
    pub invariants: Vec<Check>,
}

/// One check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// `check_id`, part of the cache key.
    pub id: String,
    /// What to run.
    pub command: Command,
    /// The check's own budget.
    pub timeout: Duration,
    /// What exceeding the budget means.
    pub on_timeout: OnTimeout,
    /// Extra runs allowed when the first one fails.
    pub flaky_retry: u32,
}

/// A conditional group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conditional {
    /// The trigger.
    pub when: When,
    /// The checks it gates.
    pub checks: Vec<Check>,
}

/// What makes a conditional group run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct When {
    /// Globs matched against the diff's changed paths.
    pub changed_paths: Vec<String>,
}

/// What a timeout means for one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnTimeout {
    /// §4.5's configured value, and the default.
    #[default]
    Inconclusive,
    /// The check's author asserts that overrunning is a defect.
    Fail,
}

/// A command as argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    argv: Vec<String>,
}

/// A key the loader did not recognise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileWarning {
    /// Dotted path to the containing node.
    pub location: String,
    /// The unrecognised key.
    pub key: String,
}

/// Why a profile could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// The file could not be read.
    #[error("verification profile at {path}: {source}")]
    Io {
        /// The path.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The YAML did not parse.
    #[error("verification profile is not valid YAML: {0}")]
    Yaml(String),
    /// The YAML parsed but does not describe a profile.
    #[error("verification profile is invalid: {0}")]
    Invalid(String),
}

impl Command {
    /// Build from an explicit argv.
    pub fn from_argv(argv: impl IntoIterator<Item = String>) -> Result<Command, ProfileError> {
        let argv: Vec<String> = argv.into_iter().collect();
        if argv.is_empty() || argv[0].trim().is_empty() {
            return Err(ProfileError::Invalid(
                "a command must name a program".to_string(),
            ));
        }
        Ok(Command { argv })
    }

    /// Parse §4.5's string form.
    ///
    /// Whitespace splitting is the whole of it. A quote is **refused** rather
    /// than half-honoured: partial shell quoting is where "the command that ran
    /// is not the command that was configured" lives, and the argv list form
    /// says exactly what was meant.
    pub fn parse(command: &str) -> Result<Command, ProfileError> {
        if command.contains(['\'', '"', '\\']) {
            return Err(ProfileError::Invalid(format!(
                "command {command:?} contains a quote or backslash; the string \
                 form splits on whitespace and performs no shell parsing — \
                 write the argv as a YAML list instead"
            )));
        }
        Command::from_argv(command.split_whitespace().map(str::to_string))
    }

    /// The program and its arguments.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// `blake3:<hex>` over the resolved argv — the cache key's `command_hash`.
    ///
    /// Arguments are joined with `0x1f`, for the reason `OperationId::compute`
    /// gives: plain concatenation would make `["ab","c"]` and `["a","bc"]` the
    /// same command, and a cache whose key confuses two commands returns the
    /// wrong answer rather than no answer.
    pub fn command_hash(&self) -> String {
        let mut bytes = Vec::new();
        for arg in &self.argv {
            bytes.extend_from_slice(arg.as_bytes());
            bytes.push(0x1f);
        }
        conductor_core::effect::content_hash(&bytes)
    }
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.argv.join(" "))
    }
}

/// Read and parse a profile from disk.
pub fn load(path: &Path) -> Result<LoadedProfile, ProfileError> {
    let text = std::fs::read_to_string(path).map_err(|source| ProfileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&text)
}

/// Parse a profile from YAML text.
pub fn parse(yaml: &str) -> Result<LoadedProfile, ProfileError> {
    let root: Value = serde_yaml::from_str(yaml).map_err(|e| ProfileError::Yaml(e.to_string()))?;
    let mut warnings = Vec::new();

    let root_map = as_mapping(&root, "the file")?;
    warn_unknown(root_map, &["verification"], "", &mut warnings);
    let body = root_map.get(Value::from("verification")).ok_or_else(|| {
        ProfileError::Invalid(
            "the file has no top-level `verification:` key (§4.5's shape)".to_string(),
        )
    })?;
    let body = as_mapping(body, "verification")?;
    warn_unknown(
        body,
        &[
            "toolchain_fingerprint",
            "required",
            "conditional",
            "invariants",
        ],
        "verification",
        &mut warnings,
    );

    let mut profile = Profile::default();

    for (i, entry) in sequence(body, "toolchain_fingerprint")?.iter().enumerate() {
        profile.toolchain_fingerprint.push(command(
            entry,
            &format!("verification.toolchain_fingerprint[{i}]"),
        )?);
    }

    for (i, entry) in sequence(body, "required")?.iter().enumerate() {
        profile.required.push(check(
            entry,
            &format!("verification.required[{i}]"),
            &mut warnings,
        )?);
    }

    for (i, entry) in sequence(body, "invariants")?.iter().enumerate() {
        profile.invariants.push(check(
            entry,
            &format!("verification.invariants[{i}]"),
            &mut warnings,
        )?);
    }

    for (i, entry) in sequence(body, "conditional")?.iter().enumerate() {
        let location = format!("verification.conditional[{i}]");
        profile
            .conditional
            .push(conditional(entry, &location, &mut warnings)?);
    }

    reject_duplicate_ids(&profile)?;
    Ok(LoadedProfile { profile, warnings })
}

fn conditional(
    entry: &Value,
    location: &str,
    warnings: &mut Vec<ProfileWarning>,
) -> Result<Conditional, ProfileError> {
    let map = as_mapping(entry, location)?;
    warn_unknown(map, &["when", "checks", "commands"], location, warnings);

    let when = match map.get(Value::from("when")) {
        Some(value) => {
            let when_location = format!("{location}.when");
            let when_map = as_mapping(value, &when_location)?;
            warn_unknown(when_map, &["changed_paths"], &when_location, warnings);
            When {
                changed_paths: strings(when_map, "changed_paths", &when_location)?,
            }
        }
        None => {
            return Err(ProfileError::Invalid(format!(
                "{location} has no `when:`; a conditional check with no trigger \
                 is a required check"
            )));
        }
    };

    // §4.5 writes `commands: [migration-validate]` and defines no `checks:`
    // key, but it also gives conditional entries no ids, timeouts or
    // `on_timeout` — and the cache key needs a `check_id`. Both spellings are
    // therefore accepted: `checks:` carries full definitions, and §4.5's
    // `commands:` shorthand derives the id from the command itself, which for
    // the plan's own example yields exactly `migration-validate`.
    let mut checks = Vec::new();
    if let Some(value) = map.get(Value::from("checks")) {
        for (i, entry) in as_sequence(value, &format!("{location}.checks"))?
            .iter()
            .enumerate()
        {
            checks.push(check(entry, &format!("{location}.checks[{i}]"), warnings)?);
        }
    }
    if let Some(value) = map.get(Value::from("commands")) {
        for (i, entry) in as_sequence(value, &format!("{location}.commands"))?
            .iter()
            .enumerate()
        {
            let command = command(entry, &format!("{location}.commands[{i}]"))?;
            checks.push(Check {
                id: slug(&command.to_string()),
                command,
                timeout: DEFAULT_TIMEOUT,
                on_timeout: OnTimeout::default(),
                flaky_retry: 0,
            });
        }
    }
    if checks.is_empty() {
        return Err(ProfileError::Invalid(format!(
            "{location} triggers nothing: give it `checks:` or `commands:`"
        )));
    }
    Ok(Conditional { when, checks })
}

fn check(
    entry: &Value,
    location: &str,
    warnings: &mut Vec<ProfileWarning>,
) -> Result<Check, ProfileError> {
    let map = as_mapping(entry, location)?;
    warn_unknown(
        map,
        &[
            "id",
            "command",
            "timeout_seconds",
            "on_timeout",
            "flaky_retry",
        ],
        location,
        warnings,
    );

    let id = map
        .get(Value::from("id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProfileError::Invalid(format!("{location} has no `id:`")))?
        .to_string();
    if id.trim().is_empty() {
        return Err(ProfileError::Invalid(format!("{location} has an empty id")));
    }

    let command_value = map
        .get(Value::from("command"))
        .ok_or_else(|| ProfileError::Invalid(format!("{location} has no `command:`")))?;
    let command = command(command_value, location)?;

    let timeout = match map.get(Value::from("timeout_seconds")) {
        None => DEFAULT_TIMEOUT,
        Some(v) => {
            let seconds = v.as_u64().filter(|s| *s > 0).ok_or_else(|| {
                ProfileError::Invalid(format!(
                    "{location}.timeout_seconds must be a positive whole number of seconds"
                ))
            })?;
            Duration::from_secs(seconds)
        }
    };

    let on_timeout = match map.get(Value::from("on_timeout")).and_then(|v| v.as_str()) {
        None | Some("inconclusive") => OnTimeout::Inconclusive,
        Some("fail") => OnTimeout::Fail,
        Some(other) => {
            return Err(ProfileError::Invalid(format!(
                "{location}.on_timeout is {other:?}; expected `inconclusive` or `fail`"
            )));
        }
    };

    let flaky_retry = match map.get(Value::from("flaky_retry")) {
        None => 0,
        Some(v) => u32::try_from(v.as_u64().ok_or_else(|| {
            ProfileError::Invalid(format!("{location}.flaky_retry must be a whole number"))
        })?)
        .map_err(|_| ProfileError::Invalid(format!("{location}.flaky_retry is absurdly large")))?,
    };

    Ok(Check {
        id,
        command,
        timeout,
        on_timeout,
        flaky_retry,
    })
}

fn command(value: &Value, location: &str) -> Result<Command, ProfileError> {
    match value {
        Value::String(text) => Command::parse(text),
        Value::Sequence(items) => {
            let mut argv = Vec::with_capacity(items.len());
            for item in items {
                let arg = item.as_str().ok_or_else(|| {
                    ProfileError::Invalid(format!(
                        "{location}: every argv element must be a string"
                    ))
                })?;
                argv.push(arg.to_string());
            }
            Command::from_argv(argv)
        }
        _ => Err(ProfileError::Invalid(format!(
            "{location}: a command is a string or a list of strings"
        ))),
    }
    .map_err(|e| match e {
        ProfileError::Invalid(detail) => ProfileError::Invalid(format!("{location}: {detail}")),
        other => other,
    })
}

fn reject_duplicate_ids(profile: &Profile) -> Result<(), ProfileError> {
    let mut seen = BTreeSet::new();
    let all = profile
        .required
        .iter()
        .chain(profile.invariants.iter())
        .chain(profile.conditional.iter().flat_map(|c| c.checks.iter()));
    for check in all {
        if !seen.insert(check.id.as_str()) {
            return Err(ProfileError::Invalid(format!(
                "check id {:?} is used twice; ids are part of the result cache \
                 key, so two checks sharing one would be one row",
                check.id
            )));
        }
    }
    Ok(())
}

/// A stable, readable id for §4.5's `commands:` shorthand.
fn slug(command: &str) -> String {
    let mut out = String::new();
    for c in command.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(64).collect()
}

fn as_mapping<'v>(
    value: &'v Value,
    location: &str,
) -> Result<&'v serde_yaml::Mapping, ProfileError> {
    value
        .as_mapping()
        .ok_or_else(|| ProfileError::Invalid(format!("{location} must be a mapping")))
}

fn as_sequence<'v>(value: &'v Value, location: &str) -> Result<&'v Vec<Value>, ProfileError> {
    value
        .as_sequence()
        .ok_or_else(|| ProfileError::Invalid(format!("{location} must be a list")))
}

fn sequence<'m>(
    map: &'m serde_yaml::Mapping,
    key: &str,
) -> Result<std::borrow::Cow<'m, [Value]>, ProfileError> {
    match map.get(Value::from(key)) {
        None => Ok(std::borrow::Cow::Owned(Vec::new())),
        Some(value) => Ok(std::borrow::Cow::Borrowed(as_sequence(
            value,
            &format!("verification.{key}"),
        )?)),
    }
}

fn strings(
    map: &serde_yaml::Mapping,
    key: &str,
    location: &str,
) -> Result<Vec<String>, ProfileError> {
    let Some(value) = map.get(Value::from(key)) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in as_sequence(value, &format!("{location}.{key}"))? {
        out.push(
            item.as_str()
                .ok_or_else(|| {
                    ProfileError::Invalid(format!("{location}.{key} must be a list of strings"))
                })?
                .to_string(),
        );
    }
    Ok(out)
}

fn warn_unknown(
    map: &serde_yaml::Mapping,
    known: &[&str],
    location: &str,
    warnings: &mut Vec<ProfileWarning>,
) {
    for key in map.keys() {
        let Some(key) = key.as_str() else { continue };
        if !known.contains(&key) {
            warnings.push(ProfileWarning {
                location: location.to_string(),
                key: key.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §4.5's example, verbatim.
    const SPEC_EXAMPLE: &str = r#"
verification:
  toolchain_fingerprint:            # participates in the result cache key
    - "rustc --version"
    - "cargo --version"
  required:
    - id: typecheck
      command: cargo check --all-targets
      timeout_seconds: 600
      on_timeout: inconclusive      # NOT failure
    - id: unit-tests
      command: cargo test
      timeout_seconds: 1200
      flaky_retry: 1                # exactly one; disagreement ⇒ INCONCLUSIVE
  conditional:
    - when: {changed_paths: ["migrations/**"]}
      commands: [migration-validate]
  invariants:                        # cheap, always, never skipped
    - id: no-secrets
      command: conductor scan secrets
    - id: git-invariants
      command: conductor scan git-invariants
"#;

    #[test]
    fn the_spec_example_parses_into_the_shape_it_describes() {
        let loaded = parse(SPEC_EXAMPLE).expect("§4.5's own example must load");
        let p = &loaded.profile;

        assert_eq!(
            p.toolchain_fingerprint
                .iter()
                .map(|c| c.argv().to_vec())
                .collect::<Vec<_>>(),
            vec![
                vec!["rustc".to_string(), "--version".to_string()],
                vec!["cargo".to_string(), "--version".to_string()],
            ]
        );

        assert_eq!(p.required.len(), 2);
        assert_eq!(p.required[0].id, "typecheck");
        assert_eq!(
            p.required[0].command.argv(),
            ["cargo", "check", "--all-targets"]
        );
        assert_eq!(p.required[0].timeout, Duration::from_secs(600));
        assert_eq!(p.required[0].on_timeout, OnTimeout::Inconclusive);
        assert_eq!(p.required[0].flaky_retry, 0);

        assert_eq!(p.required[1].id, "unit-tests");
        assert_eq!(p.required[1].flaky_retry, 1);

        assert_eq!(p.conditional.len(), 1);
        assert_eq!(p.conditional[0].when.changed_paths, ["migrations/**"]);
        assert_eq!(p.conditional[0].checks.len(), 1);
        assert_eq!(p.conditional[0].checks[0].id, "migration-validate");

        assert_eq!(p.invariants.len(), 2);
        assert_eq!(p.invariants[0].id, "no-secrets");
        assert_eq!(
            p.invariants[1].command.argv(),
            ["conductor", "scan", "git-invariants"]
        );
    }

    #[test]
    fn an_unknown_key_warns_and_does_not_fail() {
        // `verification.yaml` is written by a human, not by an agent: an
        // unknown key there is far more likely a typo than a newer Conductor's
        // field. Erroring would brick a run over a cosmetic mistake, and
        // ignoring silently would let `timeout_second: 5` quietly become the
        // 600-second default. So: load, and carry the doubt.
        let loaded = parse(
            r#"
verification:
  required:
    - id: t
      command: /bin/true
      timeout_second: 5
"#,
        )
        .expect("an unknown key must not fail the load");

        assert_eq!(loaded.profile.required.len(), 1);
        assert_eq!(
            loaded.warnings,
            vec![ProfileWarning {
                location: "verification.required[0]".to_string(),
                key: "timeout_second".to_string(),
            }]
        );
        // And the typo did not silently take effect.
        assert_eq!(loaded.profile.required[0].timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn an_argv_list_and_the_equivalent_string_hash_identically() {
        // The cache key is over the *resolved* argv, so the two spellings of
        // one command must not produce two cache entries.
        let from_string = parse(
            r#"
verification:
  required:
    - {id: a, command: "/bin/echo hello world"}
"#,
        )
        .expect("string form");
        let from_list = parse(
            r#"
verification:
  required:
    - id: a
      command: ["/bin/echo", "hello", "world"]
"#,
        )
        .expect("list form");

        assert_eq!(
            from_string.profile.required[0].command,
            from_list.profile.required[0].command
        );
        assert_eq!(
            from_string.profile.required[0].command.command_hash(),
            from_list.profile.required[0].command.command_hash()
        );
    }

    #[test]
    fn a_quoted_argument_cannot_be_expressed_in_the_string_form_and_is_refused() {
        // Whitespace splitting is the whole of the string form. Rather than
        // half-implement shell quoting — which is where "the command that ran
        // is not the command configured" bugs live — the loader refuses and
        // names the alternative.
        let error = parse(
            r#"
verification:
  required:
    - {id: a, command: "sh -c 'echo hi'"}
"#,
        )
        .expect_err("a quote in the string form must be refused");
        let message = error.to_string();
        assert!(message.contains("quote"), "unhelpful message: {message}");
        assert!(message.contains("list"), "unhelpful message: {message}");
    }

    #[test]
    fn command_hash_separates_arguments_that_concatenate_to_the_same_string() {
        // ["ab", "c"] and ["a", "bc"] are different commands.
        let a = Command::from_argv(["ab".to_string(), "c".to_string()]).expect("argv");
        let b = Command::from_argv(["a".to_string(), "bc".to_string()]).expect("argv");
        assert_ne!(a.command_hash(), b.command_hash());
    }

    #[test]
    fn an_empty_command_is_refused() {
        assert!(Command::from_argv([]).is_err());
        assert!(Command::parse("   ").is_err());
    }

    #[test]
    fn duplicate_check_ids_are_refused_because_the_cache_key_would_collide() {
        // Two checks sharing an id, a command hash and a tree would be one row
        // in `ix_verif_cache`. Refuse at load, where it is a typo, rather than
        // at insert, where it is a mystery.
        let error = parse(
            r#"
verification:
  required:
    - {id: a, command: /bin/true}
  invariants:
    - {id: a, command: /bin/false}
"#,
        )
        .expect_err("duplicate ids must be refused");
        assert!(error.to_string().contains('a'));
    }

    #[test]
    fn on_timeout_fail_is_expressible_but_inconclusive_is_the_default() {
        let loaded = parse(
            r#"
verification:
  required:
    - {id: a, command: /bin/true}
    - {id: b, command: /bin/true, on_timeout: fail}
"#,
        )
        .expect("load");
        assert_eq!(
            loaded.profile.required[0].on_timeout,
            OnTimeout::Inconclusive
        );
        assert_eq!(loaded.profile.required[1].on_timeout, OnTimeout::Fail);
    }

    #[test]
    fn a_conditional_may_also_carry_full_check_definitions() {
        let loaded = parse(
            r#"
verification:
  conditional:
    - when: {changed_paths: ["migrations/**"]}
      checks:
        - {id: migrate, command: /bin/true, timeout_seconds: 30}
"#,
        )
        .expect("load");
        assert_eq!(loaded.profile.conditional[0].checks[0].id, "migrate");
        assert_eq!(
            loaded.profile.conditional[0].checks[0].timeout,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn a_file_without_the_verification_key_is_refused() {
        let error = parse("required:\n  - {id: a, command: /bin/true}\n")
            .expect_err("the top-level key is §4.5's shape");
        assert!(error.to_string().contains("verification"));
    }
}
