//! Running a verification profile — master plan §4.5.
//!
//! Sequential, in one workspace. §11.2: "No matrix, no cross-check
//! parallelism. If you want a matrix, you want CI."
//!
//! # The shape of one check
//!
//! ```text
//! hash the working tree                     ← the key's tree_hash, and the "before"
//! look the key up in the cache              ← (tree, check, command, toolchain)
//!   hit  → done, nothing runs
//!   miss → spawn, capture, wait
//!          hash the working tree again      ← the "after"
//!          before ≠ after ⇒ VOID + finding
//!          classify, record
//! ```
//!
//! # Two deadlines, because M29 exists
//!
//! macOS scans a freshly built binary on its first execution — 21.7 s cold
//! against 3.3 s warm at S2.5. Measured on this host at S4, that delay is **not
//! inside `Command::spawn()`**: spawn returns in ~200–350 µs either way, and the
//! scan lands between spawn and the child's first instruction (cold total 220 ms
//! against 2 ms warm, for a trivial binary). A single budget starting at spawn
//! would therefore charge the operating system's scan to the check and report a
//! `TIMED_OUT` on a check that had not begun.
//!
//! So, as S3 did for the agent:
//!
//! | budget | measured from | purpose |
//! |---|---|---|
//! | `startup_grace` | `spawn()` | absorbs the scan; ends at the first output byte |
//! | `timeout_seconds` | first output, or `spawn + startup_grace` if silent | §4.5's check budget |
//!
//! A check that says nothing at all is granted `startup_grace + timeout` rather
//! than `timeout`. That over-grant is bounded, deliberate, and errs in the safe
//! direction: a genuine hang is still caught, at worst `startup_grace` late,
//! while a cold start can never be mistaken for one.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as OsCommand, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use conductor_core::{Fence, RunId, VerificationOutcome};
use conductor_git::{TreeHash, TreeHasher};
use conductor_store::Store;
use conductor_store::verification::{CacheKey, RecordOutcome, VerificationRecord};
use serde::Serialize;

use crate::paths::{ArtifactRoot, OwnedDir, Owner};

use super::classify::{self, Classified, Execution, Termination, TreeWitness, VerificationFinding};
use super::profile::{Check, LoadedProfile};
use super::secrets;
use super::toolchain::{self, ToolchainFingerprint};

/// How many lines of a log an excerpt carries.
///
/// Bounded because §4.5 forbids inlining logs; an excerpt is a pointer with
/// enough context to act on, not a copy.
pub const EXCERPT_LINES: usize = 40;

/// `SIGTERM` → `SIGKILL` grace for an overrunning check (§6.4).
pub const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Where a check came from in the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CheckKind {
    /// `verification.required`.
    Required,
    /// `verification.conditional`, and its trigger matched.
    Conditional,
    /// `verification.invariants`.
    Invariant,
}

/// Everything the runner needs that is not in the profile.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// The workspace the checks run in.
    pub workspace: PathBuf,
    /// Where the tree hasher keeps its index. Must be outside the workspace.
    pub scratch_index: PathBuf,
    /// The artifacts tree (§3.1).
    pub artifacts: ArtifactRoot,
    /// The run being verified.
    pub run_id: RunId,
    /// `attempt.ordinal`, which names the log file.
    pub attempt_ordinal: i64,
    /// `attempt.id`, when the attempt is recorded.
    pub attempt_id: Option<String>,
    /// This worker's identity, for path ownership.
    pub owner: Owner,
    /// The environment checks are given — an allowlist (§4.9).
    pub env: BTreeMap<String, String>,
    /// `HEAD` at the time, recorded next to the tree hash.
    pub commit_sha: String,
    /// The diff's changed paths, which gate conditional checks.
    pub changed_paths: Vec<String>,
    /// The M29 absorber. See the module docs.
    pub startup_grace: Duration,
}

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    /// `check.id`.
    pub check_id: String,
    /// Where it came from in the profile.
    pub kind: CheckKind,
    /// The §4.5 outcome.
    pub outcome: VerificationOutcome,
    /// The tree it is bound to.
    pub tree_hash: String,
    /// `blake3` over the resolved argv.
    pub command_hash: String,
    /// Whether this came from the cache instead of running.
    pub from_cache: bool,
    /// The exit code, when the process produced one.
    pub exit_code: Option<i32>,
    /// Wall time across every execution of this check.
    pub duration_ms: i64,
    /// Where the log is. **Never the log's contents** (§4.5).
    pub log_path: Option<PathBuf>,
    /// Content hash of the log.
    pub log_digest: Option<String>,
    /// A bounded, secret-scanned tail of the log.
    pub excerpt: Option<String>,
}

/// What running a profile produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationReport {
    /// The fingerprint every result in this report is keyed by.
    pub toolchain_fingerprint: String,
    /// One entry per check that ran or was served from the cache.
    pub results: Vec<CheckResult>,
    /// Everything a human must look at. Findings never auto-resolve (§4.8).
    pub findings: Vec<VerificationFinding>,
}

impl VerificationReport {
    /// Whether every result in this report is `PASS`.
    ///
    /// Deliberately **not** "no failures": `INCONCLUSIVE` and `VOID` are not
    /// passes, and a completion criterion that accepted them would be exactly
    /// the collapse §4.5 warns about.
    pub fn all_passed(&self) -> bool {
        self.results.iter().all(|r| r.outcome.is_pass())
    }

    /// Results whose outcome is not `PASS`.
    pub fn unresolved(&self) -> Vec<&CheckResult> {
        self.results
            .iter()
            .filter(|r| !r.outcome.is_pass())
            .collect()
    }

    /// One group of results, in the shape §4.5's completion gate reads.
    ///
    /// The mapping lives here, tested, rather than in whichever slice first
    /// needs it. A gate whose inputs each caller assembles by hand is a gate
    /// with as many meanings as it has callers — and the one that matters,
    /// "`PASS` **at the current tree hash**", is exactly the part a hand-rolled
    /// mapping would drop by forgetting to carry `tree_hash`.
    pub fn checks_evidence(&self, kind: CheckKind) -> conductor_core::completion::ChecksEvidence {
        conductor_core::completion::ChecksEvidence::new(
            self.results.iter().filter(|r| r.kind == kind).map(|r| {
                conductor_core::completion::CheckEvidence {
                    check_id: r.check_id.clone(),
                    outcome: r.outcome,
                    tree_hash: r.tree_hash.clone(),
                }
            }),
        )
    }
}

/// Anything that stops a profile from being run at all.
///
/// Note what is *not* here: a check that fails, times out, cannot be spawned or
/// has its tree moved. Those are outcomes, not errors — turning them into `Err`
/// would be the collapse §4.5 spends a paragraph forbidding.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The workspace could not be prepared for hashing.
    #[error("workspace: {0}")]
    Workspace(#[from] conductor_git::GitError),
    /// The artifacts directory could not be claimed.
    #[error("artifacts: {0}")]
    Artifacts(#[from] crate::paths::OwnershipError),
    /// The store rejected a write.
    #[error("store: {0}")]
    Store(#[from] conductor_store::StoreError),
}

/// Run a whole profile.
pub fn run_profile(
    store: &mut Store,
    fence: &Fence,
    config: &RunnerConfig,
    loaded: &LoadedProfile,
    now_ms: i64,
) -> Result<VerificationReport, RunnerError> {
    let hasher = TreeHasher::new(&config.workspace, &config.scratch_index)?;
    let fingerprint = toolchain::fingerprint(
        &loaded.profile.toolchain_fingerprint,
        &config.workspace,
        &config.env,
    );

    let mut findings: Vec<VerificationFinding> = loaded
        .warnings
        .iter()
        .map(|w| VerificationFinding {
            kind: classify::PROFILE_UNKNOWN_KEY,
            detail: format!(
                "{}.{} is not a key Conductor understands; it was ignored, so \
                 whatever it was meant to configure is at its default",
                w.location, w.key
            ),
        })
        .collect();

    let mut schedule: Vec<(CheckKind, &Check)> = Vec::new();
    for check in &loaded.profile.required {
        schedule.push((CheckKind::Required, check));
    }
    for conditional in &loaded.profile.conditional {
        if conditional
            .when
            .changed_paths
            .iter()
            .any(|pattern| config.changed_paths.iter().any(|p| glob_match(pattern, p)))
        {
            for check in &conditional.checks {
                schedule.push((CheckKind::Conditional, check));
            }
        }
    }
    // §4.5: invariants are "cheap, always, never skipped" — they carry no
    // trigger and are appended unconditionally.
    for check in &loaded.profile.invariants {
        schedule.push((CheckKind::Invariant, check));
    }

    let mut results = Vec::with_capacity(schedule.len());
    for (kind, check) in schedule {
        let (result, mut check_findings) = run_one_check(
            store,
            fence,
            config,
            &hasher,
            &fingerprint,
            kind,
            check,
            now_ms,
        )?;
        findings.append(&mut check_findings);
        results.push(result);
    }

    Ok(VerificationReport {
        toolchain_fingerprint: fingerprint.as_str().to_string(),
        results,
        findings,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_one_check(
    store: &mut Store,
    fence: &Fence,
    config: &RunnerConfig,
    hasher: &TreeHasher,
    fingerprint: &ToolchainFingerprint,
    kind: CheckKind,
    check: &Check,
    now_ms: i64,
) -> Result<(CheckResult, Vec<VerificationFinding>), RunnerError> {
    let started = Instant::now();
    let mut findings = Vec::new();
    let command_hash = check.command.command_hash();

    // The tree hash *before* is both the cache key and half the VOID witness.
    // Taking it once is what makes those two the same tree by construction.
    let tree_before = hasher.hash()?;

    let key = CacheKey {
        tree_hash: tree_before.as_str(),
        check_id: &check.id,
        command_hash: &command_hash,
        toolchain_fingerprint: fingerprint.as_str(),
    };

    if let Some(hit) = conductor_store::verification::cached(store.conn(), &key)? {
        return Ok((
            CheckResult {
                check_id: check.id.clone(),
                kind,
                outcome: hit.outcome,
                tree_hash: tree_before.as_str().to_string(),
                command_hash,
                from_cache: true,
                exit_code: hit.exit_code,
                duration_ms: hit.duration_ms.unwrap_or(0),
                log_path: hit.log_path.map(PathBuf::from),
                log_digest: None,
                excerpt: None,
            },
            findings,
        ));
    }

    let dir = config
        .artifacts
        .open_verification_dir(&config.run_id, &config.owner)?;

    // §4.5's `flaky_retry` is "exactly one; disagreement ⇒ INCONCLUSIVE". The
    // retry is spent only on a FAIL: a PASS needs no second opinion, and a
    // timeout or an infrastructure error is already INCONCLUSIVE, whose remedy
    // (§4.5) is a bounded *infra* retry owned by the caller, not this loop.
    let mut runs: Vec<Classified> = Vec::new();
    let mut last: Option<Executed> = None;
    let attempts = check.flaky_retry.saturating_add(1);
    for attempt in 0..attempts {
        let executed = execute(config, hasher, &dir, check, &tree_before, attempt)?;
        let classified = classify::classify(&executed.execution);
        let stop = classified.outcome != VerificationOutcome::Fail;
        runs.push(classified);
        last = Some(executed);
        if stop {
            break;
        }
    }

    let combined = classify::combine_flaky(&runs);
    findings.extend(combined.findings.iter().cloned());

    let executed = last.expect("at least one execution");
    if !executed.secrets.is_empty() {
        findings.push(VerificationFinding {
            kind: classify::SECRET_IN_VERIFICATION_LOG,
            detail: format!(
                "the log of check {:?} matched {} secret pattern(s) ({}); the \
                 excerpt is redacted, the log on disk is not",
                check.id,
                executed.secrets.len(),
                executed
                    .secrets
                    .iter()
                    .map(|k| k.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }

    let duration_ms = started.elapsed().as_millis() as i64;
    let record = VerificationRecord {
        // The row id is derived from the whole cache key, not from part of it.
        // Deriving it from (check, command, tree) alone made two runs at two
        // toolchains collide on the primary key — the cache correctly missed,
        // and the insert then failed. A row id narrower than the key it
        // describes is a second, weaker key nobody declared.
        id: format!(
            "vc-{}-{}-{}",
            config.run_id.as_str(),
            config.attempt_ordinal,
            &conductor_core::effect::content_hash(
                format!(
                    "{}\u{1f}{}\u{1f}{}\u{1f}{}",
                    check.id,
                    command_hash,
                    tree_before,
                    fingerprint.as_str()
                )
                .as_bytes()
            )[7..23]
        ),
        attempt_id: config.attempt_id.clone(),
        commit_sha: config.commit_sha.clone(),
        exit_code: executed.exit_code,
        duration_ms: Some(duration_ms),
        outcome: combined.outcome,
        log_path: Some(executed.log_path.display().to_string()),
    };

    match conductor_store::verification::record(store.conn_mut(), fence, &key, &record, now_ms)? {
        RecordOutcome::Inserted | RecordOutcome::AlreadyPresent => {}
        RecordOutcome::Contradicted { stored } => {
            findings.push(VerificationFinding {
                kind: classify::VERIFICATION_NONDETERMINISM,
                detail: format!(
                    "check {:?} produced {:?} at tree {} where the cache already \
                     holds {:?}, for the same command and toolchain; one of the \
                     two is not a property of the tree",
                    check.id, combined.outcome, tree_before, stored
                ),
            });
        }
    }

    Ok((
        CheckResult {
            check_id: check.id.clone(),
            kind,
            outcome: combined.outcome,
            tree_hash: tree_before.as_str().to_string(),
            command_hash,
            from_cache: false,
            exit_code: executed.exit_code,
            duration_ms,
            log_path: Some(executed.log_path),
            log_digest: Some(executed.log_digest),
            excerpt: Some(executed.excerpt),
        },
        findings,
    ))
}

/// One execution of one check, with its log already written and scanned.
struct Executed {
    execution: Execution,
    exit_code: Option<i32>,
    log_path: PathBuf,
    log_digest: String,
    excerpt: String,
    secrets: Vec<secrets::SecretKind>,
}

fn execute(
    config: &RunnerConfig,
    hasher: &TreeHasher,
    dir: &OwnedDir,
    check: &Check,
    tree_before: &TreeHash,
    attempt: u32,
) -> Result<Executed, RunnerError> {
    // §4.5: artifacts/<run>/verification/<check>-<attempt>.log.
    //
    // That name is **not unique**, and the gap is not hypothetical. A flaky
    // retry runs the same check twice in one attempt, and — more importantly —
    // §4.5 itself says to re-run at the new tree after a `VOID`, which is the
    // same check, the same attempt, a different tree. Opening the same path
    // with `create` would truncate the log holding the evidence behind the
    // finding that caused the re-run.
    //
    // So the plan's name is used where it is unambiguous, and qualified where
    // it is not: retries by index, second trees by tree hash. Both extensions
    // are additive to §4.5's shape rather than a replacement for it.
    let base = if attempt == 0 {
        format!("{}-{}", check.id, config.attempt_ordinal)
    } else {
        format!("{}-{}.retry{}", check.id, config.attempt_ordinal, attempt)
    };
    let preferred = dir.path().join(format!("{base}.log"));
    let log_path = if preferred.exists() {
        let tree = tree_before.as_str();
        dir.path()
            .join(format!("{base}.{}.log", &tree[..tree.len().min(12)]))
    } else {
        preferred
    };

    let captured = capture(config, check, &log_path);

    // The tree hash *after*. This is the line the whole VOID outcome rests on;
    // deleting it turns every mid-check mutation into a silent PASS.
    let tree = match hasher.hash() {
        Ok(after) if &after == tree_before => TreeWitness::Held { tree: after },
        Ok(after) => TreeWitness::Moved {
            before: tree_before.clone(),
            after,
        },
        Err(error) => TreeWitness::Unknown {
            detail: error.to_string(),
        },
    };

    let bytes = std::fs::read(&log_path).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let tail: String = {
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(EXCERPT_LINES)..].join("\n")
    };
    // Scanned on every path out, per §4.5 and §11.2 — including the paths
    // where the check passed, because a leaked credential does not care
    // whether the tests were green.
    let redacted = secrets::redact(&tail);

    Ok(Executed {
        execution: Execution {
            termination: captured.termination,
            tree,
            on_timeout: check.on_timeout,
        },
        exit_code: captured.exit_code,
        log_path,
        log_digest: conductor_core::effect::content_hash(&bytes),
        excerpt: redacted.text,
        secrets: redacted.kinds,
    })
}

struct Captured {
    termination: Termination,
    exit_code: Option<i32>,
}

/// Spawn the check, stream both its streams into the log, and enforce the two
/// deadlines.
fn capture(config: &RunnerConfig, check: &Check, log_path: &Path) -> Captured {
    let argv = check.command.argv();
    let mut os = OsCommand::new(&argv[0]);
    os.args(&argv[1..])
        .current_dir(&config.workspace)
        // §4.9: an allowlist, not a denylist.
        .env_clear()
        .envs(&config.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Put the check in its own process group, so that terminating it
    // terminates everything it started.
    //
    // Found by experiment, not by foresight: `sh -c 'echo x; sleep 60'` with a
    // 1-second budget was killed on time, and the *test* still took 60 seconds.
    // `sh` had forked `sleep`, `sleep` inherited the stdout pipe, and killing
    // only `sh` left a grandchild holding the pipe open. The correctness
    // consequence is worse than the delay: a `cargo test` killed at its
    // deadline would leave compilers and test binaries running **inside the
    // workspace**, writing files after Conductor had taken the after-check tree
    // hash. That is a mutation outside the window this slice exists to police.
    unsafe {
        use std::os::unix::process::CommandExt;
        os.pre_exec(|| {
            // The child becomes its own group leader, so `kill(-pid, …)`
            // reaches it and every descendant.
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // `create_new`, never `create`: a log that already exists is evidence
    // somebody recorded, and truncating it is the S0 clobbering at file
    // granularity (`paths.rs`). The caller has already qualified the name to
    // avoid the legitimate collisions; anything left is a genuine surprise, and
    // a surprise is an infrastructure outcome rather than a silent overwrite.
    let mut log = match std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(log_path)
    {
        Ok(file) => file,
        Err(error) => {
            return Captured {
                // The log could not be opened. That is infrastructure — never a
                // statement about the code under test.
                termination: Termination::Infrastructure {
                    detail: format!("cannot open {}: {error}", log_path.display()),
                },
                exit_code: None,
            };
        }
    };

    let child = match os.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Captured {
                termination: Termination::SpawnFailed {
                    detail: format!("{}: {error}", argv[0]),
                },
                exit_code: None,
            };
        }
    };

    let mut supervised = SupervisedCheck::new(child);
    supervised.run(&mut log, config.startup_grace, check.timeout)
}

enum StreamMessage {
    Line(String),
    Eof,
}

/// A check process, owned. Dropping one kills and reaps it, so no path out of
/// `run` — including a panic — can leak a running check into the workspace.
struct SupervisedCheck {
    child: Child,
    pid: i32,
    stdout_rx: Option<Receiver<StreamMessage>>,
    stderr_rx: Option<Receiver<StreamMessage>>,
    readers: Vec<JoinHandle<()>>,
    reaped: bool,
}

impl SupervisedCheck {
    fn new(mut child: Child) -> SupervisedCheck {
        let pid = child.id() as i32;
        let mut readers = Vec::new();
        let (stdout_tx, stdout_rx) = channel();
        if let Some(stdout) = child.stdout.take() {
            readers.push(reader_thread(stdout, stdout_tx));
        }
        let (stderr_tx, stderr_rx) = channel();
        if let Some(stderr) = child.stderr.take() {
            readers.push(reader_thread(stderr, stderr_tx));
        }
        SupervisedCheck {
            child,
            pid,
            stdout_rx: Some(stdout_rx),
            stderr_rx: Some(stderr_rx),
            readers,
            reaped: false,
        }
    }

    fn run(
        &mut self,
        log: &mut std::fs::File,
        startup_grace: Duration,
        timeout: Duration,
    ) -> Captured {
        let spawned_at = Instant::now();
        let mut first_output_at: Option<Instant> = None;
        let mut timed_out = false;
        let mut log_error: Option<String> = None;
        let mut stdout_done = false;
        let mut stderr_done = false;

        let stdout_rx = self.stdout_rx.take().expect("stdout receiver");
        let stderr_rx = self.stderr_rx.take().expect("stderr receiver");

        loop {
            let mut saw_output = false;
            match stdout_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(StreamMessage::Line(line)) => {
                    saw_output = true;
                    write_line(log, &line, &mut log_error);
                }
                Ok(StreamMessage::Eof) => stdout_done = true,
                Err(RecvTimeoutError::Disconnected) => stdout_done = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
            while let Ok(message) = stderr_rx.try_recv() {
                match message {
                    StreamMessage::Line(line) => {
                        saw_output = true;
                        write_line(log, &line, &mut log_error);
                    }
                    StreamMessage::Eof => stderr_done = true,
                }
            }
            if saw_output {
                first_output_at.get_or_insert_with(Instant::now);
            }

            // A log Conductor could not write is an infrastructure failure, and
            // it stops the check: continuing would produce a verdict with no
            // evidence behind it.
            if let Some(detail) = &log_error {
                let detail = detail.clone();
                self.terminate();
                return Captured {
                    termination: Termination::Infrastructure { detail },
                    exit_code: None,
                };
            }

            if let Ok(Some(status)) = self.child.try_wait() {
                self.reaped = true;
                self.drain(log, &stdout_rx, &stderr_rx, &mut log_error);
                if let Some(detail) = log_error {
                    return Captured {
                        termination: Termination::Infrastructure { detail },
                        exit_code: None,
                    };
                }
                return finish(status, timed_out);
            }

            // The deadline: `timeout` from the first output byte, or from
            // `spawn + startup_grace` when the check has said nothing yet. See
            // the module docs for why the second clause exists.
            let deadline_from = first_output_at.unwrap_or(spawned_at + startup_grace);
            if !timed_out && Instant::now() > deadline_from + timeout {
                timed_out = true;
                self.terminate();
            }

            if stdout_done && stderr_done && !self.reaped {
                // Both pipes are closed: the process cannot produce more, so a
                // blocking wait cannot hang. `try_wait` in the loop above can
                // miss the narrow window where the child is gone but its status
                // is not yet collectable (the same window S3 documents).
                match self.child.wait() {
                    Ok(status) => {
                        self.reaped = true;
                        return finish(status, timed_out);
                    }
                    Err(error) => {
                        self.reaped = true;
                        return Captured {
                            termination: Termination::Infrastructure {
                                detail: format!("cannot collect the check's status: {error}"),
                            },
                            exit_code: None,
                        };
                    }
                }
            }
        }
    }

    fn drain(
        &self,
        log: &mut std::fs::File,
        stdout_rx: &Receiver<StreamMessage>,
        stderr_rx: &Receiver<StreamMessage>,
        log_error: &mut Option<String>,
    ) {
        for rx in [stdout_rx, stderr_rx] {
            // Waits for EOF rather than for a timeout: the reader threads may
            // still be holding lines the check wrote just before exiting, and
            // dropping those would lose the end of the log — which is the part
            // an excerpt is made of.
            while let Ok(StreamMessage::Line(line)) = rx.recv_timeout(Duration::from_millis(100)) {
                write_line(log, &line, log_error);
            }
        }
    }

    /// §6.4: `SIGTERM`, grace, `SIGKILL` — to the whole process group.
    ///
    /// The negative pid is the point: `kill(-pgid, …)` reaches every descendant
    /// the check spawned. Killing only the direct child leaves grandchildren
    /// running in the workspace after the result has been classified.
    fn terminate(&mut self) {
        self.signal_group(libc::SIGTERM);
        let deadline = Instant::now() + TERMINATE_GRACE;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                self.reaped = true;
                // The leader is gone; sweep anything it left behind.
                self.signal_group(libc::SIGKILL);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.signal_group(libc::SIGKILL);
        let _ = self.child.wait();
        self.reaped = true;
    }

    fn signal_group(&self, signal: i32) {
        unsafe {
            // The group first, then the leader itself: if `setpgid` in
            // `pre_exec` failed the child is still in Conductor's own group,
            // and `kill(-pid)` would then be a no-op — never a signal to
            // Conductor's group, because `-pid` is not `-our_pgid`.
            libc::kill(-self.pid, signal);
            libc::kill(self.pid, signal);
        }
    }
}

impl Drop for SupervisedCheck {
    fn drop(&mut self) {
        if !self.reaped {
            self.signal_group(libc::SIGKILL);
            let _ = self.child.wait();
        }
        for handle in std::mem::take(&mut self.readers) {
            let _ = handle.join();
        }
    }
}

fn finish(status: std::process::ExitStatus, timed_out: bool) -> Captured {
    use std::os::unix::process::ExitStatusExt;

    if timed_out {
        // A timeout outranks the signal Conductor used to enforce it —
        // otherwise Conductor's own decision would be reported as the check
        // crashing (the rule §6.4 states for agents, for the same reason).
        return Captured {
            termination: Termination::TimedOut,
            exit_code: status.code(),
        };
    }
    match (status.code(), status.signal()) {
        (Some(code), _) => Captured {
            termination: Termination::Exited { code },
            exit_code: Some(code),
        },
        (None, Some(signal)) => Captured {
            termination: Termination::Signalled { signal },
            exit_code: None,
        },
        (None, None) => Captured {
            termination: Termination::Infrastructure {
                detail: "the check produced neither an exit code nor a signal".to_string(),
            },
            exit_code: None,
        },
    }
}

fn write_line(log: &mut std::fs::File, line: &str, log_error: &mut Option<String>) {
    if log_error.is_some() {
        return;
    }
    if let Err(error) = writeln!(log, "{line}") {
        *log_error = Some(format!("cannot write the check log: {error}"));
    }
}

fn reader_thread<R: std::io::Read + Send + 'static>(
    stream: R,
    tx: Sender<StreamMessage>,
) -> JoinHandle<()> {
    use std::io::BufRead;

    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stream);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(StreamMessage::Line(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(StreamMessage::Eof);
    })
}

/// Glob matching for `when: {changed_paths: […]}`.
///
/// The same tiny subset `conductor-git` uses for scope: `**` spans segments,
/// `*` and `?` match within one, everything else is literal.
fn glob_match(pattern: &str, path: &str) -> bool {
    conductor_git::reconcile::Scope::new([pattern.to_string()]).contains(path)
}
