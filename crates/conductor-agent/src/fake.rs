//! The fake agent's adapter half — pure translation, like every other adapter.
//!
//! The fake exists because "real agents add nothing testable that a fake one
//! does not" (S3's *why now*), and because S10's risk note keeps it as the
//! primary CI harness permanently. It is therefore held to the same rule as the
//! real ones: **this file spawns nothing**. The process lives in
//! `src/bin/conductor-fake-agent.rs`; `conductor-run` starts it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use conductor_core::{AgentReport, AttemptOutcome};
use serde_json::Value;

use crate::error::{AgentError, AgentResult};
use crate::event::AgentEvent;
use crate::{
    AgentAdapter, AgentCommand, FunctionalCapabilities, ResumeInput, RunOutputs, StartInput,
};

/// The environment variable naming the scenario file.
pub const SCENARIO_ENV: &str = "CONDUCTOR_FAKE_SCENARIO";
/// The environment variable naming the report path.
pub const REPORT_ENV: &str = "CONDUCTOR_FAKE_REPORT";
/// The environment variable naming the attempt, for provenance in output.
pub const ATTEMPT_ENV: &str = "CONDUCTOR_FAKE_ATTEMPT";

/// The fake agent adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeAgent {
    binary: PathBuf,
    scenario: PathBuf,
    max_lifetime_ms: u64,
}

impl FakeAgent {
    /// An adapter that runs `binary` against `scenario`.
    ///
    /// Neither path is checked here. §6.1's separation means an adapter has no
    /// business touching the filesystem, and `tests/adapter.rs` relies on being
    /// able to build commands for a program that does not exist.
    pub fn new(binary: PathBuf, scenario: PathBuf) -> Self {
        FakeAgent {
            binary,
            scenario,
            // A hard ceiling on the child's own life. Tests deliberately kill
            // the supervisor and leave the agent running (§6.1: the agent
            // "survives the supervisor's own death"), so something has to
            // guarantee no test can leak a process that outlives the suite.
            max_lifetime_ms: 60_000,
        }
    }

    /// Change the child's hard lifetime ceiling.
    pub fn with_max_lifetime_ms(mut self, ms: u64) -> Self {
        self.max_lifetime_ms = ms;
        self
    }

    /// The scenario file this adapter runs.
    pub fn scenario_path(&self) -> &PathBuf {
        &self.scenario
    }
}

impl AgentAdapter for FakeAgent {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self) -> FunctionalCapabilities {
        FunctionalCapabilities {
            conductor_assigned_session_id: true,
            session_resume: false,
            schema_enforced_final_output: true,
            streaming_events: true,
            hermetic_config: true,
            // Deliberately false: nothing about the fake enforces a budget, and
            // a capability table that flatters the fake would let a later slice
            // build a gate the real adapters cannot satisfy.
            spend_cap: false,
        }
    }

    fn command(&self, input: &StartInput) -> AgentResult<AgentCommand> {
        let mut env: BTreeMap<String, String> = input.env.clone();
        env.insert(
            SCENARIO_ENV.to_string(),
            self.scenario.to_string_lossy().to_string(),
        );
        env.insert(
            REPORT_ENV.to_string(),
            input.report_path.to_string_lossy().to_string(),
        );
        env.insert(
            ATTEMPT_ENV.to_string(),
            format!("{}#{}", input.run_id, input.attempt_ordinal),
        );

        Ok(AgentCommand {
            program: self.binary.clone(),
            args: vec![
                "--scenario".to_string(),
                self.scenario.to_string_lossy().to_string(),
                "--report".to_string(),
                input.report_path.to_string_lossy().to_string(),
                "--max-lifetime-ms".to_string(),
                self.max_lifetime_ms.to_string(),
            ],
            env,
            cwd: input.workspace.clone(),
        })
    }

    fn parse_event(&self, line: &str) -> AgentResult<Option<AgentEvent>> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let value: Value =
            serde_json::from_str(trimmed).map_err(|source| AgentError::MalformedLine {
                detail: source.to_string(),
                line: truncate(trimmed, 200),
            })?;

        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let detail = value
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // An unmodelled kind is `Ok(None)`, never an error: this binary being
        // older than the agent is the normal case, not a fault.
        Ok(match kind {
            "agent.started" => Some(AgentEvent::Started {
                session_id: value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                detail,
            }),
            "file.written" => Some(AgentEvent::FileWritten {
                path: string_field(&value, &["path", "detail"]),
            }),
            "file.deleted" => Some(AgentEvent::FileDeleted {
                path: string_field(&value, &["path", "detail"]),
            }),
            "command.run" => Some(AgentEvent::CommandRun {
                command: string_field(&value, &["command", "detail"]),
            }),
            "checkpoint" => Some(AgentEvent::Checkpoint {
                name: string_field(&value, &["name", "detail"]),
            }),
            "agent.report" => match value.get("report") {
                Some(report) => Some(AgentEvent::Report {
                    report: serde_json::from_value(report.clone())
                        .map_err(|e| AgentError::ReportUnparseable(e.to_string()))?,
                }),
                None => None,
            },
            "agent.error" => Some(AgentEvent::Error {
                message: string_field(&value, &["message", "detail"]),
                infrastructure: value
                    .get("infrastructure")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            "control_socket.attempt" => Some(AgentEvent::ControlSocketAttempt {
                path: string_field(&value, &["path", "detail"]),
                connected: value
                    .get("connected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            _ => None,
        })
    }

    fn extract_report(&self, out: &RunOutputs) -> AgentResult<Option<AgentReport>> {
        // The file wins when there is one: it is the channel a real adapter's
        // `--output-schema` enforces, so a disagreement between the two should
        // resolve towards the enforced one.
        if let Some(json) = &out.report_json {
            let report: AgentReport = serde_json::from_str(json).map_err(|source| {
                AgentError::ReportUnparseable(format!(
                    "{source}; the report file held {}",
                    truncate(json, 120)
                ))
            })?;
            return Ok(Some(report));
        }

        for line in out.stdout_lines.iter().rev() {
            if let Some(AgentEvent::Report { report }) = self.parse_event(line).unwrap_or(None) {
                return Ok(Some(report));
            }
        }
        Ok(None)
    }

    fn classify_exit(&self, code: Option<i32>, sig: Option<i32>) -> AttemptOutcome {
        match (code, sig) {
            (Some(0), None) => AttemptOutcome::Exited,
            (Some(_), _) => AttemptOutcome::Crashed,
            (None, Some(_)) => AttemptOutcome::Crashed,
            // Neither observed. §5.2: unknown must not be recorded as known.
            (None, None) => AttemptOutcome::Stale,
        }
    }

    fn resume_command(&self, _input: &ResumeInput) -> Option<AgentCommand> {
        // The fake has no sessions. Saying so is the point of the Option: an
        // adapter that cannot resume must not hand back a command that quietly
        // starts a fresh one.
        None
    }
}

fn string_field(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(found) = value.get(*key).and_then(Value::as_str) {
            return found.to_string();
        }
    }
    String::new()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}
