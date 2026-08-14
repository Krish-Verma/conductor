//! The process supervisor — master plan §6.1, §6.4, §4.7.
//!
//! > Conductor drives agents as subprocesses, never in-process SDK calls. A
//! > subprocess can be `SIGKILL`ed, inspected, resource-limited, launched with a
//! > scrubbed environment, and — decisively — **survives the supervisor's own
//! > death**. (§6.1)
//!
//! # Why `std` threads and not `tokio`
//!
//! §2.2 authorises `tokio` "(process supervision)", and CLAUDE.md requires a
//! justification for any dependency. A supervisor for **one** child needs two
//! threads: one draining stdout and one draining stderr. The main thread does
//! the rest with `recv_timeout`. There is no fan-out, no task scheduling, no
//! async I/O multiplexing and — until S14 — no daemon to host a runtime. An
//! async runtime here would add a dependency tree and a scheduler in order to
//! replace twenty lines of `recv_timeout`, and it would not make the one hard
//! part (killing and reaping on every path, including a panic) any easier: that
//! is `Drop`, which is the same in both worlds. `tokio` was therefore **not**
//! taken. If S14's daemon supervises many children at once, that is the slice
//! that should re-examine it, with a concurrency figure in hand.
//!
//! # Why "always reaped" is structural
//!
//! §2.2's case for Rust names child-process lifecycle as "the highest-leak
//! surface in the system (spawn, stream, timeout, `SIGTERM`→`SIGKILL`, reap,
//! orphan-detect, on every path including panics)" and says "ownership and
//! `Drop` make 'always reaped' structural rather than disciplinary". So the
//! child is owned by [`SpawnedAgent`], whose `Drop` kills and reaps anything
//! still alive. A panic in the supervisor's own code cannot leak the agent,
//! because unwinding runs `Drop`.
//!
//! # Three deadlines, not one — and why M29 forces that
//!
//! macOS scans a freshly built binary on its **first execution**: 21.7 s cold
//! against 3.3 s warm on this host (M29, measured at S2.5 where it made a probe
//! exceed its deadline). That time is spent before the child's first
//! instruction. A supervisor with a single budget starting at `spawn()` would
//! therefore classify a cold binary as a stalled agent — a false `TIMED_OUT` on
//! a run that had not begun.
//!
//! | deadline | measured from | exceeded means |
//! |---|---|---|
//! | `startup_timeout` | `spawn()` | the process never spoke: `reason=no_startup` |
//! | `idle_timeout` | last output | it went quiet: `reason=stall` (§6.4) |
//! | `wall_timeout` | **first** output | the work itself overran: `reason=wall_clock` |
//!
//! The agent's own budgets run from its first line, so the operating system's
//! scan is charged to the startup budget — which is generous and diagnostic —
//! and never to the agent.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use conductor_agent::{AgentAdapter, AgentCommand, AgentEvent};
use conductor_core::attempt::{TIMEOUT_NO_STARTUP, TIMEOUT_STALL, TIMEOUT_WALL_CLOCK};

/// How long the supervisor waits at each stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorConfig {
    /// `spawn()` → first output. The M29 absorber; generous by design.
    pub startup_timeout: Duration,
    /// Last output → now (§6.4: `TIMED_OUT`, `reason=stall`).
    pub idle_timeout: Duration,
    /// First output → now. The agent's actual work budget.
    pub wall_timeout: Duration,
    /// `SIGTERM` → `SIGKILL` (§6.4).
    pub terminate_grace: Duration,
    /// How often the loop wakes when nothing is happening.
    pub poll_interval: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            // 60 s: comfortably over M29's measured 21.7 s cold start, and this
            // budget is only ever hit by an agent that produced nothing at all.
            startup_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(300),
            wall_timeout: Duration::from_secs(1800),
            terminate_grace: Duration::from_secs(10),
            poll_interval: Duration::from_millis(25),
        }
    }
}

/// How supervision ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisionEnd {
    /// The process exited and the code was observed.
    Exited {
        /// The exit code.
        code: i32,
    },
    /// The process died on a signal.
    Signalled {
        /// The signal number.
        signal: i32,
    },
    /// Conductor killed the process for exceeding a budget.
    TimedOut {
        /// `no_startup`, `stall` or `wall_clock`.
        reason: &'static str,
    },
    /// The process is gone and no status was obtainable. **We do not know.**
    Vanished,
}

/// Everything one supervised attempt produced.
#[derive(Debug, Clone)]
pub struct Supervised {
    /// How it ended.
    pub end: SupervisionEnd,
    /// The child's pid.
    pub pid: Option<i32>,
    /// The child's start time, microseconds since the epoch.
    pub pid_start_time: Option<i64>,
    /// Every stdout line, in order.
    pub stdout_lines: Vec<String>,
    /// Everything the child wrote to stderr.
    pub stderr: String,
    /// Events the adapter recognised.
    pub events: Vec<AgentEvent>,
    /// Lines the adapter could not parse. Recorded, never fatal.
    pub parse_errors: Vec<String>,
    /// When the process was spawned.
    pub spawned_at: Instant,
    /// When it first said anything.
    pub first_output_at: Option<Instant>,
    /// The signal Conductor sent, when it killed the process itself.
    ///
    /// `SIGTERM` when the grace period was enough; `SIGKILL` when it was not.
    /// Recorded because "the agent ignored SIGTERM" is a fact about the adapter
    /// worth having.
    pub termination_signal: Option<i32>,
}

/// What the heartbeat callback is told on each tick.
#[derive(Debug, Clone)]
pub struct Heartbeat<'a> {
    /// Proof the child is alive, or `None`.
    ///
    /// §4.7: heartbeating "conditional on the agent process still existing
    /// (`kill(pid, 0)`) — a supervisor that heartbeats while its child is dead
    /// is worse than one that crashes." The witness is not a boolean the caller
    /// may ignore: [`crate::lease::heartbeat`] takes a [`ChildAlive`], so a
    /// heartbeat without one does not compile.
    pub alive: Option<&'a ChildAlive>,
    /// How long since the process was spawned.
    pub elapsed: Duration,
    /// How many stdout lines have been seen.
    pub lines_seen: usize,
}

/// Proof that a specific process — the *same* process, not merely the same pid
/// — was alive at the moment it was checked.
///
/// The only constructor is [`probe`]. Fields are private, so a caller cannot
/// assert liveness it has not observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildAlive {
    pid: i32,
    start_time_us: i64,
}

impl ChildAlive {
    /// The pid that was observed alive.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Its start time, microseconds since the epoch.
    pub fn start_time_us(&self) -> i64 {
        self.start_time_us
    }
}

/// The answer to "is the process I recorded still there?"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// It is there, and it is the same process.
    Alive(ChildAlive),
    /// There is no such process.
    Dead,
    /// A process with that pid exists, but it started at a different time —
    /// so it is **not** the recorded one (§4.7 step 3).
    Recycled {
        /// The start time the live process actually has.
        actual_start: i64,
    },
}

/// A process's start time in microseconds since the epoch, or `None` if there is
/// no such process.
///
/// `proc_pidinfo(PROC_PIDTBSDINFO)`: exact, and it distinguishes "reaped" from
/// "alive" where `kill(pid, 0)` cannot — `kill` succeeds against a zombie.
pub fn start_time_us(pid: i32) -> Option<i64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let rc = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if rc != size {
        return None;
    }
    Some(info.pbi_start_tvsec as i64 * 1_000_000 + info.pbi_start_tvusec as i64)
}

/// Probe a recorded process identity — §4.7 step 3.
///
/// > Probe recorded pid → alive & start-time matches?
/// >     alive → adopt or terminate (config); record.
/// >     dead  → attempt := STALE.
///
/// Pass `expected_start_time = 0` to ask only whether *anything* is there. Any
/// other value is checked, because a pid on its own is not an identity: pids are
/// recycled, and adopting a stranger's process is worse than adopting nothing.
pub fn probe(pid: i32, expected_start_time: i64) -> Liveness {
    let Some(actual) = start_time_us(pid) else {
        return Liveness::Dead;
    };
    if expected_start_time == 0 || actual == expected_start_time {
        Liveness::Alive(ChildAlive {
            pid,
            start_time_us: actual,
        })
    } else {
        Liveness::Recycled {
            actual_start: actual,
        }
    }
}

enum StreamMessage {
    Line(String),
    Eof,
}

/// A spawned agent process, owned.
///
/// Dropping one kills and reaps the child if it is still running. That is the
/// whole of "always reaped": there is no path — return, `?`, or panic — that
/// skips it.
#[derive(Debug)]
pub struct SpawnedAgent {
    child: Child,
    pid: i32,
    pid_start_time: i64,
    spawned_at: Instant,
    stdout_rx: Option<Receiver<StreamMessage>>,
    stderr_rx: Option<Receiver<StreamMessage>>,
    readers: Vec<JoinHandle<()>>,
    reaped: bool,
    /// Which signal Conductor sent, when it was Conductor that ended the
    /// process. Held on the struct rather than threaded through every return
    /// path, because the kill happens mid-loop and the fact has to survive to
    /// whichever exit the loop then takes.
    termination_signal: Option<i32>,
}

/// Spawn an agent. Performs no supervision.
pub fn spawn(command: &AgentCommand) -> std::io::Result<SpawnedAgent> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .current_dir(&command.cwd)
        // §4.9: an allowlist, not a denylist. `env_clear` first, so nothing the
        // supervisor happens to have inherited reaches the agent.
        .env_clear()
        .envs(&command.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    let spawned_at = Instant::now();
    // Read the start time immediately: it is the half of the process identity
    // that survives pid reuse, and it must be captured while the child is
    // certainly alive.
    let pid_start_time = start_time_us(pid).unwrap_or(0);

    let mut readers = Vec::new();
    let (stdout_tx, stdout_rx) = channel();
    if let Some(stdout) = child.stdout.take() {
        readers.push(reader_thread(stdout, stdout_tx));
    }
    let (stderr_tx, stderr_rx) = channel();
    if let Some(stderr) = child.stderr.take() {
        readers.push(reader_thread(stderr, stderr_tx));
    }

    Ok(SpawnedAgent {
        child,
        pid,
        pid_start_time,
        spawned_at,
        stdout_rx: Some(stdout_rx),
        stderr_rx: Some(stderr_rx),
        readers,
        reaped: false,
        termination_signal: None,
    })
}

fn reader_thread<R: std::io::Read + Send + 'static>(
    stream: R,
    tx: Sender<StreamMessage>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(StreamMessage::Line(line)).is_err() {
                        return;
                    }
                }
                // A read error on a pipe whose writer died is the same event as
                // EOF for our purposes: there will be no more output.
                Err(_) => break,
            }
        }
        let _ = tx.send(StreamMessage::Eof);
    })
}

impl SpawnedAgent {
    /// The child's pid.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The child's start time, microseconds since the epoch.
    pub fn pid_start_time(&self) -> i64 {
        self.pid_start_time
    }

    /// When it was spawned.
    pub fn spawned_at(&self) -> Instant {
        self.spawned_at
    }

    /// Whether the child — that exact process — is still there.
    pub fn liveness(&self) -> Liveness {
        probe(self.pid, self.pid_start_time)
    }

    /// Supervise the child to completion.
    ///
    /// `on_tick` is called on every poll while the child is alive, and is handed
    /// a liveness witness. It is where a caller heartbeats its lease.
    pub fn supervise<F>(
        mut self,
        adapter: &dyn AgentAdapter,
        config: &SupervisorConfig,
        mut on_tick: F,
    ) -> Supervised
    where
        F: FnMut(&Heartbeat<'_>),
    {
        let mut stdout_lines: Vec<String> = Vec::new();
        let mut stderr_lines: Vec<String> = Vec::new();
        let mut events: Vec<AgentEvent> = Vec::new();
        let mut parse_errors: Vec<String> = Vec::new();

        let mut first_output_at: Option<Instant> = None;
        let mut last_activity = self.spawned_at;
        let mut stdout_done = false;
        let mut timed_out: Option<&'static str> = None;

        let stdout_rx = self.stdout_rx.take().expect("stdout receiver");
        let stderr_rx = self.stderr_rx.take().expect("stderr receiver");

        loop {
            // Drain whatever is waiting, then decide. Draining first means a
            // process that produced output and exited in the same instant has
            // its output attributed to it rather than lost to the exit check.
            let mut lines_this_pass = 0usize;
            loop {
                match stdout_rx.recv_timeout(config.poll_interval) {
                    Ok(StreamMessage::Line(line)) => {
                        lines_this_pass += 1;
                        first_output_at.get_or_insert_with(Instant::now);
                        last_activity = Instant::now();
                        match adapter.parse_event(&line) {
                            Ok(Some(event)) => events.push(event),
                            Ok(None) => {}
                            // A malformed line is evidence, not a reason to stop
                            // reading. Acceptance: "malformed JSONL".
                            Err(error) => parse_errors.push(error.to_string()),
                        }
                        stdout_lines.push(line);
                    }
                    Ok(StreamMessage::Eof) => {
                        stdout_done = true;
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        stdout_done = true;
                        break;
                    }
                }
                if lines_this_pass >= 64 {
                    // Yield to the deadline checks periodically so a spinning
                    // agent cannot keep the loop inside the drain forever.
                    break;
                }
            }

            while let Ok(message) = stderr_rx.try_recv() {
                if let StreamMessage::Line(line) = message {
                    stderr_lines.push(line);
                }
            }

            // Has it exited?
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    let end = classify_status(&status, timed_out);
                    self.drain_after_exit(
                        adapter,
                        &stdout_rx,
                        &stderr_rx,
                        &mut stdout_lines,
                        &mut stderr_lines,
                        &mut events,
                        &mut parse_errors,
                        &mut first_output_at,
                    );
                    return self.finish(
                        end,
                        stdout_lines,
                        stderr_lines,
                        events,
                        parse_errors,
                        first_output_at,
                    );
                }
                Ok(None) => {}
                Err(_) => {
                    // The status is unobtainable. Unknown must not be recorded
                    // as known (§5.2).
                    return self.finish(
                        SupervisionEnd::Vanished,
                        stdout_lines,
                        stderr_lines,
                        events,
                        parse_errors,
                        first_output_at,
                    );
                }
            }

            let alive = match probe(self.pid, self.pid_start_time) {
                Liveness::Alive(witness) => Some(witness),
                // Alive-but-recycled is impossible for a child we hold, and
                // dead-but-not-reaped is a zombie awaiting `try_wait` above.
                _ => None,
            };
            if let Some(witness) = &alive {
                on_tick(&Heartbeat {
                    alive: Some(witness),
                    elapsed: self.spawned_at.elapsed(),
                    lines_seen: stdout_lines.len(),
                });
            }

            if timed_out.is_none()
                && let Some(reason) = self.overdue(config, first_output_at, last_activity)
            {
                {
                    timed_out = Some(reason);
                    // §6.4: SIGTERM, grace, SIGKILL.
                    self.terminate(config.terminate_grace);
                }
            }

            // Both the process gone and the stream closed: nothing more can
            // happen, so collect the status.
            //
            // **Blocking `wait`, not `try_wait`.** There is a window during
            // teardown where the process is no longer visible to
            // `proc_pidinfo` but `waitpid(WNOHANG)` has not yet been able to
            // report its status. Treating that window as "vanished" would
            // record `STALE` — "we do not know" — for a process whose exit code
            // was one syscall away, which is the opposite of the honesty §5.2
            // asks for: unknown must not be recorded as known, and *known must
            // not be discarded as unknown either*. The wait cannot hang: the
            // pipe is at EOF and the process is unrunnable.
            if stdout_done && matches!(probe(self.pid, self.pid_start_time), Liveness::Dead) {
                match self.child.wait() {
                    Ok(status) => {
                        self.reaped = true;
                        let end = classify_status(&status, timed_out);
                        return self.finish(
                            end,
                            stdout_lines,
                            stderr_lines,
                            events,
                            parse_errors,
                            first_output_at,
                        );
                    }
                    Err(_) => {
                        self.reaped = true;
                        return self.finish(
                            SupervisionEnd::Vanished,
                            stdout_lines,
                            stderr_lines,
                            events,
                            parse_errors,
                            first_output_at,
                        );
                    }
                }
            }
        }
    }

    fn overdue(
        &self,
        config: &SupervisorConfig,
        first_output_at: Option<Instant>,
        last_activity: Instant,
    ) -> Option<&'static str> {
        match first_output_at {
            // Nothing yet. Only the startup budget applies, and it is the one
            // that absorbs M29's first-execution scan.
            None => {
                if self.spawned_at.elapsed() > config.startup_timeout {
                    Some(TIMEOUT_NO_STARTUP)
                } else {
                    None
                }
            }
            Some(first) => {
                if last_activity.elapsed() > config.idle_timeout {
                    Some(TIMEOUT_STALL)
                } else if first.elapsed() > config.wall_timeout {
                    Some(TIMEOUT_WALL_CLOCK)
                } else {
                    None
                }
            }
        }
    }

    /// `SIGTERM`, grace, `SIGKILL` (§6.4).
    ///
    /// The escalation is not optional and not configurable away: an agent that
    /// ignores `SIGTERM` still dies, and one that handles it gets the chance to
    /// shut down cleanly first.
    fn terminate(&mut self, grace: Duration) {
        unsafe {
            libc::kill(self.pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                self.reaped = true;
                self.termination_signal = Some(libc::SIGTERM);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
        self.reaped = true;
        self.termination_signal = Some(libc::SIGKILL);
    }

    #[allow(clippy::too_many_arguments)]
    fn drain_after_exit(
        &self,
        adapter: &dyn AgentAdapter,
        stdout_rx: &Receiver<StreamMessage>,
        stderr_rx: &Receiver<StreamMessage>,
        stdout_lines: &mut Vec<String>,
        stderr_lines: &mut Vec<String>,
        events: &mut Vec<AgentEvent>,
        parse_errors: &mut Vec<String>,
        first_output_at: &mut Option<Instant>,
    ) {
        // The reader threads may still hold buffered lines the child wrote just
        // before exiting. Losing them would mean losing a report, so the drain
        // waits for EOF rather than for a timeout.
        loop {
            match stdout_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(StreamMessage::Line(line)) => {
                    first_output_at.get_or_insert_with(Instant::now);
                    match adapter.parse_event(&line) {
                        Ok(Some(event)) => events.push(event),
                        Ok(None) => {}
                        Err(error) => parse_errors.push(error.to_string()),
                    }
                    stdout_lines.push(line);
                }
                Ok(StreamMessage::Eof) => break,
                Err(_) => break,
            }
        }
        loop {
            match stderr_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(StreamMessage::Line(line)) => stderr_lines.push(line),
                Ok(StreamMessage::Eof) => break,
                Err(_) => break,
            }
        }
    }

    fn finish(
        mut self,
        end: SupervisionEnd,
        stdout_lines: Vec<String>,
        stderr_lines: Vec<String>,
        events: Vec<AgentEvent>,
        parse_errors: Vec<String>,
        first_output_at: Option<Instant>,
    ) -> Supervised {
        let termination_signal = self.termination_signal;
        // Join the reader threads so nothing outlives this call. They end when
        // the pipes close, which the child's death guarantees.
        for handle in std::mem::take(&mut self.readers) {
            let _ = handle.join();
        }
        Supervised {
            end,
            pid: Some(self.pid),
            pid_start_time: Some(self.pid_start_time),
            stdout_lines,
            stderr: stderr_lines.join("\n"),
            events,
            parse_errors,
            spawned_at: self.spawned_at,
            first_output_at,
            termination_signal,
        }
    }
}

impl Drop for SpawnedAgent {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // §2.2: always reaped, structurally. This runs on every path out of
        // `supervise`, on an early return, and on a panic that unwinds through
        // the owner.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        let _ = self.child.wait();
        for handle in std::mem::take(&mut self.readers) {
            let _ = handle.join();
        }
    }
}

fn classify_status(
    status: &std::process::ExitStatus,
    timed_out: Option<&'static str>,
) -> SupervisionEnd {
    use std::os::unix::process::ExitStatusExt;

    // A timeout wins over whatever signal Conductor used to enforce it. The
    // process died of SIGTERM because Conductor sent it; reporting `CRASHED`
    // would attribute Conductor's own decision to the agent.
    if let Some(reason) = timed_out {
        return SupervisionEnd::TimedOut { reason };
    }
    if let Some(code) = status.code() {
        return SupervisionEnd::Exited { code };
    }
    if let Some(signal) = status.signal() {
        return SupervisionEnd::Signalled { signal };
    }
    SupervisionEnd::Vanished
}
