//! The plan document — master plan §3.2, §3.6, §5.2.
//!
//! # A plan is a data structure, not a document with structure in it
//!
//! §3.6: *"A **plan is a data structure** — milestones containing slices
//! containing tasks, each with criteria bound to named checks, scope globs and
//! dependencies. Prose is confined to `rationale:` and `objective:` fields."*
//! That sentence is the whole shape of this module. [`Milestone`] contains
//! [`Slice`] contains [`Task`], and the only `String` fields a human writes
//! freely are `objective`, `rationale` and a criterion's `statement`.
//!
//! # Why there is no `state:` field
//!
//! §5.2 gives a plan five states, and none of them is a field here. §3.3 is the
//! reason: *"`.conductor/` lives in the repo, therefore inside the agent's
//! workspace, therefore an agent **can write**
//! `.conductor/plans/v3/APPROVED` in its own clone."* A `state:` key in
//! `plan.yaml` would be a second, easier way to do the same thing — an agent
//! writing `state: APPROVED` into a file it already has write access to.
//!
//! So a plan's state is **not reachable from a parsed document at all**, and
//! this module does not define a type for it. §5.1's `plan_version.state`
//! column is spelled once, by [`conductor_core::PlanVersionState`], and that is
//! the only spelling: [`crate::plan::ledger`] reads and writes it, the
//! `APPROVED` sidecar records the approval in git, and the two are compared
//! rather than merged — §3.3: *"If file and store disagree, **execution
//! halts** — it is never resynced."*
//!
//! An earlier draft of this module carried its own `PlanState` enum, on the
//! reasoning that *"the ledger that will own it must not get to invent its own
//! spelling"*. That reasoning was sound and its conclusion was wrong:
//! `conductor_core` had already owned the spelling since S1, so the second enum
//! was not a guard against divergence, it **was** the divergence — the "two
//! representations that silently disagree the first time someone edits the
//! wrong one" §3.6 forbids. Deleted at S11 when the ledger was built and used
//! the core type.
//!
//! A `state:` key in a plan file is therefore simply an unknown key, and
//! unknown keys are inert (see below).
//!
//! # No `deny_unknown_fields`
//!
//! The rule and the reason: a plan written for a later Conductor must still load,
//! and the fields it does not know about are the later Conductor's business. S5's
//! task spec stated it first; S12 deleted that type, so the statement lives here
//! now — beside the only document it still governs. §3.2 requires an approved
//! plan to travel with the repository to another machine, and a machine running
//! an older Conductor that refuses to read the plan is a machine where §3.2 is
//! false.
//!
//! The obvious hole — an ignored key is a key an agent can add for free — is
//! closed in [`crate::plan::hash`] rather than here: the content hash is taken
//! over the **whole parsed document**, not over the subset this version models,
//! so an unmodelled key still invalidates an approval.
//!
//! `verify::profile` takes the third option for its own file (accept, and hand
//! back a warning) because a mistyped verification key silently disables a
//! check. That does not apply to a plan: a plan key this version ignores cannot
//! silently weaken anything, because everything a plan controls — scope,
//! bindings, dependencies — is read from keys this version *does* model, and
//! §3.7's rules refuse a plan whose modelled content is incomplete.

use serde::{Deserialize, Serialize};

/// Anything that stops a plan document from loading.
///
/// Every variant is a refusal. There is no "carry on with defaults" path,
/// because the default plan is the empty plan, and the empty plan validates.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// A plan file could not be read.
    #[error("plan file {path}: {source}")]
    Io {
        /// The path.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The YAML did not parse.
    #[error("plan is not valid YAML: {0}")]
    Yaml(String),
    /// The YAML parsed but does not describe a plan.
    #[error("plan is invalid: {0}")]
    Invalid(String),
}

/// The file's top level: a single `plan:` key.
///
/// The wrapper matches `.conductor/policy.yaml`'s `policy:` and
/// `.conductor/verification.yaml`'s `verification:`, so an author reading the
/// three files sees one convention. It also leaves room for a sibling key
/// later without the ambiguity of "is this a plan field or a file field?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDocument {
    /// The plan itself.
    pub plan: Plan,
}

/// One version of a project's plan — `.conductor/plans/vN/plan.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The project's stable plan id, e.g. `p-conductor`.
    ///
    /// Constant across versions. It is what says "v3 and v4 are two versions of
    /// the same plan" rather than two unrelated plans, which is what §5.1's
    /// `UNIQUE(project_id, version)` and S11's supersession rule both need.
    pub id: String,
    /// `N` from `.conductor/plans/vN/`.
    ///
    /// Not defaulted. A plan with no version cannot be superseded by a later
    /// one, cannot be named in §3.4's `Conductor-Plan: v3@blake3:…` trailer,
    /// and cannot answer acceptance row 21's question — "does this in-flight
    /// run still belong to v3?".
    pub version: u32,
    /// What the plan is for. Prose (§3.6).
    #[serde(default)]
    pub objective: String,
    /// The milestones, in declaration order.
    #[serde(default)]
    pub milestones: Vec<Milestone>,
}

impl Plan {
    /// Every task in the plan, in declaration order.
    ///
    /// Declaration order rather than dependency order: a topological order is
    /// what *materialisation* needs, and materialisation is not part of this
    /// slice's pure core. Everything here — validation, hashing, reporting —
    /// wants the order the author wrote, because that is the order they will
    /// read an error message against.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.milestones
            .iter()
            .flat_map(|m| m.slices.iter())
            .flat_map(|s| s.tasks.iter())
    }

    /// Every slice, in declaration order.
    pub fn slices(&self) -> impl Iterator<Item = &Slice> {
        self.milestones.iter().flat_map(|m| m.slices.iter())
    }

    /// The slice a task belongs to, by task id.
    pub fn slice_of(&self, task_id: &str) -> Option<&Slice> {
        self.slices()
            .find(|slice| slice.tasks.iter().any(|task| task.id == task_id))
    }
}

/// A milestone — `M-01`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Milestone {
    /// Stable id (§3.6).
    pub id: String,
    /// A human-readable name.
    #[serde(default)]
    pub title: String,
    /// The slices it contains, in declaration order.
    #[serde(default)]
    pub slices: Vec<Slice>,
}

/// A slice — `S-05`. The unit a review boundary lands on (§6.5's `context`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slice {
    /// Stable id (§3.6).
    pub id: String,
    /// A human-readable name.
    #[serde(default)]
    pub title: String,
    /// The tasks it contains, in declaration order.
    #[serde(default)]
    pub tasks: Vec<Task>,
}

/// A task — `T-0012`. The unit a run executes.
///
/// This is the shape S5's task spec said it was standing in for: *"Deliberately
/// absent: […] dependencies and acceptance-criterion bindings (S11 — and
/// inventing a half-version of those here is exactly what would have to be
/// unpicked)."* S12 deleted that stopgap once `task run` read this type instead,
/// so this is now the only answer to "what is a task?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Stable id (§3.6). Appears in run branches, findings and commit trailers.
    pub id: String,
    /// What the task is for. Prose (§3.6). Reaches the agent, and the reviewer.
    #[serde(default)]
    pub objective: String,
    /// Why it is worth doing. Prose (§3.6). Reaches the reviewer.
    #[serde(default)]
    pub rationale: String,
    /// Task ids that must reach `COMPLETE` before this one becomes `READY`
    /// (§5.2's "deps met").
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// What the task may and may not touch.
    #[serde(default)]
    pub scope: TaskScope,
    /// Which `verification.yaml` profile proves it (§4.5).
    #[serde(default)]
    pub verification_profile: String,
    /// How many work attempts it gets. §5.1's column defaults to 3.
    #[serde(default = "default_attempt_budget")]
    pub attempt_budget: i64,
    /// What "done" means, and what proves each part of it.
    #[serde(default)]
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    /// Decision ids (`D-0007`) whose argument this task needs — §6.5's
    /// *"explicit refs"*.
    ///
    /// # Why this field exists, and why the other half of §6.5's sentence does
    /// not
    ///
    /// §6.5 says a packet carries decisions *"selected by touching the task's
    /// scope globs **or explicit refs** — never 'all accepted decisions'"*, and
    /// names two mechanisms. Only this one is implementable: matching a
    /// decision against a task's scope globs requires the **decision** to
    /// declare a scope, and §3.6 fixes a decision's frontmatter at four fields
    /// (`id`, `status`, `supersedes`, `date`) with `deny_unknown_fields`, on the
    /// reasoning that *"an unknown key here is not tomorrow's feature"*. Adding
    /// `scope:` there would contradict §3.6 **and** change every existing
    /// decision's content-hash preimage.
    ///
    /// So the reference points the other way: the plan — which already tolerates
    /// unknown keys, and which a human writes and approves — names the decisions
    /// a task needs. See ADR-0016.
    ///
    /// Plain strings rather than a validated id type, for
    /// [`Task::actions`]'s reason: whether an id resolves is a question about a
    /// *set of documents*, not about this struct, and the packet builder is what
    /// has both halves. A reference to a decision nothing defines is refused
    /// there, fail-closed, rather than silently dropped from the packet.
    #[serde(default)]
    pub decisions: Vec<String>,
    /// The actions this task is authorized to perform, named with §4.4's
    /// taxonomy strings.
    ///
    /// §4.3's binding rule — *"a task whose policy can produce an approval
    /// gate may not run unattended below tier A"* — is decided by
    /// `approval::gate::unattended_requirements(policy, actions)`. That
    /// function has taken an `actions` parameter since S9 with nothing in
    /// the plan able to supply it, because a task could not declare what it
    /// does. This field is that declaration.
    ///
    /// Plain strings, not `policy::model::Action`, for the same reason
    /// [`Plan`] has no `deny_unknown_fields`: a name outside §4.4's taxonomy
    /// still has to load, because `Action::parse` is infallible and turns it
    /// into `Action::Unknown`, which `Action::floor()` denies at evaluation
    /// rather than at parse. Whether a string is *taxonomy-known* is
    /// therefore not this module's question — [`crate::plan::validate`]
    /// only refuses a blank one, on the same reasoning it refuses a blank
    /// id: a blank action addresses nothing.
    #[serde(default)]
    pub actions: Vec<String>,
    /// §4.2's `execution_requirements:` YAML block, exactly as
    /// `.conductor/project.yaml` would write it — the *"or per-task
    /// override"* §4.2 names.
    ///
    /// `None` when the task overrides nothing, which is deliberately not the
    /// same case as a block that parses to nothing: `enforce::launch`'s
    /// `requirements_for` already treats a column that holds bytes but
    /// yields no requirement as a refusal rather than "no requirements",
    /// because that is what a mis-nested override looks like, and
    /// mis-nesting must not silently read as "nothing is gated".
    /// [`crate::plan::validate`] applies the same rule at validate time, so
    /// a plan whose per-task override is broken is refused before a human
    /// approves it rather than at the launch it would then refuse forever.
    ///
    /// A `String`, not a parsed `ExecutionRequirements`, for the same reason
    /// `actions` is `Vec<String>` and not `Vec<Action>`: the dialect is
    /// already owned by one parser —
    /// `crate::policy::eligibility::ExecutionRequirements::parse_yaml` — and
    /// giving this module a second one would be a second place the two could
    /// drift apart.
    #[serde(default)]
    pub execution_requirements: Option<String>,
}

fn default_attempt_budget() -> i64 {
    3
}

/// §6.5's `scope:` block.
///
/// Both lists are plain glob strings and are **not** matched here. §3.7 refuses
/// "scope globs matching no path", which needs a repository to match against;
/// this slice's core is pure, so that rule is not implemented — see
/// [`crate::plan`]'s module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskScope {
    /// Paths the task may write.
    #[serde(default)]
    pub allowed_globs: Vec<String>,
    /// Paths the task may never write, whatever `allowed_globs` says.
    ///
    /// §6.5's example lists `.conductor/**` here. That entry is a courtesy to
    /// the reader, **not** the enforcement: §3.3 makes `.conductor/**` an
    /// always-forbidden write scope rejected at reconciliation regardless of
    /// what any plan says, precisely because the plan is a file the agent can
    /// edit.
    #[serde(default)]
    pub forbidden_globs: Vec<String>,
}

/// One acceptance criterion — §6.5's `{id, statement, verified_by}`.
///
/// §4.5's completion criterion 5 is *"every acceptance criterion binds to ≥1
/// passing check"*. A criterion with an empty `verified_by` satisfies that
/// vacuously, which is why §3.7 refuses one outright unless it is declared
/// [`manual`](AcceptanceCriterion::manual).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    /// Stable id, unique within its task — `AC-1`.
    pub id: String,
    /// What must be true. Prose, quoted verbatim into packets and reviews.
    #[serde(default)]
    pub statement: String,
    /// Check ids from `verification.yaml` that prove it.
    #[serde(default)]
    pub verified_by: Vec<String>,
    /// This criterion needs a person, and says so.
    ///
    /// §3.7's escape hatch: *"mark it `manual: true`, which forces a review
    /// boundary."* It does not make the criterion cheaper — it makes it honest.
    /// A manual criterion can never be satisfied by an agent's report, and a
    /// plan containing one is reported as requiring human review by
    /// [`crate::plan::ValidatedPlan::requires_human_review`].
    #[serde(default)]
    pub manual: bool,
}

impl AcceptanceCriterion {
    /// Whether anything mechanical could ever satisfy this criterion.
    ///
    /// This is §3.7's actual question. A criterion that names no check and is
    /// not declared `manual` claims to be automatically completable while
    /// offering nothing that could complete it, and the only thing left that
    /// can say "done" is the agent's own report — which §4.5 and §4.8 make
    /// evidence, never authority.
    pub fn is_mechanically_bound(&self) -> bool {
        !self.verified_by.is_empty()
    }
}

/// Parse a plan document.
pub fn parse(yaml: &str) -> Result<Plan, PlanError> {
    // A hand-walked `serde_yaml::Value` is what `policy::load` and
    // `verify::profile` both do, for reasons that do not apply here: policy
    // must refuse unknown keys, and a profile must report them. A plan does
    // neither — unknown keys load and are inert — so a derive is the honest
    // implementation, and it keeps the field list in one place instead of two.
    let document: PlanDocument = serde_yaml::from_str(yaml).map_err(|error| {
        let text = error.to_string();
        // serde reports both "this is not YAML" and "this YAML is not a plan"
        // through one type. Splitting them matters to whoever reads the error:
        // one is a syntax mistake, the other is a shape mistake.
        if text.contains("missing field") || text.contains("invalid type") {
            PlanError::Invalid(text)
        } else {
            PlanError::Yaml(text)
        }
    })?;
    Ok(document.plan)
}

/// Read a plan document from disk.
pub fn load(path: &std::path::Path) -> Result<Plan, PlanError> {
    let text = std::fs::read_to_string(path).map_err(|source| PlanError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "plan:\n  id: p-x\n  version: 1\n";

    #[test]
    fn a_plan_with_no_milestones_loads_as_an_empty_plan() {
        let plan = parse(MINIMAL).expect("parses");
        assert_eq!(plan.id, "p-x");
        assert_eq!(plan.version, 1);
        assert_eq!(plan.tasks().count(), 0);
    }

    #[test]
    fn the_attempt_budget_defaults_to_the_schemas_three() {
        let yaml = "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
                    slices:\n        - id: S-01\n          tasks:\n            - id: T-0001\n";
        let plan = parse(yaml).expect("parses");
        assert_eq!(plan.tasks().next().expect("a task").attempt_budget, 3);
    }

    #[test]
    fn a_task_declaring_no_actions_authorizes_nothing() {
        let yaml = "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
                    slices:\n        - id: S-01\n          tasks:\n            - id: T-0001\n";
        let plan = parse(yaml).expect("parses");
        assert!(plan.tasks().next().expect("a task").actions.is_empty());
    }

    #[test]
    fn a_declared_action_round_trips_through_parse() {
        let yaml = "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
                    slices:\n        - id: S-01\n          tasks:\n            - id: T-0001\n              \
                    actions: [git.push]\n";
        let plan = parse(yaml).expect("parses");
        assert_eq!(
            plan.tasks().next().expect("a task").actions,
            vec!["git.push".to_string()]
        );
    }

    #[test]
    fn a_task_declaring_no_execution_requirements_override_has_none() {
        let yaml = "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
                    slices:\n        - id: S-01\n          tasks:\n            - id: T-0001\n";
        let plan = parse(yaml).expect("parses");
        assert_eq!(
            plan.tasks().next().expect("a task").execution_requirements,
            None
        );
    }

    #[test]
    fn a_declared_execution_requirements_block_round_trips_through_parse() {
        // The same dialect `ExecutionRequirements::parse_yaml` already accepts
        // for `.conductor/project.yaml`: the block carries its own
        // `execution_requirements:` key, not a bare mapping — see this field's
        // doc comment for why the model stores it as text rather than parsing
        // it here.
        let yaml = "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
                    slices:\n        - id: S-01\n          tasks:\n            - id: T-0001\n              \
                    execution_requirements: |\n                execution_requirements:\n                  \
                    control_surface: hard\n";
        let plan = parse(yaml).expect("parses");
        assert_eq!(
            plan.tasks().next().expect("a task").execution_requirements,
            Some("execution_requirements:\n  control_surface: hard\n".to_string())
        );
    }

    #[test]
    fn a_missing_version_is_a_shape_error_and_not_a_syntax_error() {
        let error = parse("plan:\n  id: p-x\n").expect_err("refused");
        assert!(matches!(error, PlanError::Invalid(_)), "{error}");
    }

    #[test]
    fn a_criterion_with_no_checks_is_not_mechanically_bound() {
        let bound = AcceptanceCriterion {
            id: "AC-1".to_string(),
            statement: "…".to_string(),
            verified_by: vec!["typecheck".to_string()],
            manual: false,
        };
        let unbound = AcceptanceCriterion {
            verified_by: Vec::new(),
            ..bound.clone()
        };
        assert!(bound.is_mechanically_bound());
        assert!(!unbound.is_mechanically_bound());
        // `manual` is not a *binding*. It is a declaration that there will not
        // be one, which is a different claim and must not be confused with it.
        let manual = AcceptanceCriterion {
            manual: true,
            ..unbound.clone()
        };
        assert!(!manual.is_mechanically_bound());
    }
}
