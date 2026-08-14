//! The policy algebra — master plan §4.4.
//!
//! # Why the shapes here are the way they are
//!
//! §4.4 is a decision procedure whose failure mode is *silent permission*. Three
//! things in this module exist to make that failure mode unrepresentable rather
//! than merely tested for.
//!
//! **1. [`Effect`] derives `Ord`.** §4.4 says "the join is `max`". Writing the
//! join as a hand-rolled `match` would create a second definition of the order
//! that can drift from the first; [`Effect::join`] is literally `Ord::max`, so
//! the two cannot disagree.
//!
//! **2. Parsing an action cannot fail.** [`Action::parse`] returns an `Action`,
//! never an `Option<Action>` or a `Result`. An `Option` is exactly the hole §4.4
//! warns about — a caller writes `.unwrap_or(Effect::Allow)` and the taxonomy
//! being incomplete has quietly become permission. Instead an unrecognised name
//! becomes [`Action::Unknown`], which [`crate::policy::evaluate`] floors at
//! `deny`. Incompleteness reads as refusal, and the unrecognised name survives
//! into the explanation so a human can extend the taxonomy.
//!
//! **3. Built-in invariants are not part of any document.** [`PolicyDocument`]
//! has no field for them, the loader has no syntax for them, and
//! [`BuiltinInvariant::ALL`] is a `const` slice with no constructor. §4.4: "not
//! configurable at all". The only way to weaken one would be to edit this file,
//! which is the point.
//!
//! # What a `Fact` is for
//!
//! §4.4: *"Every fact carries `source: deterministic | model_assisted | human`.
//! A `require_approval` may rest on any; **a `deny` must rest only on
//! `deterministic` facts.**"* A [`Fact`] is therefore constructed through a
//! source-named constructor — there is no `Fact::new` that defaults a source,
//! because a defaulted source is a source nobody chose.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Anything that makes a policy unusable.
///
/// Loading and construction share one error type because they share one rule:
/// **a policy that cannot be understood is never a permissive policy.** Every
/// variant here is a refusal, and none of them has a "carry on with defaults"
/// counterpart.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// A policy file could not be read.
    #[error("policy file {path}: {source}")]
    Io {
        /// The path.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The YAML did not parse.
    #[error("policy is not valid YAML: {0}")]
    Yaml(String),
    /// The YAML parsed but does not describe a policy.
    #[error("policy is invalid at {location}: {detail}")]
    Invalid {
        /// Dotted path to the offending node.
        location: String,
        /// What is wrong.
        detail: String,
    },
    /// A `locked: true` rule appeared outside the global document.
    ///
    /// §4.4 gives the ceiling to *locked global* rules. A project able to mint a
    /// locked rule would be a project able to raise its own ceiling.
    #[error(
        "{origin} rule {rule_id:?} is locked; only the global policy may declare a \
         locked rule, because locked rules are the Stage-1 ceiling (§4.4)"
    )]
    LockedOutsideGlobal {
        /// Where the rule came from.
        origin: Origin,
        /// The offending rule.
        rule_id: String,
    },
    /// A document tried to configure a built-in invariant.
    #[error(
        "{what} names {action}, which is governed by the built-in invariant \
         {invariant:?}; §4.4 makes built-in invariants not configurable at all"
    )]
    BuiltinNotConfigurable {
        /// Which construct tried.
        what: String,
        /// The action it named.
        action: String,
        /// The invariant that governs it.
        invariant: &'static str,
    },
    /// A dimension name that is not one of §4.2's four gating dimensions.
    #[error("{0}")]
    NotAGatingDimension(String),
    /// The store refused a read or write.
    #[error("policy store: {0}")]
    Store(#[from] conductor_store::StoreError),
    /// A run pins a `policy_snapshot.hash` with no row behind it.
    #[error(
        "run {run} pins policy snapshot {hash}, which is not in the store; a run \
         whose snapshot cannot be resolved is halted, never evaluated against an \
         empty policy (§4.4)"
    )]
    SnapshotMissing {
        /// The run.
        run: String,
        /// The hash it pins.
        hash: String,
    },
    /// No `run` row with that id.
    #[error("no run {0}")]
    RunNotFound(String),
}

/// The three effects, in §4.4's total order.
///
/// ```text
/// allow  <  require_approval  <  deny
/// ```
///
/// `Ord` is **derived**, so the declaration order above *is* the order, and
/// [`Effect::join`] is `Ord::max` rather than a second definition of it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Conductor proceeds.
    #[default]
    Allow,
    /// Conductor stops and asks a human (S8 owns the asking).
    RequireApproval,
    /// Conductor refuses.
    Deny,
}

impl Effect {
    /// Every effect, most permissive first.
    pub const ALL: &'static [Effect] = &[Effect::Allow, Effect::RequireApproval, Effect::Deny];

    /// §4.4's join. Literally `max`, so it cannot drift from the order.
    pub fn join(self, other: Effect) -> Effect {
        self.max(other)
    }

    /// The dual, used only where an exception *lowers* an effect.
    pub fn meet(self, other: Effect) -> Effect {
        self.min(other)
    }

    /// The spelling used in YAML, JSON and `explain` output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Effect::Allow => "allow",
            Effect::RequireApproval => "require_approval",
            Effect::Deny => "deny",
        }
    }

    /// Parse §4.4's spelling. `None` for anything else — and every caller of
    /// this function is a *loader*, which turns `None` into a hard error rather
    /// than into a default.
    pub fn parse(text: &str) -> Option<Effect> {
        match text {
            "allow" => Some(Effect::Allow),
            "require_approval" => Some(Effect::RequireApproval),
            "deny" => Some(Effect::Deny),
            _ => None,
        }
    }
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One of §4.4's twenty-two typed actions, or a name outside the taxonomy.
///
/// [`Action::Unknown`] is not an error state. It is the representation that
/// makes "the taxonomy will be incomplete on day one" safe: evaluation floors an
/// unknown action at `deny`, so an action Conductor has never heard of cannot be
/// performed, and the name is carried through to the explanation so that the
/// taxonomy can be extended deliberately.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    /// `git.commit.local`
    GitCommitLocal,
    /// `git.push`
    GitPush,
    /// `git.remote.modify`
    GitRemoteModify,
    /// `git.branch.delete`
    GitBranchDelete,
    /// `git.force_push`
    GitForcePush,
    /// `dependency.add.runtime`
    DependencyAddRuntime,
    /// `dependency.add.dev`
    DependencyAddDev,
    /// `dependency.remove`
    DependencyRemove,
    /// `lockfile.modify`
    LockfileModify,
    /// `database.migration.create`
    DatabaseMigrationCreate,
    /// `database.migration.apply`
    DatabaseMigrationApply,
    /// `database.destructive_change`
    DatabaseDestructiveChange,
    /// `filesystem.write.outside_workspace`
    FilesystemWriteOutsideWorkspace,
    /// `network.external_access`
    NetworkExternalAccess,
    /// `credential.access`
    CredentialAccess,
    /// `deployment.execute`
    DeploymentExecute,
    /// `release.publish`
    ReleasePublish,
    /// `architecture.change`
    ArchitectureChange,
    /// `authentication.change`
    AuthenticationChange,
    /// `authorization.change`
    AuthorizationChange,
    /// `billing.spend`
    BillingSpend,
    /// `service.paid_addition`
    ServicePaidAddition,
    /// A name outside the taxonomy. Evaluation floors this at `deny`.
    Unknown(String),
}

impl Action {
    /// §4.4's typed-action block, in the order it is written there.
    pub const KNOWN: &'static [&'static str] = &[
        "git.commit.local",
        "git.push",
        "git.remote.modify",
        "git.branch.delete",
        "git.force_push",
        "dependency.add.runtime",
        "dependency.add.dev",
        "dependency.remove",
        "lockfile.modify",
        "database.migration.create",
        "database.migration.apply",
        "database.destructive_change",
        "filesystem.write.outside_workspace",
        "network.external_access",
        "credential.access",
        "deployment.execute",
        "release.publish",
        "architecture.change",
        "authentication.change",
        "authorization.change",
        "billing.spend",
        "service.paid_addition",
    ];

    /// Interpret an action name.
    ///
    /// **Infallible on purpose.** There is no `Option` for a caller to unwrap
    /// into permission; an unrecognised name becomes [`Action::Unknown`], which
    /// evaluation denies.
    pub fn parse(name: &str) -> Action {
        match name {
            "git.commit.local" => Action::GitCommitLocal,
            "git.push" => Action::GitPush,
            "git.remote.modify" => Action::GitRemoteModify,
            "git.branch.delete" => Action::GitBranchDelete,
            "git.force_push" => Action::GitForcePush,
            "dependency.add.runtime" => Action::DependencyAddRuntime,
            "dependency.add.dev" => Action::DependencyAddDev,
            "dependency.remove" => Action::DependencyRemove,
            "lockfile.modify" => Action::LockfileModify,
            "database.migration.create" => Action::DatabaseMigrationCreate,
            "database.migration.apply" => Action::DatabaseMigrationApply,
            "database.destructive_change" => Action::DatabaseDestructiveChange,
            "filesystem.write.outside_workspace" => Action::FilesystemWriteOutsideWorkspace,
            "network.external_access" => Action::NetworkExternalAccess,
            "credential.access" => Action::CredentialAccess,
            "deployment.execute" => Action::DeploymentExecute,
            "release.publish" => Action::ReleasePublish,
            "architecture.change" => Action::ArchitectureChange,
            "authentication.change" => Action::AuthenticationChange,
            "authorization.change" => Action::AuthorizationChange,
            "billing.spend" => Action::BillingSpend,
            "service.paid_addition" => Action::ServicePaidAddition,
            other => Action::Unknown(other.to_string()),
        }
    }

    /// The action's name, including for [`Action::Unknown`].
    pub fn as_str(&self) -> &str {
        match self {
            Action::GitCommitLocal => "git.commit.local",
            Action::GitPush => "git.push",
            Action::GitRemoteModify => "git.remote.modify",
            Action::GitBranchDelete => "git.branch.delete",
            Action::GitForcePush => "git.force_push",
            Action::DependencyAddRuntime => "dependency.add.runtime",
            Action::DependencyAddDev => "dependency.add.dev",
            Action::DependencyRemove => "dependency.remove",
            Action::LockfileModify => "lockfile.modify",
            Action::DatabaseMigrationCreate => "database.migration.create",
            Action::DatabaseMigrationApply => "database.migration.apply",
            Action::DatabaseDestructiveChange => "database.destructive_change",
            Action::FilesystemWriteOutsideWorkspace => "filesystem.write.outside_workspace",
            Action::NetworkExternalAccess => "network.external_access",
            Action::CredentialAccess => "credential.access",
            Action::DeploymentExecute => "deployment.execute",
            Action::ReleasePublish => "release.publish",
            Action::ArchitectureChange => "architecture.change",
            Action::AuthenticationChange => "authentication.change",
            Action::AuthorizationChange => "authorization.change",
            Action::BillingSpend => "billing.spend",
            Action::ServicePaidAddition => "service.paid_addition",
            Action::Unknown(name) => name,
        }
    }

    /// Whether the taxonomy recognises this action.
    pub fn is_known(&self) -> bool {
        !matches!(self, Action::Unknown(_))
    }

    /// The floor an action imposes before any rule is consulted.
    ///
    /// §4.4: *"unknown action → `deny` (fail closed; the taxonomy will be
    /// incomplete on day one and incompleteness must not read as permission)"*.
    /// Expressed as a floor rather than as an early return so that it
    /// participates in the join *and* in the exception clamp — an exception must
    /// not be able to grant an action Conductor cannot name.
    pub fn floor(&self) -> Effect {
        if self.is_known() {
            Effect::Allow
        } else {
            Effect::Deny
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a fact was derived — §4.4's `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    /// Read off the repository, the filesystem or git. Reproducible.
    Deterministic,
    /// Inferred. May be wrong, and §4.4 forbids it from carrying a `deny`.
    ModelAssisted,
    /// Asserted by a person.
    Human,
}

impl FactSource {
    /// §4.4: only `deterministic` may carry a `deny`.
    ///
    /// `Human` is included in the refusal deliberately. §4.4 says "**only**
    /// `deterministic`", and a human assertion is a reason to *ask*, not a
    /// reason to block work with no recourse.
    pub fn may_carry_a_deny(&self) -> bool {
        matches!(self, FactSource::Deterministic)
    }

    /// The spelling used in YAML, JSON and `explain` output.
    pub fn as_str(&self) -> &'static str {
        match self {
            FactSource::Deterministic => "deterministic",
            FactSource::ModelAssisted => "model_assisted",
            FactSource::Human => "human",
        }
    }
}

impl fmt::Display for FactSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One observation, with its derivation attached.
///
/// There is deliberately no constructor that leaves `source` to a default: a
/// defaulted derivation is a derivation nobody chose, and the default anyone
/// would pick is `deterministic` — the one value that unlocks `deny`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    /// What was observed, e.g. `dependency_added`.
    pub key: String,
    /// The observation itself, e.g. `serde_yaml`.
    pub value: String,
    /// How it was derived.
    pub source: FactSource,
    /// Supporting material: a diff, a path, a redacted excerpt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

impl Fact {
    /// A fact read directly off the repository or filesystem.
    pub fn deterministic(key: impl Into<String>, value: impl Into<String>) -> Fact {
        Fact::with_source(key, value, FactSource::Deterministic)
    }

    /// A fact a model inferred. Cannot carry a `deny` (§4.4).
    pub fn model_assisted(key: impl Into<String>, value: impl Into<String>) -> Fact {
        Fact::with_source(key, value, FactSource::ModelAssisted)
    }

    /// A fact a person asserted.
    pub fn human(key: impl Into<String>, value: impl Into<String>) -> Fact {
        Fact::with_source(key, value, FactSource::Human)
    }

    fn with_source(key: impl Into<String>, value: impl Into<String>, source: FactSource) -> Fact {
        Fact {
            key: key.into(),
            value: value.into(),
            source,
            evidence: None,
        }
    }

    /// Attach supporting material.
    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Fact {
        self.evidence = Some(evidence.into());
        self
    }
}

/// The facts an evaluation was given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FactSet(Vec<Fact>);

impl FactSet {
    /// An empty set.
    pub fn new() -> FactSet {
        FactSet(Vec::new())
    }

    /// Add one fact.
    pub fn push(&mut self, fact: Fact) {
        self.0.push(fact);
    }

    /// The first fact with this key.
    pub fn get(&self, key: &str) -> Option<&Fact> {
        self.0.iter().find(|f| f.key == key)
    }

    /// Whether any fact carries this key.
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Every fact, in the order it was supplied.
    pub fn iter(&self) -> impl Iterator<Item = &Fact> {
        self.0.iter()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Fact> for FactSet {
    fn from_iter<I: IntoIterator<Item = Fact>>(iter: I) -> FactSet {
        FactSet(iter.into_iter().collect())
    }
}

/// Which document a rule or exception came from.
///
/// Ordered global-first so that `explain` lists rules in the order §4.4's join
/// names them, and so that two policies with the same rules serialize the same
/// way regardless of the order the loader happened to read the files in.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// `~/.config/conductor/policy.yaml`. The only document that may lock.
    #[default]
    Global,
    /// `<repo>/.conductor/policy.yaml`.
    Project,
    /// A per-task constraint.
    Task,
}

impl Origin {
    /// The spelling used in YAML, JSON and `explain` output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Origin::Global => "global",
            Origin::Project => "project",
            Origin::Task => "task",
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which actions a rule applies to.
///
/// Two spellings, and no more: an exact action name, or a dotted prefix followed
/// by `*` (`dependency.*`, or `*` for everything). There is no glob engine here
/// because a policy pattern that is hard to read is a policy nobody audits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActionPattern(String);

impl ActionPattern {
    /// Parse a pattern, refusing anything the two spellings do not cover.
    pub fn parse(text: &str) -> Result<ActionPattern, PolicyError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(PolicyError::Invalid {
                location: "action".to_string(),
                detail: "an action pattern may not be empty".to_string(),
            });
        }
        // A `*` anywhere other than as the whole final segment is a glob
        // spelling this deliberately does not support, and accepting it would
        // mean silently matching less (or more) than the author intended.
        let star_count = trimmed.matches('*').count();
        let well_formed =
            star_count == 0 || (star_count == 1 && (trimmed == "*" || trimmed.ends_with(".*")));
        if !well_formed {
            return Err(PolicyError::Invalid {
                location: "action".to_string(),
                detail: format!(
                    "{trimmed:?} is not an action pattern; write an exact action \
                     name, a dotted prefix ending in `.*`, or `*`"
                ),
            });
        }
        Ok(ActionPattern(trimmed.to_string()))
    }

    /// Whether this pattern names exactly one action.
    pub fn is_exact(&self) -> bool {
        !self.0.contains('*')
    }

    /// Whether the pattern covers `action`.
    pub fn matches(&self, action: &Action) -> bool {
        if self.0 == "*" {
            return true;
        }
        match self.0.strip_suffix(".*") {
            Some(prefix) => {
                let name = action.as_str();
                name.strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('.'))
            }
            None => self.0 == action.as_str(),
        }
    }

    /// The pattern as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActionPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a rule or exception applies.
///
/// A set of key/value constraints matched against the evaluation's context —
/// `{run: r-0041}`, `{repo: acme}`. Empty means everywhere, which is right for a
/// rule and forbidden for an exception (see [`PolicyDocument::new`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scope(BTreeMap<String, String>);

/// Why a scope did or did not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeMatch {
    /// Every constraint held.
    Matched,
    /// One constraint did not, named so that `explain` can say which.
    Mismatch {
        /// The constraint's key.
        key: String,
        /// What the scope requires.
        expected: String,
        /// What the context carried, if anything.
        actual: Option<String>,
    },
}

impl Scope {
    /// The scope that constrains nothing.
    pub fn everywhere() -> Scope {
        Scope(BTreeMap::new())
    }

    /// Build from key/value pairs.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Scope {
        Scope(pairs.into_iter().collect())
    }

    /// Whether this scope constrains nothing.
    pub fn is_everywhere(&self) -> bool {
        self.0.is_empty()
    }

    /// The constraints, sorted by key.
    pub fn pairs(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }

    /// Match against an evaluation context.
    ///
    /// A key the context does not carry is a **mismatch**, not a wildcard: a
    /// scope naming a run must not apply to an evaluation that has no run.
    pub fn matches(&self, context: &BTreeMap<String, String>) -> ScopeMatch {
        for (key, expected) in &self.0 {
            let actual = context.get(key);
            if actual != Some(expected) {
                return ScopeMatch::Mismatch {
                    key: key.clone(),
                    expected: expected.clone(),
                    actual: actual.cloned(),
                };
            }
        }
        ScopeMatch::Matched
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("everywhere");
        }
        let rendered: Vec<String> = self.0.iter().map(|(k, v)| format!("{k}={v}")).collect();
        f.write_str(&rendered.join(","))
    }
}

/// One policy rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Stable identifier. Appears in `explain` and in approval requests.
    pub id: String,
    /// Which document it came from.
    pub origin: Origin,
    /// Which actions it applies to.
    pub pattern: ActionPattern,
    /// What it says.
    pub effect: Effect,
    /// Where it applies.
    pub scope: Scope,
    /// Whether it forms part of the Stage-1 ceiling. Global documents only.
    pub locked: bool,
    /// Fact keys that must all be present for the rule to apply.
    ///
    /// This is also what makes §4.4's deny rule checkable: the facts named here
    /// are the rule's *supporting* facts, and a `deny` supported by anything
    /// other than a `deterministic` fact is capped at `require_approval`.
    pub when: Vec<String>,
}

/// A scoped, expiring loosening — §4.3's third approval kind.
///
/// Constrained at construction to name exactly one action and a non-empty scope.
/// §4.4 says an exception applies only when it "matches **exactly**"; a wildcard
/// exception is a blanket loosening wearing an exception's clothes, and an
/// unscoped one is simply a rule with an expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyException {
    /// Stable identifier.
    pub id: String,
    /// Which document it came from.
    pub origin: Origin,
    /// The single action it loosens.
    pub action: Action,
    /// What it lowers the effect to — bounded by the ceiling.
    pub effect: Effect,
    /// Where it applies. Never empty.
    pub scope: Scope,
    /// When it stops applying, in milliseconds since the epoch. Mandatory.
    pub expires_at_ms: i64,
}

/// One policy file, or one task's constraints.
///
/// Constructed through [`PolicyDocument::new`], which enforces the structural
/// rules that cannot be expressed in the types: only the global document may
/// lock, no construct may name a built-in invariant, ids are unique, and
/// exceptions are exact and scoped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PolicyDocument {
    origin: Origin,
    rules: Vec<Rule>,
    exceptions: Vec<PolicyException>,
}

impl PolicyDocument {
    /// Build a document, refusing anything §4.4 forbids.
    pub fn new(
        origin: Origin,
        rules: Vec<Rule>,
        exceptions: Vec<PolicyException>,
    ) -> Result<PolicyDocument, PolicyError> {
        let mut seen: Vec<&str> = Vec::new();
        for rule in &rules {
            if rule.locked && origin != Origin::Global {
                return Err(PolicyError::LockedOutsideGlobal {
                    origin,
                    rule_id: rule.id.clone(),
                });
            }
            if seen.contains(&rule.id.as_str()) {
                return Err(PolicyError::Invalid {
                    location: format!("{origin}.rules"),
                    detail: format!(
                        "rule id {:?} is used twice; ids name rules in `explain` \
                         and in approval requests, so two rules sharing one would \
                         be indistinguishable in an audit",
                        rule.id
                    ),
                });
            }
            seen.push(&rule.id);
        }

        for exception in &exceptions {
            // An exception naming an action outside the taxonomy is *not*
            // refused here: it is inert, because evaluation floors an unknown
            // action at `deny` and clamps every exception by that floor. Making
            // it an error would only move a typo's discovery from `explain`,
            // where the author is already looking, to load time.
            if exception.scope.is_everywhere() {
                return Err(PolicyError::Invalid {
                    location: format!("{origin}.exceptions"),
                    detail: format!(
                        "exception {:?} has no scope; an unscoped exception is a \
                         rule with an expiry, and §4.4 applies an exception only \
                         when it matches exactly",
                        exception.id
                    ),
                });
            }
            if seen.contains(&exception.id.as_str()) {
                return Err(PolicyError::Invalid {
                    location: format!("{origin}.exceptions"),
                    detail: format!("id {:?} is used twice", exception.id),
                });
            }
            seen.push(&exception.id);
        }

        Ok(PolicyDocument {
            origin,
            rules,
            exceptions,
        })
    }

    /// Build without the structural checks.
    ///
    /// Exists for the tests that have to construct an *invalid* document in
    /// order to prove that evaluation refuses it anyway — defence in depth is
    /// only demonstrable if the first defence can be bypassed.
    pub fn new_unchecked(
        origin: Origin,
        rules: Vec<Rule>,
        exceptions: Vec<PolicyException>,
    ) -> PolicyDocument {
        PolicyDocument {
            origin,
            rules,
            exceptions,
        }
    }

    /// Which document this is.
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// Its rules, in file order.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Its exceptions, in file order.
    pub fn exceptions(&self) -> &[PolicyException] {
        &self.exceptions
    }
}

/// The three documents that make up one evaluation's policy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedPolicy {
    global: PolicyDocument,
    project: PolicyDocument,
    task: PolicyDocument,
}

impl ResolvedPolicy {
    /// Assemble the three documents, each of which may be absent.
    ///
    /// Refuses a document filed under the wrong origin: the ceiling is read from
    /// the global slot, so a project document filed as global would be a project
    /// that can lock.
    pub fn new(
        global: Option<PolicyDocument>,
        project: Option<PolicyDocument>,
        task: Option<PolicyDocument>,
    ) -> Result<ResolvedPolicy, PolicyError> {
        let slots = [
            (global.as_ref(), Origin::Global),
            (project.as_ref(), Origin::Project),
            (task.as_ref(), Origin::Task),
        ];
        for (document, expected) in slots {
            if let Some(document) = document
                && document.origin() != expected
            {
                return Err(PolicyError::Invalid {
                    location: "policy".to_string(),
                    detail: format!(
                        "a {} document was supplied in the {expected} slot",
                        document.origin()
                    ),
                });
            }
        }
        Ok(ResolvedPolicy {
            global: global.unwrap_or_else(|| PolicyDocument {
                origin: Origin::Global,
                ..PolicyDocument::default()
            }),
            project: project.unwrap_or_else(|| PolicyDocument {
                origin: Origin::Project,
                ..PolicyDocument::default()
            }),
            task: task.unwrap_or_else(|| PolicyDocument {
                origin: Origin::Task,
                ..PolicyDocument::default()
            }),
        })
    }

    /// The global document — **the only source of the Stage-1 ceiling**.
    pub fn global(&self) -> &PolicyDocument {
        &self.global
    }

    /// The project document.
    pub fn project(&self) -> &PolicyDocument {
        &self.project
    }

    /// The task document.
    pub fn task(&self) -> &PolicyDocument {
        &self.task
    }

    /// The three documents, global first.
    pub fn documents(&self) -> [&PolicyDocument; 3] {
        [&self.global, &self.project, &self.task]
    }
}

/// The four invariants §4.4 makes **not configurable at all**.
///
/// > never write outside the run workspace · never print a value matching a
/// > secret detector · never push to a remote · never operate on an unregistered
/// > repository
///
/// They are an enum with a `const` list and no constructor, and no
/// [`PolicyDocument`] field refers to them. That is what "not configurable"
/// means here: there is no data path from a file to this type, so weakening one
/// requires editing this source file rather than editing a policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinInvariant {
    /// Never write outside the run workspace.
    NeverWriteOutsideRunWorkspace,
    /// Never print a value matching a secret detector.
    NeverPrintASecretMatchingValue,
    /// Never push to a remote.
    NeverPushToARemote,
    /// Never operate on an unregistered repository.
    NeverOperateOnAnUnregisteredRepository,
}

/// A built-in invariant that applies to one evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedInvariant {
    /// Which invariant.
    pub invariant: BuiltinInvariant,
    /// What it contributes. Always [`Effect::Deny`] before the deny cap.
    pub effect: Effect,
    /// The fact keys it rests on. Empty when it rests on the action alone.
    pub supporting: Vec<String>,
}

impl BuiltinInvariant {
    /// All four, in §4.4's order.
    pub const ALL: &'static [BuiltinInvariant] = &[
        BuiltinInvariant::NeverWriteOutsideRunWorkspace,
        BuiltinInvariant::NeverPrintASecretMatchingValue,
        BuiltinInvariant::NeverPushToARemote,
        BuiltinInvariant::NeverOperateOnAnUnregisteredRepository,
    ];

    /// The identifier used in `explain` and in the cap record.
    pub fn id(&self) -> &'static str {
        match self {
            BuiltinInvariant::NeverWriteOutsideRunWorkspace => {
                "builtin.never-write-outside-run-workspace"
            }
            BuiltinInvariant::NeverPrintASecretMatchingValue => {
                "builtin.never-print-a-secret-matching-value"
            }
            BuiltinInvariant::NeverPushToARemote => "builtin.never-push-to-a-remote",
            BuiltinInvariant::NeverOperateOnAnUnregisteredRepository => {
                "builtin.never-operate-on-an-unregistered-repository"
            }
        }
    }

    /// The actions this invariant governs unconditionally.
    ///
    /// Empty for the two invariants that are conditioned on a fact rather than
    /// on the action — see [`BuiltinInvariant::applies`].
    pub fn actions(&self) -> &'static [Action] {
        match self {
            BuiltinInvariant::NeverWriteOutsideRunWorkspace => {
                &[Action::FilesystemWriteOutsideWorkspace]
            }
            BuiltinInvariant::NeverPushToARemote => &[Action::GitPush, Action::GitForcePush],
            _ => &[],
        }
    }

    /// Which invariant, if any, governs `action` unconditionally.
    ///
    /// Used by the loader to refuse a policy file whose exception names one: an
    /// exception is the only construct that can *lower* an effect, so it is the
    /// only place a built-in could be aimed at. Evaluation does not rely on that
    /// refusal — it clamps every exception by the invariants regardless — but a
    /// file that tries deserves to be told rather than silently ignored.
    pub fn governing_action(action: &Action) -> Option<BuiltinInvariant> {
        BuiltinInvariant::ALL
            .iter()
            .copied()
            .find(|invariant| invariant.actions().contains(action))
    }

    /// Whether this invariant applies to one evaluation, and what it rests on.
    ///
    /// The two fact-conditioned invariants are conditioned rather than
    /// unconditional for a stated reason:
    ///
    /// * **the secret detector** governs *whatever* action would emit the value,
    ///   so it is keyed on the scan result rather than on an action name;
    /// * **the unregistered repository** is a property of the repository, not of
    ///   the operation, and it denies everything while it holds. Absence of the
    ///   fact means "not asserted here" — registration is enforced at workspace
    ///   creation by `conductor_git::assert_registrable`, and this entry exists
    ///   so that no policy file can *override* that enforcement.
    pub fn applies(&self, action: &Action, facts: &FactSet) -> Option<AppliedInvariant> {
        let supporting = match self {
            BuiltinInvariant::NeverWriteOutsideRunWorkspace
            | BuiltinInvariant::NeverPushToARemote => {
                if self.actions().contains(action) {
                    Vec::new()
                } else {
                    return None;
                }
            }
            BuiltinInvariant::NeverPrintASecretMatchingValue => {
                if facts.contains(crate::policy::facts::key::SECRET_MATCH) {
                    vec![crate::policy::facts::key::SECRET_MATCH.to_string()]
                } else {
                    return None;
                }
            }
            BuiltinInvariant::NeverOperateOnAnUnregisteredRepository => {
                match facts.get(crate::policy::facts::key::REPOSITORY_REGISTERED) {
                    Some(fact) if fact.value == "false" => {
                        vec![crate::policy::facts::key::REPOSITORY_REGISTERED.to_string()]
                    }
                    _ => return None,
                }
            }
        };
        Some(AppliedInvariant {
            invariant: *self,
            effect: Effect::Deny,
            supporting,
        })
    }
}

impl fmt::Display for BuiltinInvariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_join_is_max_and_the_meet_is_min() {
        assert_eq!(Effect::Allow.join(Effect::Deny), Effect::Deny);
        assert_eq!(
            Effect::RequireApproval.join(Effect::Allow),
            Effect::RequireApproval
        );
        assert_eq!(Effect::Deny.meet(Effect::Allow), Effect::Allow);
    }

    #[test]
    fn an_unknown_action_floors_at_deny_and_a_known_one_does_not() {
        assert_eq!(Action::parse("nope").floor(), Effect::Deny);
        assert_eq!(Action::parse("git.push").floor(), Effect::Allow);
    }

    #[test]
    fn action_names_round_trip_for_the_whole_taxonomy() {
        for name in Action::KNOWN {
            let action = Action::parse(name);
            assert!(action.is_known());
            assert_eq!(action.as_str(), *name);
        }
    }

    #[test]
    fn a_prefix_pattern_matches_only_on_a_segment_boundary() {
        let pattern = ActionPattern::parse("dependency.*").expect("pattern");
        assert!(pattern.matches(&Action::DependencyAddRuntime));
        assert!(!pattern.matches(&Action::LockfileModify));
        // `dependency` is a prefix of `dependency.add.runtime` but the pattern
        // must not match a name that merely starts with the same letters.
        assert!(!pattern.matches(&Action::parse("dependencyx.add")));
    }

    #[test]
    fn a_malformed_pattern_is_refused_rather_than_half_honoured() {
        for bad in ["dep*ndency", "*.push", "", "   ", "a.*.b"] {
            assert!(
                ActionPattern::parse(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_scope_key_the_context_lacks_is_a_mismatch_and_not_a_wildcard() {
        let scope = Scope::from_pairs([("run".to_string(), "r-1".to_string())]);
        assert_eq!(scope.matches(&BTreeMap::new()), {
            ScopeMatch::Mismatch {
                key: "run".to_string(),
                expected: "r-1".to_string(),
                actual: None,
            }
        });
    }

    #[test]
    fn only_deterministic_facts_may_carry_a_deny() {
        assert!(FactSource::Deterministic.may_carry_a_deny());
        assert!(!FactSource::ModelAssisted.may_carry_a_deny());
        assert!(!FactSource::Human.may_carry_a_deny());
    }
}
