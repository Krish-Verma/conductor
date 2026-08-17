//! `plan validate` — master plan §3.7.
//!
//! # What this refuses, and why each refusal is not optional
//!
//! §3.7 lists what `plan validate` refuses:
//!
//! > Duplicate IDs · dangling `verified_by` references · verification IDs absent
//! > from `verification.yaml` · forward dependencies · scope globs matching no
//! > path · **any acceptance criterion not bound to at least one check**.
//! >
//! > That last one matters most: an unbound criterion is the mechanism by which
//! > a task reaches `COMPLETE` on an agent's word. Escape hatch: mark it
//! > `manual: true`, which forces a review boundary.
//!
//! The last one is the reason the rest exist. §4.5's completion criterion 5 is
//! *"every acceptance criterion binds to ≥1 passing check"*, and a criterion
//! with no bindings satisfies that **vacuously**: zero of zero checks passed, so
//! the task completes and nothing ran. The only thing left saying "done" is the
//! agent's report, which §4.8 makes evidence and never authority. So an unbound
//! criterion is a hard error, not a warning — a warning would be a finding
//! attached to a task that had already completed.
//!
//! [`AcceptanceCriterion::manual`](super::AcceptanceCriterion::manual) is the
//! escape hatch, and it is not a way to skip the check. A validated plan
//! carrying a manual criterion reports
//! [`ValidatedPlan::requires_human_review`], and the criterion is listed by
//! [`ValidatedPlan::manual_criteria`] — so "a person has to read this" is data
//! the ledger and the review bridge can act on, not a comment in a YAML file.
//!
//! # Every defect, not the first one
//!
//! `policy::explain` exists because negative results are what people debug. The
//! same argument applies to a plan: returning the first defect turns fixing a
//! plan into an N-round game against a validator that already knows all N
//! answers. [`validate`] runs every rule and returns every defect, and each
//! defect names the offending id and says what is wrong with it.
//!
//! # Two directions
//!
//! A validator that refuses everything passes every rejection test ever written
//! for it. The rules below are therefore written as *predicates over named ids*
//! rather than as a whitelist of accepted shapes, and the test suite's positive
//! control is the same plan every rejection fixture is derived from.
//!
//! # What §3.7 lists and this module does not implement
//!
//! * **Scope globs matching no path.** Needs a repository to match against.
//!   This module is pure — it takes a parsed plan and a set of check ids — so
//!   the rule belongs with the code that has a working tree in its hands.
//! * **Forward dependencies.** §3.7's phrase is about *declaration order*: a
//!   dependency on a task declared later in the file. That is a stricter and
//!   different rule from the cycle detection implemented here (every cycle is a
//!   forward dependency, but a plan can order its tasks freely and still be a
//!   perfectly executable DAG). Implementing the literal reading would refuse
//!   plans that §5.2's task machine executes without complaint, so it is
//!   recorded as an open question rather than guessed at.
//!
//! # Two rules §3.7 does not name, added for `Task::actions` and
//! `Task::execution_requirements` (S11 task 1)
//!
//! §3.7's list is closed, and neither of these appears in it — which is why
//! neither is treated as a §3.7 rule below. Both exist for the same reason
//! every other rule here does: an unrefused defect that reaches an approved
//! plan is a defect nobody can act on until a run has already tried and
//! failed against it.
//!
//! * [`empty_actions`] refuses a blank `actions` entry, on exactly
//!   [`empty_ids`]'s reasoning: a blank string addresses nothing.
//!   **Deliberately not covered:** an action name outside §4.4's taxonomy.
//!   `Action::parse` is infallible and turns it into `Action::Unknown`, which
//!   `Action::floor()` already denies when the action is evaluated. Refusing
//!   it here too would be a *second* gate on the same question, and a second
//!   gate can drift from the first — e.g. the taxonomy grows and only one of
//!   the two is updated. So an unrecognised action is data the plan is
//!   allowed to carry; denying its use is evaluation's job.
//! * [`malformed_execution_requirements`] refuses a per-task
//!   `execution_requirements` override that does not parse, or that parses
//!   but names no `execution_requirements:` mapping at all.
//!   `enforce::launch`'s `requirements_for` already treats the second case as
//!   a refusal for the durable column, rather than as "no requirements" —
//!   its doc comment gives the reason: a mis-nested override must not
//!   silently read as "nothing is gated". A plan carrying an override that
//!   would refuse every launch through that path is caught here instead,
//!   while a human can still read the file.
//!
//! # Two more §3.7 does not name, inherited from the task spec S12 deleted
//!
//! S5's `.conductor/task.yaml` had five refusals of its own, and S12 deleted
//! the type that held them when `conductor task run` moved onto the plan ledger
//! (ADR-0017). Three of the five had no successor. Two are restored here,
//! because a plan is now the *only* document that answers "what is a task?" and
//! the failures they prevent are still reachable:
//!
//! * [`blank_objective`] — the spec's reason was *"the objective is the only
//!   thing in the file that tells an agent what to do"*, and S10 measured the
//!   consequence: `codex exec` with no prompt argument **blocks forever reading
//!   stdin**, so `CodexAgent::command` refuses an empty prompt. Without this
//!   rule, a plan with a blank objective is approved, its tasks are
//!   materialized, a workspace is cloned — and *then* the launch is refused.
//!   Fail-closed, and expensively late. §3.7's whole philosophy is to refuse the
//!   plan rather than the run.
//! * [`non_positive_attempt_budget`] — §5.1's column defaults to 3 and §4.6
//!   bounds repair by it. `attempt_budget: 0` is a task that can never launch,
//!   which is a stall no state in §5.2 explains.
//!
//! **The third is deliberately not restored.** The spec refused an empty
//! `scope`, and that refusal *does* have a successor: `conductor_git`'s
//! `Scope::contains` is explicit that it "fails closed — an empty scope contains
//! nothing, so a task that forgot to declare one halts for review rather than
//! authorising everything", and a task that declares no scope inherits
//! `project.yaml`'s `scope_defaults` before it ever gets there. Adding a rule
//! here would be a second gate on a question already answered safely, and §3.7's
//! own version of it — "scope globs matching no path" — needs a working tree and
//! stays deferred.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use super::model::{AcceptanceCriterion, Plan, Task};
use crate::policy::eligibility::ExecutionRequirements;
use crate::verify::profile::Profile;

/// Which kind of id a defect is about.
///
/// Ids are scoped by kind — §3.6's `M-01`, `S-05`, `T-0012` are three
/// namespaces, and a criterion's `AC-1` is scoped to its task, which is why
/// every task in §6.5's packet example can start at `AC-1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdKind {
    /// `M-01`.
    Milestone,
    /// `S-05`.
    Slice,
    /// `T-0012`.
    Task,
    /// `AC-1`, scoped to its task.
    Criterion,
}

impl IdKind {
    /// The word used in a message a human reads.
    pub fn as_str(&self) -> &'static str {
        match self {
            IdKind::Milestone => "milestone",
            IdKind::Slice => "slice",
            IdKind::Task => "task",
            IdKind::Criterion => "acceptance criterion",
        }
    }
}

impl fmt::Display for IdKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One reason a plan is refused.
///
/// Every variant carries the id it is about. A refusal that says only "the plan
/// is invalid" makes a human diff their plan against a specification; a refusal
/// that names `T-0002` and `AC-3` makes them open one file at one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDefect {
    /// An id is empty or whitespace.
    EmptyId {
        /// What kind of thing has no id.
        kind: IdKind,
        /// Where it is, since the id itself cannot be quoted.
        location: String,
    },
    /// One id names two things.
    DuplicateId {
        /// What kind of id.
        kind: IdKind,
        /// The id.
        id: String,
        /// Where the second one is.
        location: String,
    },
    /// A `depends_on` entry names a task the plan does not declare.
    DanglingDependency {
        /// The task that declares the dependency.
        task: String,
        /// The id it names.
        depends_on: String,
    },
    /// A `depends_on` entry names a task declared later in the document.
    ///
    /// §3.7 names this refusal separately from a cycle, and it is a stricter
    /// rule: a plan can point forward and still be a perfectly executable DAG.
    /// Keeping it makes acyclicity **structural** — a graph whose every edge
    /// points backwards cannot contain a cycle — and makes a plan readable in
    /// the order a human will execute it.
    ForwardDependency {
        /// The task that declares the dependency.
        task: String,
        /// The later-declared id it names.
        depends_on: String,
    },
    /// Tasks that transitively depend on each other.
    DependencyCycle {
        /// The cycle, starting and ending at the same id.
        cycle: Vec<String>,
    },
    /// A `verified_by` entry names a check `verification.yaml` does not define.
    UnknownVerificationId {
        /// The task.
        task: String,
        /// The criterion.
        criterion: String,
        /// The check id that resolves to nothing.
        check: String,
    },
    /// §3.7's most important refusal.
    UnboundCriterion {
        /// The task.
        task: String,
        /// The criterion.
        criterion: String,
        /// What it claims, quoted so the author recognises it.
        statement: String,
    },
    /// An `actions` entry is empty or whitespace.
    ///
    /// Not a §3.7 rule — see this module's docs — but the same reasoning as
    /// [`PlanDefect::EmptyId`]: a blank string addresses nothing, and here it
    /// is the string `approval::gate::unattended_requirements` reads to
    /// decide whether the task can produce an approval gate (§4.3).
    EmptyAction {
        /// The task that declares it.
        task: String,
        /// Where in the list, since a blank entry cannot be quoted.
        location: String,
    },
    /// A per-task `execution_requirements` override that does not parse, or
    /// parses to no requirement at all.
    ///
    /// Not a §3.7 rule — see this module's docs. Mirrors
    /// `enforce::launch::requirements_for`'s refusal for the same text once
    /// it reaches the durable column: "present and meaningless" is refused
    /// rather than read as "no requirements", because that is what a
    /// mis-nested override looks like.
    MalformedExecutionRequirements {
        /// The task whose override is broken.
        task: String,
        /// The parser's complaint, or why an empty result is dangerous here.
        detail: String,
    },
    /// A task declares no objective.
    ///
    /// Not a §3.7 rule — see this module's docs. Inherited from the task spec
    /// S12 deleted: the objective is the only field that tells an agent what to
    /// do, and S10 measured that `codex exec` given no prompt blocks forever on
    /// stdin, so the launch refuses it — after a workspace has been cloned.
    BlankObjective {
        /// The task.
        task: String,
    },
    /// A task's `attempt_budget` is not at least 1.
    ///
    /// Not a §3.7 rule — see this module's docs. §4.6 bounds repair by this
    /// number, and a task that is allowed zero attempts can never launch.
    NonPositiveAttemptBudget {
        /// The task.
        task: String,
        /// What it declared.
        budget: i64,
    },
}

impl PlanDefect {
    /// A stable machine-readable kind, for `plan validate --json` and for tests
    /// that want to assert *which* rule fired rather than match on prose.
    pub fn kind(&self) -> &'static str {
        match self {
            PlanDefect::EmptyId { .. } => "empty_id",
            PlanDefect::DuplicateId { .. } => "duplicate_id",
            PlanDefect::DanglingDependency { .. } => "dangling_dependency",
            PlanDefect::ForwardDependency { .. } => "forward_dependency",
            PlanDefect::DependencyCycle { .. } => "dependency_cycle",
            PlanDefect::UnknownVerificationId { .. } => "unknown_verification_id",
            PlanDefect::UnboundCriterion { .. } => "unbound_criterion",
            PlanDefect::EmptyAction { .. } => "empty_action",
            PlanDefect::MalformedExecutionRequirements { .. } => "malformed_execution_requirements",
            PlanDefect::BlankObjective { .. } => "blank_objective",
            PlanDefect::NonPositiveAttemptBudget { .. } => "non_positive_attempt_budget",
        }
    }

    /// The id this defect is about — what an author greps their plan for.
    pub fn subject(&self) -> String {
        match self {
            PlanDefect::EmptyId { location, .. } => location.clone(),
            PlanDefect::DuplicateId { id, .. } => id.clone(),
            PlanDefect::DanglingDependency { task, .. } => task.clone(),
            PlanDefect::ForwardDependency { task, .. } => task.clone(),
            PlanDefect::DependencyCycle { cycle } => cycle.join(" -> "),
            PlanDefect::UnknownVerificationId { check, .. } => check.clone(),
            PlanDefect::UnboundCriterion { criterion, .. } => criterion.clone(),
            PlanDefect::EmptyAction { location, .. } => location.clone(),
            PlanDefect::MalformedExecutionRequirements { task, .. } => task.clone(),
            PlanDefect::BlankObjective { task } => task.clone(),
            PlanDefect::NonPositiveAttemptBudget { task, .. } => task.clone(),
        }
    }
}

impl fmt::Display for PlanDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanDefect::EmptyId { kind, location } => write!(
                f,
                "the {kind} at {location} has a blank id; §3.6's ids are assigned \
                 once and never reused, and a blank one addresses nothing — no \
                 dependency, finding, approval binding or commit trailer could \
                 refer to it"
            ),
            PlanDefect::DuplicateId { kind, id, location } => write!(
                f,
                "{kind} id {id:?} is used twice (again at {location}); §3.6 assigns \
                 an id once, and two things sharing one are indistinguishable in \
                 every dependency, finding and commit trailer that names it"
            ),
            PlanDefect::DanglingDependency { task, depends_on } => write!(
                f,
                "task {task:?} depends on {depends_on:?}, which the plan does not \
                 declare; §5.2 leaves `PENDING` only on \"deps met\", so {task} \
                 would wait forever for something that can never complete"
            ),
            PlanDefect::ForwardDependency { task, depends_on } => write!(
                f,
                "task {task:?} depends on {depends_on:?}, which the plan declares \
                 *later*; §3.7 refuses a forward dependency, so that a plan reads \
                 in the order it executes and cannot contain a cycle by \
                 construction — move {depends_on} above {task}"
            ),
            PlanDefect::DependencyCycle { cycle } => write!(
                f,
                "dependency cycle: {}; no task in it can ever become `READY`, \
                 because each waits for the next",
                cycle.join(" -> ")
            ),
            PlanDefect::UnknownVerificationId {
                task,
                criterion,
                check,
            } => write!(
                f,
                "task {task:?} criterion {criterion:?} is verified_by {check:?}, \
                 which `verification.yaml` does not define; the criterion looks \
                 bound and is bound to nothing, so §4.5's \"binds to ≥1 passing \
                 check\" would be satisfied by a check that never ran"
            ),
            PlanDefect::UnboundCriterion {
                task,
                criterion,
                statement,
            } => write!(
                f,
                "task {task:?} criterion {criterion:?} ({statement:?}) binds to no \
                 check and is not declared `manual: true`; §3.7 refuses this \
                 because an unbound criterion is the mechanism by which a task \
                 reaches `COMPLETE` on an agent's word — bind it to a check id \
                 from `verification.yaml`, or mark it `manual: true`, which \
                 forces a review boundary"
            ),
            PlanDefect::EmptyAction { task, location } => write!(
                f,
                "task {task:?} declares a blank action at {location}; §4.4's \
                 taxonomy strings are how `approval::gate::unattended_requirements` \
                 (§4.3) learns what a task may do, and a blank one addresses \
                 nothing"
            ),
            PlanDefect::MalformedExecutionRequirements { task, detail } => write!(
                f,
                "task {task:?}'s execution_requirements override cannot be read: \
                 {detail}; `enforce::launch::requirements_for` refuses this same \
                 shape for the durable column rather than reading it as \"no \
                 requirements\", because that is what a mis-nested override looks \
                 like — fix the block, or remove it"
            ),
            PlanDefect::BlankObjective { task } => write!(
                f,
                "task {task:?} declares no objective; it is the only field that \
                 tells an agent what to do, and an empty one is refused here \
                 rather than at launch — `codex exec` given no prompt blocks \
                 forever reading stdin (measured at S10), so the alternative is \
                 cloning a workspace and then discovering it"
            ),
            PlanDefect::NonPositiveAttemptBudget { task, budget } => write!(
                f,
                "task {task:?} declares attempt_budget {budget}; §4.6 bounds \
                 repair by this number and §5.1's column defaults to 3, so a \
                 task allowed fewer than one attempt can never launch — which is \
                 a stall no state in §5.2 explains"
            ),
        }
    }
}

/// Why a plan was refused — every defect found, in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    defects: Vec<PlanDefect>,
}

impl ValidationReport {
    /// Every defect found.
    ///
    /// Never empty: a report with no defects is not constructed, because
    /// [`validate`] returns `Ok` in that case.
    pub fn defects(&self) -> &[PlanDefect] {
        &self.defects
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "plan validate: refused, {} defect(s)",
            self.defects.len()
        )?;
        for defect in &self.defects {
            writeln!(f, "  - [{}] {defect}", defect.kind())?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationReport {}

/// A criterion that no machine can satisfy, and that says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualCriterion {
    /// The task it belongs to.
    pub task: String,
    /// The criterion's id.
    pub criterion: String,
    /// What a person has to judge.
    pub statement: String,
}

/// A plan that passed §3.7.
///
/// Separate type with private fields: everything downstream takes this rather
/// than a raw [`Plan`], so "was it validated?" is answered by the type rather
/// than by remembering to call a function. S5's validated task spec used the same
/// discipline and S12 deleted it with the rest of the spec; the discipline
/// outlived it, and [`super::project::Project`] applies it to `config_hash` too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPlan {
    plan: Plan,
    manual_criteria: Vec<ManualCriterion>,
}

impl ValidatedPlan {
    /// The plan.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Every task, in declaration order.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.plan.tasks()
    }

    /// The criteria declared `manual: true`, with the tasks they belong to.
    pub fn manual_criteria(&self) -> &[ManualCriterion] {
        &self.manual_criteria
    }

    /// Whether this plan cannot be completed without a person.
    ///
    /// §3.7's escape hatch "forces a review boundary". This is where the force
    /// comes from: the declaration survives validation as data, so the ledger
    /// that materialises the plan and the review bridge that schedules
    /// boundaries can both see it. A `manual: true` that validation dropped on
    /// the floor would be a comment.
    pub fn requires_human_review(&self) -> bool {
        !self.manual_criteria.is_empty()
    }
}

/// The check ids a verification profile defines — §3.7's catalogue.
///
/// Every place a profile can declare a check: `required`, `invariants`, and the
/// checks inside each `conditional` group. A conditional check counts: it may
/// not run on a given diff, but it is defined, and §3.7's rule is about ids that
/// resolve to nothing at all.
pub fn check_ids(profile: &Profile) -> BTreeSet<String> {
    let conditional = profile
        .conditional
        .iter()
        .flat_map(|group| group.checks.iter());
    profile
        .required
        .iter()
        .chain(profile.invariants.iter())
        .chain(conditional)
        .map(|check| check.id.clone())
        .collect()
}

/// One rule this module enforces. Most are §3.7's; two are not — see this
/// module's docs for why they belong here anyway. Appends the defects it
/// finds; never short-circuits.
type Rule = fn(&Plan, &BTreeSet<String>, &mut Vec<PlanDefect>);

/// Every rule [`validate`] runs, in the order their defects are reported.
///
/// A table rather than a sequence of calls, so that the rule set is
/// enumerable — and so that removing exactly one rule, to check that exactly
/// one test fails, is a one-line edit.
const RULES: &[Rule] = &[
    empty_ids,
    duplicate_ids,
    dangling_dependencies,
    forward_dependencies,
    dependency_cycles,
    unknown_verification_ids,
    unbound_criteria,
    empty_actions,
    malformed_execution_requirements,
    blank_objective,
    non_positive_attempt_budget,
];

/// Run §3.7 over a plan.
///
/// `defined_checks` is the id set from `verification.yaml` — see [`check_ids`].
/// It is a parameter rather than something this function loads, because the
/// profile lives in the repository and this module is pure.
pub fn validate(
    plan: &Plan,
    defined_checks: &BTreeSet<String>,
) -> Result<ValidatedPlan, ValidationReport> {
    let mut defects = Vec::new();
    for rule in RULES {
        rule(plan, defined_checks, &mut defects);
    }
    if !defects.is_empty() {
        return Err(ValidationReport { defects });
    }

    let manual_criteria = plan
        .tasks()
        .flat_map(|task| {
            task.acceptance_criteria
                .iter()
                .filter(|criterion| criterion.manual)
                .map(|criterion| ManualCriterion {
                    task: task.id.clone(),
                    criterion: criterion.id.clone(),
                    statement: criterion.statement.clone(),
                })
        })
        .collect();

    Ok(ValidatedPlan {
        plan: plan.clone(),
        manual_criteria,
    })
}

// ---------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------

/// An id that addresses nothing.
///
/// First, because it is the rule the others rest on: duplicate detection,
/// dependency resolution and every message below quote ids, and a blank id
/// makes all three meaningless at once.
fn empty_ids(plan: &Plan, _checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    let mut report = |kind: IdKind, id: &str, location: String| {
        if id.trim().is_empty() {
            defects.push(PlanDefect::EmptyId { kind, location });
        }
    };
    report(
        IdKind::Milestone,
        &plan.id,
        format!("plan {:?}", plan.version),
    );
    for (m, milestone) in plan.milestones.iter().enumerate() {
        report(IdKind::Milestone, &milestone.id, format!("milestones[{m}]"));
        for (s, slice) in milestone.slices.iter().enumerate() {
            let at = format!("{}/slices[{s}]", label(&milestone.id));
            report(IdKind::Slice, &slice.id, at);
            for (t, task) in slice.tasks.iter().enumerate() {
                let at = format!("{}/tasks[{t}]", label(&slice.id));
                report(IdKind::Task, &task.id, at);
                for (c, criterion) in task.acceptance_criteria.iter().enumerate() {
                    let at = format!("{}/acceptance_criteria[{c}]", label(&task.id));
                    report(IdKind::Criterion, &criterion.id, at);
                }
            }
        }
    }
}

/// What to call a container in a location string when its own id is blank.
fn label(id: &str) -> String {
    if id.trim().is_empty() {
        "<blank>".to_string()
    } else {
        id.to_string()
    }
}

/// §3.6: an id is assigned once and never reused.
///
/// Four namespaces, checked separately. Criterion ids are scoped to their
/// task — §6.5's example starts every task's criteria at `AC-1`, so a
/// plan-wide criterion namespace would refuse the format's own example.
fn duplicate_ids(plan: &Plan, _checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    let mut milestones = BTreeSet::new();
    let mut slices = BTreeSet::new();
    let mut tasks = BTreeSet::new();

    for milestone in &plan.milestones {
        if !milestones.insert(milestone.id.as_str()) {
            defects.push(PlanDefect::DuplicateId {
                kind: IdKind::Milestone,
                id: milestone.id.clone(),
                location: "milestones".to_string(),
            });
        }
        for slice in &milestone.slices {
            if !slices.insert(slice.id.as_str()) {
                defects.push(PlanDefect::DuplicateId {
                    kind: IdKind::Slice,
                    id: slice.id.clone(),
                    location: format!("{}/slices", label(&milestone.id)),
                });
            }
            for task in &slice.tasks {
                if !tasks.insert(task.id.as_str()) {
                    defects.push(PlanDefect::DuplicateId {
                        kind: IdKind::Task,
                        id: task.id.clone(),
                        location: format!("{}/tasks", label(&slice.id)),
                    });
                }
                let mut criteria = BTreeSet::new();
                for criterion in &task.acceptance_criteria {
                    if !criteria.insert(criterion.id.as_str()) {
                        defects.push(PlanDefect::DuplicateId {
                            kind: IdKind::Criterion,
                            id: criterion.id.clone(),
                            location: format!("{}/acceptance_criteria", label(&task.id)),
                        });
                    }
                }
            }
        }
    }
}

/// A `depends_on` entry that names nothing.
fn dangling_dependencies(plan: &Plan, _checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    let known: BTreeSet<&str> = plan.tasks().map(|task| task.id.as_str()).collect();
    for task in plan.tasks() {
        for dependency in &task.depends_on {
            if !known.contains(dependency.as_str()) {
                defects.push(PlanDefect::DanglingDependency {
                    task: task.id.clone(),
                    depends_on: dependency.clone(),
                });
            }
        }
    }
}

/// Tasks that wait for each other.
///
/// Depth-first search with three colours. Dependencies that resolve to nothing
/// are skipped rather than treated as leaves — [`dangling_dependencies`] has
/// already reported those, and reporting them twice under two names would make
/// the report worse rather than more thorough.
///
/// A self-dependency is a cycle of length one and is found by the same walk:
/// there is no "skip the node you started from" shortcut here, because that
/// shortcut is exactly what makes `T-0002 depends_on [T-0002]` invisible.
/// §3.7: a `depends_on` that names a task declared later in the document.
///
/// Kept as its own rule rather than folded into [`dependency_cycles`] because
/// it is strictly stronger and the two answer different questions. A plan can
/// point forward and still be acyclic — `forward_dependency.yaml` is exactly
/// that, and a cycle checker accepts it — so collapsing them would quietly
/// substitute the easier rule for the one §3.7 names.
///
/// Only tasks the plan actually declares are considered; a dependency on an id
/// that does not exist at all is [`dangling_dependencies`]' finding, and
/// reporting both for one edge would make an author fix it twice.
fn forward_dependencies(plan: &Plan, _checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    let position: HashMap<&str, usize> = plan
        .tasks()
        .enumerate()
        .map(|(index, task)| (task.id.as_str(), index))
        .collect();

    for (index, task) in plan.tasks().enumerate() {
        for depends_on in &task.depends_on {
            // A self-dependency is a cycle, not a forward reference: it points
            // at the same position, not a later one. `dependency_cycles` owns it.
            if let Some(&target) = position.get(depends_on.as_str())
                && target > index
            {
                defects.push(PlanDefect::ForwardDependency {
                    task: task.id.clone(),
                    depends_on: depends_on.clone(),
                });
            }
        }
    }
}

fn dependency_cycles(plan: &Plan, _checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        White,
        Grey,
        Black,
    }

    let order: Vec<&str> = plan.tasks().map(|task| task.id.as_str()).collect();
    let dependencies: HashMap<&str, Vec<&str>> = plan
        .tasks()
        .map(|task| {
            (
                task.id.as_str(),
                task.depends_on.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let mut colour: HashMap<&str, Colour> = order.iter().map(|id| (*id, Colour::White)).collect();

    // Explicit stack rather than recursion: a plan is data from the repository,
    // and a deep dependency chain must refuse the plan, never overflow the
    // stack that is refusing it.
    for start in &order {
        if colour.get(start).copied() != Some(Colour::White) {
            continue;
        }
        let mut path: Vec<&str> = Vec::new();
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        colour.insert(start, Colour::Grey);
        path.push(start);

        while let Some((node, index)) = stack.pop() {
            let edges = dependencies.get(node).cloned().unwrap_or_default();
            if index < edges.len() {
                stack.push((node, index + 1));
                let next = edges[index];
                match colour.get(next).copied() {
                    // Not a task: `dangling_dependencies` owns this one.
                    None => {}
                    Some(Colour::Grey) => {
                        // `next` is on the current path, so the path from its
                        // first occurrence to here, closed by `next`, is the
                        // cycle.
                        let from = path.iter().position(|id| *id == next).unwrap_or(0);
                        let mut cycle: Vec<String> =
                            path[from..].iter().map(|id| id.to_string()).collect();
                        cycle.push(next.to_string());
                        defects.push(PlanDefect::DependencyCycle { cycle });
                    }
                    Some(Colour::Black) => {}
                    Some(Colour::White) => {
                        colour.insert(next, Colour::Grey);
                        path.push(next);
                        stack.push((next, 0));
                    }
                }
            } else {
                colour.insert(node, Colour::Black);
                path.pop();
            }
        }
    }
}

/// A `verified_by` entry naming a check `verification.yaml` does not define.
fn unknown_verification_ids(plan: &Plan, checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    for task in plan.tasks() {
        for criterion in &task.acceptance_criteria {
            for check in &criterion.verified_by {
                if !checks.contains(check) {
                    defects.push(PlanDefect::UnknownVerificationId {
                        task: task.id.clone(),
                        criterion: criterion.id.clone(),
                        check: check.clone(),
                    });
                }
            }
        }
    }
}

/// §3.7's most important refusal: an automatically completable criterion with
/// no mechanical verification binding.
///
/// The predicate is deliberately narrow. It does not ask whether the statement
/// reads like prose, or how long it is, or whether it contains the word
/// "correctly" — those are heuristics, and a heuristic that refuses a plan is a
/// heuristic that gets worked around. It asks the one structural question that
/// decides whether a machine could ever say "done": is there a check, and if
/// not, does the plan admit that a person is required?
fn unbound_criteria(plan: &Plan, _checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    for task in plan.tasks() {
        for criterion in &task.acceptance_criteria {
            if is_automatically_completable(criterion) && !criterion.is_mechanically_bound() {
                defects.push(PlanDefect::UnboundCriterion {
                    task: task.id.clone(),
                    criterion: criterion.id.clone(),
                    statement: criterion.statement.clone(),
                });
            }
        }
    }
}

/// Whether anything other than a person is allowed to satisfy this criterion.
///
/// `manual: true` is the only thing that makes the answer `false`. That is what
/// §3.7 means by an escape hatch that "forces a review boundary": it does not
/// exempt the criterion from proof, it moves the proof to a human and records
/// that it did.
fn is_automatically_completable(criterion: &AcceptanceCriterion) -> bool {
    !criterion.manual
}

/// An `actions` entry that addresses nothing.
///
/// Not §3.7's list — see this module's docs — but the same defect as a blank
/// task id, on a different namespace. **Deliberately does not check taxonomy
/// membership.** An action outside §4.4's twenty-two becomes
/// `Action::Unknown` and is denied at evaluation by `Action::floor()`; adding
/// a second refusal for the same thing here would be a gate that can drift
/// from the one that actually runs.
/// A task with nothing to tell the agent.
///
/// Not §3.7's list — see this module's docs — and inherited from the task spec
/// S12 deleted. Refused at validate rather than at launch because the launch is
/// on the far side of a workspace clone.
fn blank_objective(plan: &Plan, _checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    for task in plan.tasks() {
        if task.objective.trim().is_empty() {
            defects.push(PlanDefect::BlankObjective {
                task: task.id.clone(),
            });
        }
    }
}

/// A task allowed fewer than one attempt.
///
/// Not §3.7's list — see this module's docs. §5.1's column defaults to 3, so
/// this only fires on a value somebody wrote deliberately, which is exactly the
/// case worth naming: a `0` here reads as "do not retry" and means "never run".
fn non_positive_attempt_budget(
    plan: &Plan,
    _checks: &BTreeSet<String>,
    defects: &mut Vec<PlanDefect>,
) {
    for task in plan.tasks() {
        if task.attempt_budget < 1 {
            defects.push(PlanDefect::NonPositiveAttemptBudget {
                task: task.id.clone(),
                budget: task.attempt_budget,
            });
        }
    }
}

fn empty_actions(plan: &Plan, _checks: &BTreeSet<String>, defects: &mut Vec<PlanDefect>) {
    for task in plan.tasks() {
        for (index, action) in task.actions.iter().enumerate() {
            if action.trim().is_empty() {
                defects.push(PlanDefect::EmptyAction {
                    task: task.id.clone(),
                    location: format!("{}/actions[{index}]", label(&task.id)),
                });
            }
        }
    }
}

/// A per-task `execution_requirements` override that cannot be read as a
/// requirement.
///
/// Two shapes are refused, both because `enforce::launch::requirements_for`
/// already refuses their equivalent for the durable column:
///
/// * The text does not parse as §4.2's dialect at all —
///   [`ExecutionRequirements::parse_yaml`] returns `Err`.
/// * The text parses, but names no `execution_requirements:` mapping, so the
///   result is empty. `enforce::launch`'s doc comment gives the reason this
///   is a refusal rather than "no requirements": it is indistinguishable
///   from a mis-nested override, and a mis-nested override must not silently
///   read as "nothing is gated".
///
/// A block that is present but blank (only whitespace) is treated as no
/// override at all, matching `requirements_for`'s own `yaml.trim().is_empty()`
/// short-circuit — an author who wrote nothing did not mean to gate anything.
fn malformed_execution_requirements(
    plan: &Plan,
    _checks: &BTreeSet<String>,
    defects: &mut Vec<PlanDefect>,
) {
    for task in plan.tasks() {
        let Some(text) = &task.execution_requirements else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        match ExecutionRequirements::parse_yaml(text) {
            Ok(parsed) if !parsed.is_empty() => {}
            Ok(_) => defects.push(PlanDefect::MalformedExecutionRequirements {
                task: task.id.clone(),
                detail: format!(
                    "holds {} bytes of YAML but no `execution_requirements` \
                     mapping was found in it, so nothing would be gated",
                    text.trim().len()
                ),
            }),
            Err(error) => defects.push(PlanDefect::MalformedExecutionRequirements {
                task: task.id.clone(),
                detail: error.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::model;

    fn checks() -> BTreeSet<String> {
        ["typecheck", "unit-tests"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// A one-task plan with one bound criterion. The unit tests below mutate
    /// this rather than re-declaring a plan each time, so that each test's
    /// difference from a valid plan is the thing it is testing.
    fn plan_with(criteria: &str, depends_on: &str) -> Plan {
        let yaml = format!(
            "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
             slices:\n        - id: S-01\n          tasks:\n            - id: T-0001\n              \
             objective: \"Do the thing.\"\n              \
             depends_on: {depends_on}\n              acceptance_criteria:\n{criteria}"
        );
        model::parse(&yaml).expect("the unit-test plan parses")
    }

    const BOUND: &str = "                - id: AC-1\n                  verified_by: [typecheck]\n";

    #[test]
    fn a_plan_whose_criteria_all_bind_is_accepted() {
        let plan = plan_with(BOUND, "[]");
        assert!(validate(&plan, &checks()).is_ok());
    }

    #[test]
    fn an_unbound_criterion_is_refused_and_a_manual_one_is_not() {
        let unbound = plan_with("                - id: AC-1\n", "[]");
        let manual = plan_with(
            "                - id: AC-1\n                  manual: true\n",
            "[]",
        );

        let report = validate(&unbound, &checks()).expect_err("refused");
        assert_eq!(report.defects().len(), 1);
        assert_eq!(report.defects()[0].kind(), "unbound_criterion");

        let validated = validate(&manual, &checks()).expect("accepted");
        assert!(validated.requires_human_review());
    }

    #[test]
    fn an_empty_check_catalogue_refuses_every_binding_rather_than_ignoring_them() {
        // The catalogue is a parameter, so an empty one is reachable — e.g. a
        // project with no `verification.yaml`. It must not read as "everything
        // is defined".
        let plan = plan_with(BOUND, "[]");
        let report = validate(&plan, &BTreeSet::new()).expect_err("refused");
        assert_eq!(report.defects()[0].kind(), "unknown_verification_id");
    }

    #[test]
    fn a_dependency_chain_that_is_long_is_not_a_cycle() {
        // Guards the cycle detector against the lazy implementation that calls
        // any revisited node a cycle: T-0003 is reached from two paths here.
        //
        // Declared dependencies-first, because §3.7 also refuses **forward**
        // dependencies. That rule constrains declaration *order*; it does not
        // reduce what a plan can express — this is the same diamond, written in
        // the order it executes. A fixture that pointed forward would now be
        // refused for a second, unrelated reason and would stop being a test of
        // the cycle detector.
        let yaml = "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
                    slices:\n        - id: S-01\n          tasks:\n            \
                    - id: T-0003\n              objective: \"Third.\"\n              \
                    depends_on: []\n            \
                    - id: T-0002\n              objective: \"Second.\"\n              \
                    depends_on: [T-0003]\n            \
                    - id: T-0001\n              objective: \"First.\"\n              \
                    depends_on: [T-0002, T-0003]\n";
        let plan = model::parse(yaml).expect("parses");
        assert!(
            validate(&plan, &BTreeSet::new()).is_ok(),
            "a diamond is a DAG"
        );
    }

    #[test]
    fn the_forward_dependency_rule_constrains_order_without_losing_expressiveness() {
        // The claim that makes §3.7's forward rule acceptable rather than
        // merely strict: every DAG has a declaration order that satisfies it —
        // a topological one. So the rule costs nothing but the author's
        // ordering, and buys structural acyclicity.
        //
        // The same diamond in the two orders: refused one way, accepted the
        // other. If this ever fails, the rule has started refusing graphs
        // rather than orderings, and that is a different and much worse rule.
        let forward = "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
                       slices:\n        - id: S-01\n          tasks:\n            \
                       - id: T-0001\n              objective: \"First.\"\n              \
                       depends_on: [T-0002]\n            \
                       - id: T-0002\n              objective: \"Second.\"\n              \
                       depends_on: []\n";
        let backward = "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
                        slices:\n        - id: S-01\n          tasks:\n            \
                        - id: T-0002\n              objective: \"Second.\"\n              \
                        depends_on: []\n            \
                        - id: T-0001\n              objective: \"First.\"\n              \
                        depends_on: [T-0002]\n";

        let refused = validate(&model::parse(forward).expect("parses"), &BTreeSet::new())
            .expect_err("a forward dependency is refused");
        assert_eq!(refused.defects()[0].kind(), "forward_dependency");
        assert!(
            validate(&model::parse(backward).expect("parses"), &BTreeSet::new()).is_ok(),
            "the same graph, declared in execution order, must be accepted"
        );
    }

    // -----------------------------------------------------------------------
    // S11 task 1 — `actions` and `execution_requirements`
    // -----------------------------------------------------------------------

    /// [`plan_with`]'s sibling for the two fields it does not parameterise:
    /// `extra` is spliced in after the task id and before
    /// `acceptance_criteria:`, at the task's own indentation, so a caller
    /// supplies exactly the `actions:`/`execution_requirements:` lines under
    /// test and nothing else about the task changes.
    fn plan_with_task_extra(extra: &str) -> Plan {
        let yaml = format!(
            "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
             slices:\n        - id: S-01\n          tasks:\n            - id: T-0001\n              \
             objective: \"Do the thing.\"\n\
             {extra}              acceptance_criteria:\n{BOUND}"
        );
        model::parse(&yaml).expect("the unit-test plan parses")
    }

    /// The same one-task plan with an objective the caller chooses, so that
    /// "blank" can be tested without a duplicate YAML key.
    fn plan_with_objective(objective: &str) -> Plan {
        let yaml = format!(
            "plan:\n  id: p-x\n  version: 1\n  milestones:\n    - id: M-01\n      \
             slices:\n        - id: S-01\n          tasks:\n            - id: T-0001\n              \
             objective: {objective}\n              acceptance_criteria:\n{BOUND}"
        );
        model::parse(&yaml).expect("the unit-test plan parses")
    }

    // -----------------------------------------------------------------------
    // S12 — the two refusals inherited from the deleted task spec (ADR-0017)
    // -----------------------------------------------------------------------

    #[test]
    fn a_task_with_no_objective_is_refused_and_names_itself() {
        // The failure this prevents is not a wrong answer, it is a wasted one:
        // without it the plan is approved, the task is materialized, a workspace
        // is cloned, and only then does `CodexAgent::command` refuse the empty
        // prompt — because S10 measured that `codex exec` with no prompt argument
        // blocks forever reading stdin.
        let plan = plan_with_objective("\"   \"");
        let report = validate(&plan, &checks()).expect_err("refused");
        assert_eq!(report.defects()[0].kind(), "blank_objective");
        assert!(
            report.to_string().contains("T-0001"),
            "the refusal must name the task: {report}"
        );
    }

    #[test]
    fn a_task_that_declares_an_objective_validates() {
        // POSITIVE CONTROL. Without it, "a blank objective is refused" is
        // satisfied by a rule that refuses every objective.
        let plan = plan_with_objective("\"Do the thing.\"");
        assert!(validate(&plan, &checks()).is_ok());
    }

    #[test]
    fn a_task_allowed_fewer_than_one_attempt_is_refused() {
        // §5.1's column defaults to 3, so this can only be a value somebody
        // wrote. `0` reads as "do not retry" and means "never run".
        let plan = plan_with_task_extra("              attempt_budget: 0\n");
        let report = validate(&plan, &checks()).expect_err("refused");
        assert_eq!(report.defects()[0].kind(), "non_positive_attempt_budget");
    }

    #[test]
    fn the_default_attempt_budget_validates() {
        // POSITIVE CONTROL, and it asserts the default is the one §5.1 names: a
        // rule reading an absent field as `0` would refuse every plan that does
        // not spell the budget out.
        let plan = plan_with_task_extra("");
        assert_eq!(
            plan.tasks().next().expect("a task").attempt_budget,
            3,
            "§5.1's column defaults to 3"
        );
        assert!(validate(&plan, &checks()).is_ok());
    }

    #[test]
    fn a_task_that_declares_no_scope_is_not_refused_here() {
        // The third of the deleted spec's refusals, deliberately **not**
        // restored. `conductor_git::Scope::contains` fails closed on an empty
        // scope — "a task that forgot to declare one halts for review rather
        // than authorising everything" — and `project.yaml`'s `scope_defaults`
        // are inherited before it gets there. A rule here would be a second gate
        // on a question already answered safely. Asserted rather than left
        // implicit, so that adding one later is a deliberate reversal.
        let plan = plan_with_task_extra("");
        assert!(
            plan.tasks()
                .next()
                .expect("a task")
                .scope
                .allowed_globs
                .is_empty(),
            "the fixture must declare no scope for this to mean anything"
        );
        assert!(validate(&plan, &checks()).is_ok());
    }

    #[test]
    fn a_plan_declaring_a_taxonomy_action_validates() {
        let plan = plan_with_task_extra("              actions: [git.push]\n");
        assert!(validate(&plan, &checks()).is_ok());
    }

    #[test]
    fn an_action_outside_the_taxonomy_still_validates() {
        // §3.7 is a closed list and does not name "unknown action" as a
        // refusal, and §4.4 already floors an unknown action at `deny` when
        // it is evaluated (`Action::parse` is infallible and yields
        // `Action::Unknown`, which `Action::floor()` denies). Refusing it a
        // second time here, at validate, would be a second gate that can
        // drift from the first — e.g. if the taxonomy grows and this rule is
        // not updated in lockstep, it would start refusing plans §4.4 would
        // happily deny at evaluation instead. So an unrecognised action name
        // is data the plan is allowed to carry; denying its use is
        // evaluation's job, not validate's.
        let plan = plan_with_task_extra("              actions: [something.not.in.the.taxonomy]\n");
        assert!(validate(&plan, &checks()).is_ok());
    }

    #[test]
    fn a_blank_action_string_is_refused_and_names_the_task() {
        let plan = plan_with_task_extra("              actions: [\"   \"]\n");
        let report = validate(&plan, &checks()).expect_err("refused");
        assert_eq!(report.defects()[0].kind(), "empty_action");
        assert!(
            report.to_string().contains("T-0001"),
            "the refusal must name the task the blank action belongs to: {report}"
        );
    }

    #[test]
    fn a_well_formed_execution_requirements_override_validates() {
        let plan = plan_with_task_extra(
            "              execution_requirements: |\n                execution_requirements:\n                  \
             control_surface: hard\n",
        );
        assert!(validate(&plan, &checks()).is_ok());
    }

    #[test]
    fn an_execution_requirements_override_that_parses_to_nothing_is_refused() {
        // Mirrors `enforce::launch::requirements_for`'s rule for the durable
        // column: a block that holds YAML but contains no
        // `execution_requirements:` mapping is indistinguishable from a
        // mis-nested override, and a mis-nested override must not silently
        // read as "no requirement" — see [`Task::execution_requirements`]'s
        // doc comment.
        let plan = plan_with_task_extra(
            "              execution_requirements: |\n                project:\n                  \
             adapter: codex\n",
        );
        let report = validate(&plan, &checks()).expect_err("refused");
        assert_eq!(
            report.defects()[0].kind(),
            "malformed_execution_requirements"
        );
        assert!(
            report.to_string().contains("T-0001"),
            "the refusal must name the task whose override is broken: {report}"
        );
    }

    #[test]
    fn an_execution_requirements_override_that_does_not_parse_at_all_is_refused() {
        let plan = plan_with_task_extra(
            "              execution_requirements: |\n                execution_requirements:\n                  \
             not_a_real_dimension: hard\n",
        );
        let report = validate(&plan, &checks()).expect_err("refused");
        assert_eq!(
            report.defects()[0].kind(),
            "malformed_execution_requirements"
        );
        assert!(
            report.to_string().contains("T-0001"),
            "the refusal must name the task whose override is broken: {report}"
        );
    }

    #[test]
    fn the_rule_table_holds_every_defect_kind_the_module_can_produce() {
        // A rule that is written but never listed in `RULES` is a rule that
        // does not run, and nothing else would notice.
        assert_eq!(RULES.len(), 11);
    }
}
