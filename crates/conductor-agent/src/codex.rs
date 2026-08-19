//! The Codex adapter — master plan §6.2, the first real agent (S10).
//!
//! Pure translation, like every other adapter (§6.1): this file builds argv,
//! parses lines and classifies exits. **It spawns nothing, opens nothing and
//! reads no clock.** `conductor-run` runs the command; `tests/codex.rs` proves
//! every behaviour here against recorded bytes with no process at all, which is
//! the only way to test the least stable component in the system.
//!
//! ## Why Codex first
//!
//! §6.2: on a host with no container runtime, `--sandbox workspace-write` is the
//! only measured mechanism that actually denies writes outside the workspace
//! (M6), denies network egress (M9) and denies the control socket (M10).
//! Building the first adapter against the agent that offers real containment
//! exercises the enforcement layer from the beginning instead of stubbing it.
//! **`danger-full-access` therefore never appears in an argv this file builds** —
//! it would throw away the entire reason this agent was chosen, and §4.9's
//! "prevented" column with it.
//!
//! ## Four things measured on codex-cli 0.142.0 (2026-08-15) that the shape of
//! ## this adapter depends on
//!
//! 1. **`--output-schema` shapes every `agent_message`, not just the last one.**
//!    The recorded run in `tests/fixtures/codex-jsonl/success.jsonl` emits five
//!    schema-shaped messages: four claim `PARTIAL`, and only the fifth claims
//!    `COMPLETE`. An adapter that takes the *first* schema-shaped message reports
//!    `PARTIAL` for a run that finished, and the task is repaired forever.
//!    [`CodexAgent::extract_report`] therefore takes the file written by
//!    `--output-last-message` and falls back to the **last** `agent_message`.
//! 2. **`files_touched` holds absolute paths.** Codex reports
//!    `/workspace/lib.rs`; §4.8 reconciles against `git status`, which says
//!    `lib.rs`. Without normalisation every successful run disagrees with the
//!    repository and reconciles as `CONTRADICTED` — a false "the agent lied" on
//!    every run. A path that is genuinely *outside* the workspace is left exactly
//!    as the agent wrote it: that one is evidence (§4.9's detection column), and
//!    rewriting it would destroy the only record of an escape.
//! 3. **The session id is `thread_id` on `thread.started`.** There is no
//!    `session_id` field anywhere in the stream. §6.2 already says the identity
//!    cannot be pre-assigned, which is why
//!    [`FunctionalCapabilities::conductor_assigned_session_id`] is `false` here
//!    and `true` for Claude Code (§6.3).
//! 4. **Codex blocks forever on stdin when given no prompt argument** — it prints
//!    "Reading additional input from stdin..." and waits, even when stdin is not
//!    a TTY. `supervise::spawn` gives the child `Stdio::null()`, so this is safe
//!    today. **Do not "helpfully" give the child a pipe or inherit stdin**: an
//!    agent blocked on a read Conductor never satisfies burns the whole
//!    wall-clock budget and produces `TIMED_OUT` with nothing in it. The prompt
//!    goes in argv, and [`CodexAgent::command`] refuses an empty one rather than
//!    building a command that hangs.
//!
//! ## Flags deliberately not used
//!
//! - **`--ephemeral`.** §6.2 lists it under hermeticity, but it discards the
//!   session, and `resume` — in S10's scope — needs the session to exist.
//!   `--ignore-user-config` and `--ignore-rules` give the hermeticity without
//!   the cost.
//! - **`--skip-git-repo-check`.** A run workspace is always a
//!   `git clone --no-hardlinks --no-checkout` (ADR-0001), so the check can only
//!   fire when something is already wrong. Keeping it is a free assertion.
//! - **`--add-dir`.** Widening the sandbox is the opposite of why Codex was
//!   chosen. A task that needs a second directory needs an ADR, not a flag.
//! - **`--model`.** Model selection is a policy question (§4.4), not an adapter
//!   default, and hardcoding one here would silently outlive the model.
//!
//! ## Two known limitations, recorded rather than hidden
//!
//! - **`-C/--cd` versus §6.2.** The master plan's §6.2 table says the working
//!   root is "cwd (`-C` requires `--permissions-profile`; use cwd instead)". The
//!   S10 measurement of `codex exec --help` on 0.142.0 lists `-C/--cd <DIR>`
//!   with no such condition. This adapter sets **both**: [`AgentCommand::cwd`] is
//!   the workspace (which is what §6.2 relies on) *and* `--cd` names it
//!   explicitly. They cannot disagree, because [`CodexAgent::command`] refuses a
//!   `StartInput` whose workspace is not the one the adapter was built for. If a
//!   real run shows `--cd` demanding `--permissions-profile`, deleting the two
//!   `--cd` arguments is the whole fix and `cwd` still carries the run.
//! - **One line can become several events, and that is why §6.1 changed.** A
//!   Codex `file_change` item carries an *array* of changes, and the original
//!   `parse_event -> Option<AgentEvent>` could report only the first — so every
//!   multi-file edit understated what the agent did. Nothing about correctness
//!   depended on it (§4.8 reconciles against git, which sees all of them), which
//!   is precisely why no test would ever have caught it. S10's own note settles
//!   the direction: *"a design smell to fix in the interface, not the adapter"*.
//!   The trait now returns `Vec<AgentEvent>`; this adapter emits one event per
//!   change, in order, with each change's kind preserved.
//!
//! Parsing is permissive throughout (§2.2): an unknown event type, an unknown
//! item type and an unknown field all yield no events, never an error. A Codex upgrade
//! that adds an event kind must not fail a run.

use std::path::{Path, PathBuf};

use conductor_core::{AgentReport, AttemptOutcome};
use serde_json::Value;

use crate::error::{AgentError, AgentResult};
use crate::event::AgentEvent;
use crate::{
    AgentAdapter, AgentCommand, FunctionalCapabilities, ResumeInput, RunOutputs, StartInput,
};

/// The JSON Schema Conductor hands to `codex exec --output-schema <FILE>`.
///
/// It is exactly [`AgentReport`]: a schema that drifts from the type Conductor
/// deserialises makes the agent runtime enforce the *wrong* contract, which is
/// worse than enforcing none. `tests/codex.rs` pins the two together, including
/// the enum, which it takes from `ReportClaim`'s own serde encoding rather than
/// from a second copy of the strings.
///
/// The adapter never writes this file — that is I/O, and §6.1 keeps I/O out of
/// adapters. `conductor-run` writes it into the attempt's artifact directory and
/// passes the path to [`CodexAgent::new`].
///
/// # One artifact, included rather than duplicated (S12)
///
/// The schema used to be a string literal here, and `schemas/agent-report.v1.json`
/// — the path §6.5's packet names — did not exist. Two copies of a contract are
/// two things that can disagree, and the one an agent is held to would have been
/// whichever a later editor happened to change. `include_str!` makes the
/// repository file the only copy: the crate does not build without it, and editing
/// it changes what every agent is asked to produce.
pub const REPORT_SCHEMA_JSON: &str = include_str!("../../../schemas/agent-report.v1.json");

/// The Codex adapter.
///
/// It holds the workspace root because [`AgentAdapter::extract_report`] is
/// handed [`RunOutputs`] and nothing else, and normalising an absolute path
/// needs a root to normalise against. That makes the root a second copy of
/// something [`StartInput`] also carries, so [`CodexAgent::command`] refuses an
/// input whose workspace disagrees rather than quietly measuring a report
/// against the wrong tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexAgent {
    binary: PathBuf,
    workspace: PathBuf,
    output_schema: PathBuf,
}

impl CodexAgent {
    /// An adapter running `binary` in `workspace`, with the report schema at
    /// `output_schema`.
    ///
    /// No path is checked here. §6.1's separation means an adapter has no
    /// business touching the filesystem, and `tests/codex.rs` relies on being
    /// able to build commands for a `codex` that does not exist.
    ///
    /// **The instruction is not adapter state (changed at S12).** It arrives on
    /// [`StartInput::instructions`], because §6.5's packet cannot be built until
    /// the workspace exists and §4.6 gives attempt 2 a *different* one. A
    /// `with_prompt` used to set it here, which meant an adapter constructed
    /// before the run could only ever carry something less than a packet — and
    /// every attempt of a run would carry the same one.
    pub fn new(binary: PathBuf, workspace: PathBuf, output_schema: PathBuf) -> Self {
        CodexAgent {
            binary,
            workspace,
            output_schema,
        }
    }

    /// The workspace this adapter normalises reported paths against.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The flags every invocation carries, start or resume.
    ///
    /// Sharing them is not tidiness: a resumed attempt that quietly dropped
    /// `--sandbox workspace-write` would be an uncontained run wearing a
    /// contained run's record.
    fn common_args(&self, report_path: &Path) -> Vec<String> {
        vec![
            // JSONL on stdout — §6.2's event stream.
            "--json".to_string(),
            // The only reason Codex is the first adapter (M6, M9, M10).
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            // Hermeticity: neither `~/.codex/config.toml` nor ambient rule files
            // may change what a planned run does.
            "--ignore-user-config".to_string(),
            "--ignore-rules".to_string(),
            // Redundant with `cwd`, and deliberately so — see the module note.
            "--cd".to_string(),
            self.workspace.to_string_lossy().to_string(),
            // The report contract, enforced by the agent runtime rather than
            // validated afterwards.
            "--output-schema".to_string(),
            self.output_schema.to_string_lossy().to_string(),
            "--output-last-message".to_string(),
            report_path.to_string_lossy().to_string(),
        ]
    }

    /// Refuse anything that would produce a command Conductor cannot trust.
    fn check(&self, input: &StartInput) -> AgentResult<()> {
        if input.workspace != self.workspace {
            return Err(AgentError::Unusable(format!(
                "this adapter normalises reported paths against the workspace {}, \
                 but the attempt runs in {}",
                self.workspace.display(),
                input.workspace.display()
            )));
        }
        if input.instructions.trim().is_empty() {
            return Err(AgentError::Unusable(
                "codex exec with no prompt argument blocks forever reading stdin, \
                 and the supervisor gives the child a null stdin that will never \
                 satisfy it"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Make a path Codex reported comparable with what §4.8 observes in git.
    ///
    /// Lexical only, because resolving a path is I/O. On macOS that means
    /// `/var/...` and `/private/var/...` are different strings for the same
    /// directory, so a workspace under a symlinked root would leave its paths
    /// absolute. That degrades to "the report disagrees with git", which
    /// reconciliation already handles as evidence — it never invents agreement
    /// that is not there.
    fn workspace_relative(&self, reported: &str) -> String {
        let path = Path::new(reported);
        if !path.is_absolute() {
            return reported.to_string();
        }
        match path.strip_prefix(&self.workspace) {
            // `strip_prefix` compares whole components, so `/workspace-other`
            // is not inside `/workspace` however much string matching would
            // like it to be.
            Ok(relative) if !relative.as_os_str().is_empty() => {
                relative.to_string_lossy().to_string()
            }
            // The workspace root itself, or a path outside it: leave it alone.
            _ => reported.to_string(),
        }
    }

    /// Parse a report and make its paths comparable with the repository.
    ///
    /// `Err` means a report existed and could not be read — acceptance row 5,
    /// finding `REPORT_UNPARSEABLE`, which reconciliation turns into
    /// `CONTRADICTED`. Deciding what an unreadable report *means* is §4.8's job,
    /// not the adapter's.
    fn report_from_json(&self, json: &str) -> AgentResult<AgentReport> {
        let mut report: AgentReport = serde_json::from_str(json).map_err(|source| {
            AgentError::ReportUnparseable(format!(
                "{source}; the report held {}",
                truncate(json.trim(), 120)
            ))
        })?;
        for path in &mut report.files_touched {
            *path = self.workspace_relative(path);
        }
        Ok(report)
    }

    /// One `item.*` payload, when Conductor models it.
    fn item_event(&self, kind: &str, item: &Value) -> Vec<AgentEvent> {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };
        match item_type {
            // Reported the moment it starts. A run killed mid-command (S3's
            // crash matrix) never produces the `item.completed`, and the attempt
            // is exactly the evidence §4.9's detection column needs.
            "command_execution" if kind == "item.started" => vec![AgentEvent::CommandRun {
                command: item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }],
            "file_change" if kind == "item.started" => {
                // **Every** change, not the first. Codex puts an array here,
                // and one edit touching four files is the ordinary case rather
                // than the exotic one. Reporting only the first understated
                // what the agent did on every multi-file edit; widening
                // `parse_event` to a `Vec` at S10 is what let this say the
                // truth — see the trait's own note on why the interface moved
                // instead of the adapter.
                let Some(changes) = item.get("changes").and_then(Value::as_array) else {
                    return Vec::new();
                };
                changes
                    .iter()
                    .map(|change| {
                        let path = self.workspace_relative(
                            change
                                .get("path")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        );
                        match change.get("kind").and_then(Value::as_str) {
                            Some("delete") => AgentEvent::FileDeleted { path },
                            _ => AgentEvent::FileWritten { path },
                        }
                    })
                    .collect()
            }
            // The text only exists once the message is complete. Every one of
            // these is schema-shaped when `--output-schema` is in force, and
            // most of them claim PARTIAL — which is why `extract_report` reads
            // the *last* one and never the first.
            "agent_message" if kind == "item.completed" => {
                // Prose, when the model answers in prose. Not an error: an
                // `agent_message` is not required to be a report.
                match item
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(|text| self.report_from_json(text).ok())
                {
                    Some(report) => vec![AgentEvent::Report { report }],
                    None => Vec::new(),
                }
            }
            // `item.completed` for an item already reported at `item.started`,
            // and every item kind this binary has never heard of.
            _ => Vec::new(),
        }
    }
}

impl AgentAdapter for CodexAgent {
    fn id(&self) -> &str {
        "codex"
    }

    fn capabilities(&self) -> FunctionalCapabilities {
        FunctionalCapabilities {
            // §6.2: identity arrives in `thread.started`. Conductor cannot pick
            // it, and a crash before the first line leaves an unknown session —
            // which costs the resume *optimisation*, not correctness, because
            // the run clone is the evidence.
            conductor_assigned_session_id: false,
            // `codex exec resume <SESSION_ID>`.
            session_resume: true,
            // `--output-schema`, enforced by the agent runtime.
            schema_enforced_final_output: true,
            // `--json`.
            streaming_events: true,
            // `--ignore-user-config`, `--ignore-rules`.
            hermetic_config: true,
            // codex-cli 0.142.0 `exec --help` has no budget flag. Claude Code's
            // `--max-budget-usd` is Claude's (§6.3), and a capability table that
            // flattered Codex would let a later slice build a `billing.spend`
            // gate this adapter cannot satisfy.
            spend_cap: false,
        }
    }

    fn command(&self, input: &StartInput) -> AgentResult<AgentCommand> {
        self.check(input)?;

        let mut args = vec!["exec".to_string()];
        args.extend(self.common_args(&input.report_path));
        // Last, because it is the positional PROMPT. §6.5's packet, which the
        // worker built once the workspace existed — see `StartInput::instructions`
        // for why this cannot be adapter state.
        args.push(input.instructions.clone());

        Ok(AgentCommand {
            program: self.binary.clone(),
            args,
            // The caller's allowlist, unchanged (§4.9). Codex authenticates from
            // `~/.codex/auth.json` or an API key variable, and which of those the
            // child may see is the caller's decision, not the adapter's.
            env: input.env.clone(),
            cwd: input.workspace.clone(),
        })
    }

    fn parse_event(&self, line: &str) -> AgentResult<Vec<AgentEvent>> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        // A half-written line is what a killed agent leaves behind. It is an
        // error so the supervisor records a finding, and it says nothing about
        // the lines before it, which the supervisor keeps.
        let value: Value =
            serde_json::from_str(trimmed).map_err(|source| AgentError::MalformedLine {
                detail: source.to_string(),
                line: truncate(trimmed, 200),
            })?;

        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        Ok(match kind {
            "thread.started" => vec![AgentEvent::Started {
                // Measured: the field is `thread_id`. There is no `session_id`.
                session_id: value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                detail: String::new(),
            }],
            "item.started" | "item.completed" | "item.updated" => match value.get("item") {
                Some(item) => self.item_event(kind, item),
                None => Vec::new(),
            },
            "error" | "turn.failed" => {
                let message = error_message(&value).unwrap_or_else(|| truncate(trimmed, 200));
                vec![AgentEvent::Error {
                    infrastructure: is_infrastructure(&message),
                    message,
                }]
            }
            // `turn.started`, `turn.completed` (usage counters), and every kind
            // released after this binary was built.
            _ => Vec::new(),
        })
    }

    fn extract_report(&self, out: &RunOutputs) -> AgentResult<Option<AgentReport>> {
        // The file `--output-last-message` wrote is the channel `--output-schema`
        // enforces, so it wins over anything on the stream. An unreadable one is
        // row 5, not a reason to go looking for a second opinion.
        if let Some(json) = &out.report_json {
            return self.report_from_json(json).map(Some);
        }

        // Otherwise the **last** schema-shaped `agent_message`. Not the first:
        // the recorded run claims PARTIAL four times before it claims COMPLETE.
        for line in out.stdout_lines.iter().rev() {
            if let Some(AgentEvent::Report { report }) = self
                .parse_event(line)
                .unwrap_or_default()
                .into_iter()
                .next()
            {
                return Ok(Some(report));
            }
        }
        Ok(None)
    }

    fn classify_exit(&self, code: Option<i32>, sig: Option<i32>) -> AttemptOutcome {
        // §6.4, and M15: Codex propagates the exit code of what it ran.
        match (code, sig) {
            (Some(0), None) => AttemptOutcome::Exited,
            (Some(_), _) => AttemptOutcome::Crashed,
            (None, Some(_)) => AttemptOutcome::Crashed,
            // Neither observed. §5.2: unknown must not be recorded as known.
            (None, None) => AttemptOutcome::Stale,
        }
    }

    fn resume_command(&self, input: &ResumeInput) -> Option<AgentCommand> {
        // `None` for anything `command` would refuse: `resume_command` has no
        // error channel, and a resume that hangs on stdin or measures paths
        // against the wrong root is worse than no resume at all.
        self.check(&input.start).ok()?;
        if input.session_id.trim().is_empty() {
            return None;
        }

        let mut args = vec![
            "exec".to_string(),
            "resume".to_string(),
            input.session_id.clone(),
        ];
        args.extend(self.common_args(&input.start.report_path));
        args.push(input.start.instructions.clone());

        Some(AgentCommand {
            program: self.binary.clone(),
            args,
            env: input.start.env.clone(),
            cwd: input.start.workspace.clone(),
        })
    }
}

/// The message an `error` or `turn.failed` event carries, wherever it put it.
///
/// Only the success stream was recorded at S10, so the failure shapes are
/// handled permissively rather than assumed: an upstream change to where the
/// message nests must not turn a reported failure into a silent one.
fn error_message(value: &Value) -> Option<String> {
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }
    match value.get("error") {
        Some(Value::String(message)) => Some(message.clone()),
        Some(error) => error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
        None => None,
    }
}

/// Whether an error is the environment's fault rather than the task's.
///
/// §6.4: an auth or rate-limit error is `CRASHED, kind=infrastructure` — an
/// infra retry that consumes no attempt budget. Charging a task's whole
/// allowance to an expired token would end the task for a reason that has
/// nothing to do with it; calling *everything* infrastructure would make every
/// failure a free retry and defeat §4.6's bounded repair. The list is therefore
/// narrow and literal, and it is a classification of evidence, not a guess about
/// intent.
fn is_infrastructure(message: &str) -> bool {
    const MARKERS: &[&str] = &[
        // Authentication.
        "401",
        "unauthorized",
        "unauthenticated",
        "authentication",
        "not logged in",
        "invalid api key",
        "credential",
        // Rate limiting and quota.
        "429",
        "rate limit",
        "rate-limit",
        "ratelimit",
        "usage limit",
        "quota",
        "too many requests",
        "overloaded",
        "capacity",
    ];
    let lowered = message.to_lowercase();
    MARKERS.iter().any(|marker| lowered.contains(marker))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}
