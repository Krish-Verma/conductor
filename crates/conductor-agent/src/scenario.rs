//! Fake-agent scenarios — the S3 slice's fifteen behaviours, as data.
//!
//! The fake agent is **not** a stub that returns canned values. It is a real
//! subprocess that writes real files into a real workspace and dies in real
//! ways, driven by a scenario file. That is deliberate: S10's risk note says the
//! fake agent stays "the primary CI harness **forever**", and a harness that
//! cannot corrupt a repository, stall, or die mid-write cannot exercise the
//! recovery paths that are the whole point of this slice.
//!
//! Scenarios are data rather than compiled behaviours so a later slice can add
//! one without changing Conductor.
//!
//! **[`Step::Checkpoint`] is the synchronisation primitive.** A test that wants
//! to `SIGKILL` an agent "after it has written the second file but before the
//! report" reads stdout until the named checkpoint arrives and kills there. No
//! sleeps, no races, and — decisively for M29 — no dependence on how long the
//! operating system spent scanning the binary before it ran.

use serde::{Deserialize, Serialize};

/// One scripted agent behaviour.
///
/// No `deny_unknown_fields`: scenario files are written by hand and by later
/// slices, and an unknown key must never be the reason a test cannot be
/// extended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenario {
    /// Stable identifier.
    pub id: String,
    /// What the scenario is for.
    #[serde(default)]
    pub description: String,
    /// What the agent does, in order.
    pub steps: Vec<Step>,
}

/// One thing the fake agent does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// Emit a JSONL event on stdout.
    Emit {
        /// The `kind` field.
        kind: String,
        /// Free-form detail, merged into the emitted object.
        #[serde(default)]
        detail: String,
    },
    /// Emit a line that is not valid JSON (injected malformed JSONL).
    EmitRaw {
        /// The exact bytes to write, followed by a newline.
        line: String,
    },
    /// Emit a named checkpoint and flush.
    Checkpoint {
        /// The name a test synchronises on.
        name: String,
    },
    /// Write a file inside the workspace.
    WriteFile {
        /// Workspace-relative path.
        path: String,
        /// Contents.
        contents: String,
    },
    /// Delete a file inside the workspace.
    DeleteFile {
        /// Workspace-relative path.
        path: String,
    },
    /// Run `git` inside the workspace.
    Git {
        /// Arguments after `git`.
        args: Vec<String>,
    },
    /// Write the structured report file.
    WriteReport {
        /// `COMPLETE` | `PARTIAL` | `FAILED`.
        claim: String,
        /// Paths the agent claims to have touched.
        #[serde(default)]
        files_touched: Vec<String>,
        /// Prose summary.
        #[serde(default)]
        summary: String,
    },
    /// Write a report file that is not valid JSON (acceptance row 5).
    WriteRawReport {
        /// The exact bytes.
        contents: String,
    },
    /// Emit the report as a stdout event instead of a file.
    ReportOnStdout {
        /// `COMPLETE` | `PARTIAL` | `FAILED`.
        claim: String,
        /// Paths the agent claims to have touched.
        #[serde(default)]
        files_touched: Vec<String>,
        /// Prose summary.
        #[serde(default)]
        summary: String,
    },
    /// Attempt to connect to a unix socket whose path comes from the
    /// environment (acceptance row 28).
    ConnectUnix {
        /// The environment variable holding the socket path.
        path_env: String,
    },
    /// Create a file that must not already exist, and exit non-zero if it does.
    ///
    /// The duplicate-attempt tripwire, and the same mechanism as
    /// `conductor-run`'s owned paths: two agents that believe they are the same
    /// attempt cannot both succeed.
    ClaimPath {
        /// The environment variable holding the path to claim.
        path_env: String,
    },
    /// Sleep.
    SleepMs {
        /// Milliseconds.
        ms: u64,
    },
    /// Go silent until something kills us — the idle-timeout scenario.
    Stall,
    /// Keep emitting output forever — the wall-clock-timeout scenario.
    ///
    /// Distinct from [`Step::Stall`] because §6.4 distinguishes the two timers,
    /// and a fixture that tripped the idle timer would never reach the
    /// wall-clock one.
    Spin {
        /// Milliseconds between lines.
        interval_ms: u64,
    },
    /// Exit with a code.
    Exit {
        /// The exit code.
        code: i32,
    },
    /// `abort(3)` — die on `SIGABRT` without unwinding.
    Abort,
    /// Send ourselves a signal. `KillSelf { signal: 9 }` is a `SIGKILL` at an
    /// exact point in the script, which is stronger than an external kill racing
    /// a sleep.
    KillSelf {
        /// The signal number.
        signal: i32,
    },
}

/// The scenarios the S3 slice names, in the order it names them.
pub fn catalogue() -> Vec<Scenario> {
    vec![
        scenario(
            "success",
            "Edits in scope, a report on stdout, exit 0.",
            vec![
                Step::Emit {
                    kind: "agent.started".to_string(),
                    detail: "success".to_string(),
                },
                Step::WriteFile {
                    path: "src/added.rs".to_string(),
                    contents: "pub fn added() -> u32 { 1 }\n".to_string(),
                },
                Step::Emit {
                    kind: "file.written".to_string(),
                    detail: "src/added.rs".to_string(),
                },
                Step::Checkpoint {
                    name: "after-edits".to_string(),
                },
                Step::ReportOnStdout {
                    claim: "COMPLETE".to_string(),
                    files_touched: vec!["src/added.rs".to_string()],
                    summary: "added a function".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "success-with-report",
            "The same work, with the report delivered as a file rather than on \
             stdout. Both channels exist on both real adapters, so both are \
             exercised from S3 rather than discovered at S10.",
            vec![
                Step::Emit {
                    kind: "agent.started".to_string(),
                    detail: "success-with-report".to_string(),
                },
                Step::WriteFile {
                    path: "src/added.rs".to_string(),
                    contents: "pub fn added() -> u32 { 2 }\n".to_string(),
                },
                Step::Checkpoint {
                    name: "after-edits".to_string(),
                },
                Step::WriteReport {
                    claim: "COMPLETE".to_string(),
                    files_touched: vec!["src/added.rs".to_string()],
                    summary: "added a function".to_string(),
                },
                Step::Checkpoint {
                    name: "after-report".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "partial-edits",
            "Some of the work, honestly reported as partial.",
            vec![
                Step::WriteFile {
                    path: "src/added.rs".to_string(),
                    contents: "pub fn half() -> u32 { 1 }\n".to_string(),
                },
                Step::Checkpoint {
                    name: "first-file".to_string(),
                },
                Step::ReportOnStdout {
                    claim: "PARTIAL".to_string(),
                    files_touched: vec!["src/added.rs".to_string()],
                    summary: "ran out of budget".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "crash-before-edits",
            "Acceptance row 2: dies at once, tree untouched.",
            vec![
                Step::Emit {
                    kind: "agent.started".to_string(),
                    detail: "crash-before-edits".to_string(),
                },
                Step::Checkpoint {
                    name: "before-edits".to_string(),
                },
                Step::KillSelf { signal: 9 },
            ],
        ),
        scenario(
            "crash-after-edits",
            "Acceptance row 3: writes, then dies before reporting. The work is \
             on disk and only the repository knows about it.",
            vec![
                Step::WriteFile {
                    path: "src/added.rs".to_string(),
                    contents: "pub fn survived_a_crash() -> u32 { 3 }\n".to_string(),
                },
                Step::Emit {
                    kind: "file.written".to_string(),
                    detail: "src/added.rs".to_string(),
                },
                Step::Checkpoint {
                    name: "after-edits".to_string(),
                },
                Step::KillSelf { signal: 9 },
            ],
        ),
        scenario(
            "stall",
            "Goes silent. Trips the idle timer: §6.4's TIMED_OUT, reason=stall.",
            vec![
                Step::Emit {
                    kind: "agent.started".to_string(),
                    detail: "stall".to_string(),
                },
                Step::Checkpoint {
                    name: "about-to-stall".to_string(),
                },
                Step::Stall,
            ],
        ),
        scenario(
            "timeout",
            "Never stops working. Trips the wall-clock budget, not the idle \
             timer — the two are different failures and both must be reachable.",
            vec![
                Step::Emit {
                    kind: "agent.started".to_string(),
                    detail: "timeout".to_string(),
                },
                Step::Checkpoint {
                    name: "about-to-spin".to_string(),
                },
                Step::Spin { interval_ms: 20 },
            ],
        ),
        scenario(
            "malformed-report",
            "Acceptance row 5: exit 0 with a report that is not JSON.",
            vec![
                Step::WriteFile {
                    path: "src/added.rs".to_string(),
                    contents: "pub fn edited() -> u32 { 4 }\n".to_string(),
                },
                Step::WriteRawReport {
                    contents: "{\"claim\": \"COMPL".to_string(),
                },
                Step::Checkpoint {
                    name: "after-report".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "missing-report",
            "Acceptance row 4: exit 0, edits made, no report at all.",
            vec![
                Step::WriteFile {
                    path: "src/added.rs".to_string(),
                    contents: "pub fn quiet() -> u32 { 5 }\n".to_string(),
                },
                Step::Checkpoint {
                    name: "after-edits".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "verification-failure",
            "Acceptance row 7: the agent finishes cleanly and the work is wrong. \
             Only a verification runner can tell, which is S4; S3 proves the \
             attempt itself is classified EXITED and reconciled as clean.",
            vec![
                Step::WriteFile {
                    path: "src/added.rs".to_string(),
                    contents: "pub fn broken() -> u32 { \"not a number\" }\n".to_string(),
                },
                Step::ReportOnStdout {
                    claim: "COMPLETE".to_string(),
                    files_touched: vec!["src/added.rs".to_string()],
                    summary: "done".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "same-failure-repeatedly",
            "Acceptance row 9: byte-identical output on every attempt, so two \
             attempts produce the same fingerprint. Nothing in it varies — no \
             clock, no pid, no random value — because a fingerprint that drifts \
             would make the row untestable.",
            vec![
                Step::Emit {
                    kind: "agent.error".to_string(),
                    detail: "cannot resolve module 'missing'".to_string(),
                },
                Step::ReportOnStdout {
                    claim: "FAILED".to_string(),
                    files_touched: vec![],
                    summary: "cannot resolve module 'missing'".to_string(),
                },
                Step::Exit { code: 1 },
            ],
        ),
        scenario(
            "unexpected-dependency-change",
            "Acceptance row 13: edits a dependency manifest. §4.8 makes that \
             POLICY_SENSITIVE regardless of how green anything else is.",
            vec![
                Step::WriteFile {
                    path: "Cargo.toml".to_string(),
                    contents: "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\
                               [dependencies]\nleftpad = \"1\"\n"
                        .to_string(),
                },
                Step::Checkpoint {
                    name: "after-dependency".to_string(),
                },
                Step::ReportOnStdout {
                    claim: "COMPLETE".to_string(),
                    files_touched: vec!["Cargo.toml".to_string()],
                    summary: "added a dependency".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "forbidden-git-change",
            "Acceptance row 14: sets a remote inside the clone. The tree is \
             identical to baseline afterwards, which is exactly why a \
             tree-first classifier would report NO_CHANGE and advance.",
            vec![
                Step::Git {
                    args: vec![
                        "remote".to_string(),
                        "add".to_string(),
                        "origin".to_string(),
                        "https://example.invalid/exfiltrate.git".to_string(),
                    ],
                },
                Step::Checkpoint {
                    name: "after-remote".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "duplicate-attempt",
            "Two processes that both believe they are this attempt. The first \
             claims a path exclusively; a second one exits non-zero rather than \
             overwriting it.",
            vec![
                Step::ClaimPath {
                    path_env: "CONDUCTOR_FAKE_CLAIM_PATH".to_string(),
                },
                Step::Checkpoint {
                    name: "claimed".to_string(),
                },
                Step::WriteFile {
                    path: "src/added.rs".to_string(),
                    contents: "pub fn only_once() -> u32 { 6 }\n".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
        scenario(
            "control-socket",
            "Acceptance row 28: the agent reaches for Conductor's control \
             socket. Under an unsandboxed launcher the connection succeeds and \
             all Conductor can do is notice — which is the finding.",
            vec![
                Step::ConnectUnix {
                    path_env: "CONDUCTOR_SOCK".to_string(),
                },
                Step::Checkpoint {
                    name: "after-socket".to_string(),
                },
                Step::Exit { code: 0 },
            ],
        ),
    ]
}

/// One catalogued scenario by id.
pub fn scenario_by_id(id: &str) -> Option<Scenario> {
    catalogue().into_iter().find(|s| s.id == id)
}

fn scenario(id: &str, description: &str, steps: Vec<Step>) -> Scenario {
    Scenario {
        id: id.to_string(),
        description: description.to_string(),
        steps,
    }
}
