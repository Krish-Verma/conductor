//! The implementation packet — master plan §6.5.
//!
//! What an agent is given before it starts: the objective, what "done" means and
//! what proves it, what it may touch, what it may never touch, the decisions
//! whose argument it needs, where the repository is, and which boundaries stop
//! it. Nothing else.
//!
//! # Built from the store *and* the repository
//!
//! Both halves, for the reason [`super`] gives. Concretely:
//!
//! | From the store | From the repository |
//! |---|---|
//! | which plan version this run is pinned to (row 21) | the objective and rationale |
//! | the run's `policy_hash` and the policy it names | the acceptance criteria and their bindings |
//! | `base_commit`, `run_branch`, workspace | the scope globs |
//! | the plan version's `content_hash` | the decision bodies |
//! | which decisions the project has | the verification commands |
//!
//! The plan document is read from **the version the run is on**, never the
//! newest — acceptance row 21's *"a run in flight keeps its old plan version"*
//! is a statement about what the agent is told, not only about which rows exist.
//!
//! # Context minimization, and the half of §6.5 that is not implementable
//!
//! §6.5 selects decisions *"by touching the task's scope globs or explicit refs
//! — never 'all accepted decisions'"*. Only explicit refs exists: §3.6 fixes a
//! decision's frontmatter at four fields with `deny_unknown_fields`, so a
//! decision cannot declare a scope to match against. The reference therefore
//! points from the plan, which a human writes and approves. See
//! [`crate::plan::model::Task::decisions`] and ADR-0016.
//!
//! A reference that does not resolve is [`PacketError::UnknownDecision`] —
//! refused, not dropped. Silently omitting an argument the plan says the task
//! needs is context minimization failing in the direction that loses
//! information, which is the direction that matters.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use conductor_core::RunId;
use conductor_store::Store;
use serde_yaml::{Mapping, Value};

use super::{Emitted, Evidence, PacketError};
use crate::decision;
use crate::plan::{self, model::Task};
use crate::policy::load as policy_load;
use crate::policy::model::Effect;
use crate::verify::profile;

/// §6.5's `packet_version`.
///
/// Bumped when the *shape* changes, not when a value does. It is inside the
/// hashed content deliberately: a reader that cannot tell which shape it is
/// looking at cannot safely parse either.
pub const PACKET_VERSION: u32 = 1;

/// Which report shape the packet's reader is held to — §6.5's `report_schema`.
///
/// # Why this is an identifier and not a path (corrected at S12)
///
/// §6.5's example writes `report_schema: schemas/agent-report.v1.json`, which
/// reads like a path, and this constant used to be exactly that string. It could
/// not resolve for anybody. A packet is generated for **the user's** project, so a
/// repository-relative path resolves against *their* tree, where no such file
/// exists — while the schema an agent is actually held to is the one the launch
/// path writes into the run's artifact tree and passes to `codex exec
/// --output-schema`. So the field named a file the reader could not open, which is
/// the same failure ADR-0016 refuses for an unresolvable decision ref, in a field
/// nobody would have thought to check.
///
/// It is therefore a **version identifier**: the `$id` of
/// `schemas/agent-report.v1.json`, which is Conductor's own repository-tracked
/// artifact and the single source for the shape (`REPORT_SCHEMA_JSON` includes
/// it). Handing the agent the file is the adapter mechanism's job, and a path
/// belongs there.
pub const REPORT_SCHEMA_ID: &str = "agent-report.v1";

/// One decision, as it travels.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CarriedDecision {
    id: String,
    status: String,
    statement: String,
}

/// §6.5's implementation packet.
#[derive(Debug, Clone)]
pub struct ImplementationPacket {
    run_id: String,
    task_id: String,
    plan_version: u32,
    plan_hash: String,
    policy_hash: String,
    objective: String,
    milestone: String,
    slice: String,
    rationale: String,
    acceptance: Vec<(String, String, Vec<String>, bool)>,
    allowed_globs: Vec<String>,
    forbidden_globs: Vec<String>,
    decisions: Vec<CarriedDecision>,
    base_commit: String,
    branch: String,
    workspace: Option<String>,
    verification: Vec<(String, String)>,
    requires_approval: Vec<String>,
    forbidden_actions: Vec<String>,
    evidence: Vec<Evidence>,
}

impl ImplementationPacket {
    /// Attach one piece of evidence — §6.5's `evidence_links`.
    pub fn with_evidence(mut self, evidence: Evidence) -> Self {
        self.evidence.push(evidence);
        self
    }

    /// The packet as a document, for [`super::continuation`] to extend.
    ///
    /// Crate-visible rather than public: a continuation packet **is** this
    /// document plus observed reality (§6.5), and rebuilding it from scratch
    /// there would be a second place the implementation packet's shape is
    /// written down.
    pub(super) fn to_value_for_continuation(&self) -> Value {
        self.to_value()
    }

    /// The packet as a `serde_yaml` document.
    fn to_value(&self) -> Value {
        let mut m = Mapping::new();
        let mut put = |k: &str, v: Value| {
            m.insert(Value::from(k), v);
        };
        put("packet", Value::from("implementation"));
        put("packet_version", Value::from(PACKET_VERSION));
        put("run_id", Value::from(self.run_id.clone()));
        put("task_id", Value::from(self.task_id.clone()));
        put("plan_version", Value::from(self.plan_version));
        put("plan_hash", Value::from(self.plan_hash.clone()));
        put("policy_hash", Value::from(self.policy_hash.clone()));
        put("objective", Value::from(self.objective.clone()));

        let mut context = Mapping::new();
        context.insert(
            Value::from("milestone"),
            Value::from(self.milestone.clone()),
        );
        context.insert(Value::from("slice"), Value::from(self.slice.clone()));
        context.insert(Value::from("why_now"), Value::from(self.rationale.clone()));
        put("context", Value::Mapping(context));

        put(
            "acceptance_criteria",
            Value::Sequence(
                self.acceptance
                    .iter()
                    .map(|(id, statement, verified_by, manual)| {
                        let mut c = Mapping::new();
                        c.insert(Value::from("id"), Value::from(id.clone()));
                        c.insert(Value::from("statement"), Value::from(statement.clone()));
                        c.insert(
                            Value::from("verified_by"),
                            Value::Sequence(verified_by.iter().cloned().map(Value::from).collect()),
                        );
                        if *manual {
                            c.insert(Value::from("manual"), Value::from(true));
                        }
                        Value::Mapping(c)
                    })
                    .collect(),
            ),
        );

        let mut scope = Mapping::new();
        scope.insert(
            Value::from("allowed_globs"),
            Value::Sequence(
                self.allowed_globs
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        scope.insert(
            Value::from("forbidden_globs"),
            Value::Sequence(
                self.forbidden_globs
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        put("scope", Value::Mapping(scope));

        if !self.decisions.is_empty() {
            let mut by_status: BTreeMap<String, Vec<Value>> = BTreeMap::new();
            for d in &self.decisions {
                let mut e = Mapping::new();
                e.insert(Value::from("id"), Value::from(d.id.clone()));
                e.insert(Value::from("statement"), Value::from(d.statement.clone()));
                by_status
                    .entry(d.status.to_ascii_lowercase())
                    .or_default()
                    .push(Value::Mapping(e));
            }
            let mut decisions = Mapping::new();
            for (status, entries) in by_status {
                decisions.insert(Value::from(status), Value::Sequence(entries));
            }
            put("decisions", Value::Mapping(decisions));
        }

        let mut repository = Mapping::new();
        repository.insert(
            Value::from("base_commit"),
            Value::from(self.base_commit.clone()),
        );
        repository.insert(Value::from("branch"), Value::from(self.branch.clone()));
        if let Some(ws) = &self.workspace {
            repository.insert(Value::from("workspace"), Value::from(ws.clone()));
        }
        put("repository", Value::Mapping(repository));

        let mut verification = Mapping::new();
        verification.insert(
            Value::from("commands"),
            Value::Sequence(
                self.verification
                    .iter()
                    .map(|(id, command)| {
                        let mut c = Mapping::new();
                        c.insert(Value::from("id"), Value::from(id.clone()));
                        c.insert(Value::from("command"), Value::from(command.clone()));
                        Value::Mapping(c)
                    })
                    .collect(),
            ),
        );
        put("verification", Value::Mapping(verification));

        let mut boundaries = Mapping::new();
        boundaries.insert(
            Value::from("requires_approval"),
            Value::Sequence(
                self.requires_approval
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        boundaries.insert(
            Value::from("forbidden"),
            Value::Sequence(
                self.forbidden_actions
                    .iter()
                    .cloned()
                    .map(Value::from)
                    .collect(),
            ),
        );
        put("boundaries", Value::Mapping(boundaries));

        if !self.evidence.is_empty() {
            put(
                "evidence_links",
                Value::Sequence(self.evidence.iter().map(Evidence::to_value).collect()),
            );
        }
        put("report_schema", Value::from(REPORT_SCHEMA_ID));

        Value::Mapping(m)
    }

    /// The canonical bytes, whatever their size.
    ///
    /// Infallible: a canonical form always exists. *Emitting* one is what the
    /// budget bounds — see [`Self::try_canonical_bytes`].
    pub fn canonical_bytes(&self) -> Vec<u8> {
        super::canonical_bytes(&self.to_value())
    }

    /// The canonical bytes, refusing anything over [`super::MAX_PACKET_BYTES`].
    pub fn try_canonical_bytes(&self) -> Result<Vec<u8>, PacketError> {
        Ok(self.emit()?.bytes().to_vec())
    }

    /// Canonicalize, bound-check and hash.
    pub fn emit(&self) -> Result<Emitted, PacketError> {
        super::emit(&self.to_value())
    }

    /// `blake3:<hex>` over the canonical bytes.
    pub fn hash(&self) -> super::PacketHash {
        super::PacketHash::from_bytes(&self.canonical_bytes())
    }

    /// The packet as YAML, for a human and for the agent's prompt.
    ///
    /// Rendered from the same [`Value`] the hash covers, so what a reader sees
    /// and what the digest names cannot drift apart.
    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(&super::render(&self.to_value())).unwrap_or_default()
    }
}

/// Build the implementation packet for one run — §6.5.
pub fn build(store: &mut Store, run_id: &RunId) -> Result<ImplementationPacket, PacketError> {
    let run = store.run(run_id)?.ok_or_else(|| PacketError::Missing {
        what: "run",
        id: run_id.as_str().to_string(),
    })?;
    let task_id = conductor_core::TaskId::new(&run.task_id)
        .map_err(|e| PacketError::Document(e.to_string()))?;
    let task_row = store.task(&task_id)?.ok_or_else(|| PacketError::Missing {
        what: "task",
        id: run.task_id.clone(),
    })?;

    // The plan version the run is pinned to — row 21.
    let plan_version_id = conductor_core::PlanVersionId::new(&task_row.plan_version_id)
        .map_err(|e| PacketError::Document(e.to_string()))?;
    let plan_row = store
        .plan_version(&plan_version_id)?
        .ok_or_else(|| PacketError::Missing {
            what: "plan version",
            id: task_row.plan_version_id.clone(),
        })?;
    let project = store
        .project(&plan_row.project_id)?
        .ok_or_else(|| PacketError::Missing {
            what: "project",
            id: plan_row.project_id.as_str().to_string(),
        })?;
    let root = PathBuf::from(&project.root_path);
    let version = u32::try_from(plan_row.version).unwrap_or(0);

    // -- the repository half --------------------------------------------------
    let plan_text = read(&root.join(plan::plan_path(version)), "the plan document")?;
    let document = plan::parse(&plan_text).map_err(|e| PacketError::Document(e.to_string()))?;

    let mut found: Option<(&str, &str, &Task)> = None;
    for milestone in &document.milestones {
        for slice in &milestone.slices {
            for task in &slice.tasks {
                if task.id.trim() == task_row.id.as_str() {
                    found = Some((&milestone.id, &slice.id, task));
                }
            }
        }
    }
    let (milestone, slice, task) = found.ok_or_else(|| PacketError::TaskNotInPlan {
        version,
        task: task_row.id.as_str().to_string(),
    })?;

    let config = plan::project::load(&root).map_err(|e| PacketError::Document(e.to_string()))?;

    // §3.3: `.conductor/**` is always forbidden regardless of what any plan
    // says, so it is unioned in here rather than trusted from the document —
    // the plan is a file the agent can edit.
    let mut forbidden: BTreeSet<String> = task.scope.forbidden_globs.iter().cloned().collect();
    forbidden.extend(config.scope_defaults.forbidden_globs.iter().cloned());
    forbidden.insert(".conductor/**".to_string());

    let mut allowed: Vec<String> = if task.scope.allowed_globs.is_empty() {
        config.scope_defaults.allowed_globs.clone()
    } else {
        task.scope.allowed_globs.clone()
    };
    allowed.sort();
    allowed.dedup();

    // -- decisions: explicit refs only (§6.5) ---------------------------------
    let available = decision::load_all(&root).map_err(|e| PacketError::Document(e.to_string()))?;
    let mut carried = Vec::new();
    for wanted in &task.decisions {
        let wanted = wanted.trim();
        if wanted.is_empty() {
            continue;
        }
        let found = available.iter().find(|d| d.id == wanted).ok_or_else(|| {
            PacketError::UnknownDecision {
                task: task_row.id.as_str().to_string(),
                decision: wanted.to_string(),
            }
        })?;
        carried.push(CarriedDecision {
            id: found.id.clone(),
            status: found.status.as_str().to_string(),
            // The argument, not the whole document: §6.5 quotes a `statement`.
            statement: first_paragraph(&found.body),
        });
    }
    carried.sort_by(|a, b| a.id.cmp(&b.id));

    // -- verification ---------------------------------------------------------
    let profile_path = if task_row.verification_profile.trim().is_empty() {
        root.join(".conductor/verification.yaml")
    } else {
        root.join(&task_row.verification_profile)
    };
    let loaded = profile::load(&profile_path).map_err(|e| PacketError::Document(e.to_string()))?;
    // Every check the profile defines, in one flat list: §6.5's `verification`
    // is what the agent runs before claiming done, and the required/invariant
    // split is Conductor's scheduling concern, not the agent's. Conditionals are
    // included because a check that fires only on some diffs is still one the
    // agent can be asked to have satisfied.
    let profile_checks = loaded
        .profile
        .required
        .iter()
        .chain(loaded.profile.invariants.iter())
        .chain(
            loaded
                .profile
                .conditional
                .iter()
                .flat_map(|c| c.checks.iter()),
        );
    let mut verification: Vec<(String, String)> = profile_checks
        .map(|check| (check.id.clone(), check.command.argv().join(" ")))
        .collect();
    verification.sort();
    verification.dedup();

    // -- boundaries, from the policy the run is pinned to ----------------------
    let pinned = policy_load::pinned_for_run(store.conn(), run_id)
        .map_err(|e| PacketError::Document(e.to_string()))?;
    let mut requires_approval = BTreeSet::new();
    let mut forbidden_actions = BTreeSet::new();
    for document in pinned.policy.documents() {
        for rule in document.rules() {
            match rule.effect {
                Effect::RequireApproval => {
                    requires_approval.insert(rule.pattern.as_str().to_string());
                }
                Effect::Deny => {
                    forbidden_actions.insert(rule.pattern.as_str().to_string());
                }
                Effect::Allow => {}
            }
        }
    }

    let mut acceptance: Vec<(String, String, Vec<String>, bool)> = task
        .acceptance_criteria
        .iter()
        .map(|c| {
            let mut bound = c.verified_by.clone();
            bound.sort();
            (c.id.clone(), c.statement.clone(), bound, c.manual)
        })
        .collect();
    acceptance.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(ImplementationPacket {
        run_id: run_id.as_str().to_string(),
        task_id: task_row.id.as_str().to_string(),
        plan_version: version,
        plan_hash: plan_row.content_hash.clone(),
        policy_hash: pinned.hash.clone(),
        objective: task.objective.clone(),
        milestone: milestone.to_string(),
        slice: slice.to_string(),
        rationale: task.rationale.clone(),
        acceptance,
        allowed_globs: allowed,
        forbidden_globs: forbidden.into_iter().collect(),
        decisions: carried,
        base_commit: run.base_commit.clone(),
        branch: run.run_branch.clone(),
        workspace: run.workspace_path.clone(),
        verification,
        requires_approval: requires_approval.into_iter().collect(),
        forbidden_actions: forbidden_actions.into_iter().collect(),
        evidence: Vec::new(),
    })
}

fn read(path: &Path, what: &'static str) -> Result<String, PacketError> {
    std::fs::read_to_string(path).map_err(|source| PacketError::Io {
        what,
        path: path.to_path_buf(),
        source,
    })
}

/// The decision's argument, condensed to what §6.5 calls a `statement`.
///
/// The first paragraph, not the whole body: §6.5 budgets the packet, and a
/// decision record is prose a human wrote at whatever length the argument
/// needed. The full document is one path away — `source_path` is in the store —
/// so nothing is lost, only deferred.
fn first_paragraph(body: &str) -> String {
    body.trim()
        .split("\n\n")
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
