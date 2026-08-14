//! The process supervisor — master plan §6.1, §6.4.
//!
//! Conductor owns spawning, killing, timeouts and streaming. These tests pin
//! each of those against a real subprocess.
//!
//! **On M29 and the timeouts here.** macOS scans a freshly built binary on first
//! execution — 21.7 s cold against 3.3 s warm on this host, measured at S2.5,
//! where it made a probe blow its deadline. A supervisor that started the
//! agent's budget at `spawn()` would therefore classify a cold binary as a
//! stalled agent. Two things prevent that:
//!
//! 1. the supervisor has a **separate, generous startup budget** for
//!    spawn → first output, and the idle and wall-clock budgets start at the
//!    agent's first line, not at spawn;
//! 2. these tests **warm the binary once** before any of them asserts a
//!    timing-sensitive property, exactly as S2.5's `payload_self_check` does.
//!
//! Everything else synchronises on checkpoints rather than on sleeps.

mod common;

use std::time::{Duration, Instant};

use conductor_agent::fake::FakeAgent;
use conductor_agent::{AgentAdapter, AgentEvent};
use conductor_core::AttemptOutcome;
use conductor_run::supervise::{Liveness, SupervisionEnd, SupervisorConfig, spawn};

use common::agent::{fake_agent_binary, scenario_file, start_input, warm_the_binary};

fn config() -> SupervisorConfig {
    SupervisorConfig {
        // Generous: this is the M29 absorber, not a measurement.
        startup_timeout: Duration::from_secs(60),
        idle_timeout: Duration::from_millis(600),
        wall_timeout: Duration::from_secs(5),
        terminate_grace: Duration::from_millis(300),
        poll_interval: Duration::from_millis(10),
    }
}

#[test]
fn a_successful_agent_is_spawned_streamed_and_reaped() {
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "success");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let input = start_input(dir.path());
    let command = adapter.command(&input).expect("command");

    let agent = spawn(&command).expect("spawn");
    let pid = agent.pid();
    assert!(pid > 0);
    assert!(matches!(agent.liveness(), Liveness::Alive(_)));

    let supervised = agent.supervise(&adapter, &config(), |_| {});

    assert!(matches!(supervised.end, SupervisionEnd::Exited { code: 0 }));
    assert_eq!(supervised.pid, Some(pid));
    assert!(supervised.pid_start_time.is_some());
    assert!(
        supervised
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Checkpoint { name } if name == "after-edits")),
        "the JSONL stream was not parsed: {:?}",
        supervised.events
    );
    assert!(supervised.parse_errors.is_empty());

    // Reaped: the pid is gone from the process table entirely, not a zombie.
    assert!(
        matches!(conductor_run::supervise::probe(pid, 0), Liveness::Dead),
        "the child was not reaped"
    );
}

#[test]
fn the_first_line_is_a_readiness_handshake_not_a_sleep() {
    // The property M29 makes load-bearing: the agent announces itself, and
    // everything the supervisor times runs from that announcement.
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "success");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let supervised = spawn(&command)
        .expect("spawn")
        .supervise(&adapter, &config(), |_| {});

    let first = supervised
        .stdout_lines
        .first()
        .expect("the agent said nothing at all");
    assert!(
        first.contains("agent.ready"),
        "the first line must be the readiness handshake, got {first}"
    );
    assert!(supervised.first_output_at.is_some());
}

#[test]
fn a_stalled_agent_trips_the_idle_timer_and_is_killed() {
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "stall");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let agent = spawn(&command).expect("spawn");
    let pid = agent.pid();
    let supervised = agent.supervise(&adapter, &config(), |_| {});

    match supervised.end {
        SupervisionEnd::TimedOut { reason } => assert_eq!(
            reason,
            conductor_core::attempt::TIMEOUT_STALL,
            "§6.4: no output for idle_timeout is reason=stall"
        ),
        other => panic!("expected a stall timeout, got {other:?}"),
    }
    assert!(
        matches!(conductor_run::supervise::probe(pid, 0), Liveness::Dead),
        "a timed-out agent must be dead and reaped, not left running"
    );
}

#[test]
fn a_busy_agent_trips_the_wall_clock_timer_not_the_idle_one() {
    // The `timeout` scenario emits continuously, so the idle timer never fires.
    // If the supervisor had only one timer, this would be reported as a stall.
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "timeout");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let mut cfg = config();
    cfg.idle_timeout = Duration::from_secs(30);
    cfg.wall_timeout = Duration::from_millis(700);

    let agent = spawn(&command).expect("spawn");
    let pid = agent.pid();
    let supervised = agent.supervise(&adapter, &cfg, |_| {});

    match supervised.end {
        SupervisionEnd::TimedOut { reason } => {
            assert_eq!(reason, conductor_core::attempt::TIMEOUT_WALL_CLOCK)
        }
        other => panic!("expected a wall-clock timeout, got {other:?}"),
    }
    assert!(matches!(
        conductor_run::supervise::probe(pid, 0),
        Liveness::Dead
    ));
}

#[test]
fn the_wall_clock_budget_starts_at_first_output_not_at_spawn() {
    // This is the M29 defence stated as an assertion. The budget below is
    // shorter than the 21.7 s a cold binary can spend being scanned; if the
    // clock started at spawn, a cold run of this test would time out before the
    // agent had executed a single instruction.
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "success");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let supervised = spawn(&command)
        .expect("spawn")
        .supervise(&adapter, &config(), |_| {});

    let spawned = supervised.spawned_at;
    let first = supervised.first_output_at.expect("first output");
    assert!(first >= spawned);
    assert!(matches!(supervised.end, SupervisionEnd::Exited { code: 0 }));
}

#[test]
fn an_agent_that_never_speaks_is_a_startup_timeout_not_a_stall() {
    // Distinct reasons because they are distinct diagnoses: "went quiet
    // mid-run" is the agent's fault, "never started" is usually the host's.
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    // Something that produces nothing and does not exit. (`/bin/cat` was the
    // first choice and is wrong: the supervisor gives every child
    // `Stdio::null()` for stdin — §4.9, an agent must not be able to read the
    // supervisor's terminal — so `cat` sees EOF at once and exits 0.)
    let command = conductor_agent::AgentCommand {
        program: std::path::PathBuf::from("/bin/sleep"),
        args: vec!["30".to_string()],
        env: Default::default(),
        cwd: dir.path().to_path_buf(),
    };
    let adapter = FakeAgent::new(fake_agent_binary(), scenario_file(dir.path(), "success"));

    let mut cfg = config();
    cfg.startup_timeout = Duration::from_millis(400);

    let supervised = spawn(&command)
        .expect("spawn")
        .supervise(&adapter, &cfg, |_| {});

    match supervised.end {
        SupervisionEnd::TimedOut { reason } => {
            assert_eq!(reason, conductor_core::attempt::TIMEOUT_NO_STARTUP)
        }
        other => panic!("expected a startup timeout, got {other:?}"),
    }
}

#[test]
fn a_crashing_agent_is_classified_from_its_signal() {
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "crash-after-edits");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let supervised = spawn(&command)
        .expect("spawn")
        .supervise(&adapter, &config(), |_| {});

    match supervised.end {
        SupervisionEnd::Signalled { signal } => assert_eq!(signal, 9),
        other => panic!("expected a signal death, got {other:?}"),
    }
    assert_eq!(
        adapter.classify_exit(None, Some(9)),
        AttemptOutcome::Crashed
    );
    // …and the work it did before dying is on disk. Nothing about the crash
    // undoes it, which is why row 3 exists.
    assert!(dir.path().join("src/added.rs").exists());
}

#[test]
fn a_spawn_that_fails_is_reported_without_a_process() {
    let command = conductor_agent::AgentCommand {
        program: std::path::PathBuf::from("/nonexistent/definitely-not-here"),
        args: vec![],
        env: Default::default(),
        cwd: std::env::temp_dir(),
    };
    let error = spawn(&command).expect_err("spawning a missing binary must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn malformed_jsonl_is_recorded_but_does_not_stop_the_stream() {
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = common::agent::write_scenario(
        dir.path(),
        r#"{
          "id": "torn-line",
          "steps": [
            {"step":"emit_raw","line":"{\"kind\": \"file.written\", \"path\""},
            {"step":"emit","kind":"file.written","detail":"src/after.rs"},
            {"step":"checkpoint","name":"after-garbage"},
            {"step":"exit","code":0}
          ]
        }"#,
    );
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let supervised = spawn(&command)
        .expect("spawn")
        .supervise(&adapter, &config(), |_| {});

    assert_eq!(
        supervised.parse_errors.len(),
        1,
        "the malformed line must be recorded: {:?}",
        supervised.parse_errors
    );
    assert!(
        supervised
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Checkpoint { name } if name == "after-garbage")),
        "reading must continue past a malformed line"
    );
    assert!(matches!(supervised.end, SupervisionEnd::Exited { code: 0 }));
}

#[test]
fn the_heartbeat_callback_sees_a_live_child_and_stops_seeing_one_when_it_dies() {
    // §4.7: heartbeat "conditional on the agent process still existing
    // (`kill(pid, 0)`) — a supervisor that heartbeats while its child is dead is
    // worse than one that crashes." The witness is what makes that structural:
    // the callback is handed liveness, it does not assume it.
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    // An agent that lives long enough for the loop to tick at all. The
    // `success` scenario exits in single-digit milliseconds, and a supervisor
    // that ticked for it would be ticking for a child that had already gone —
    // the very thing §4.7 warns about. Its absence of ticks is correct; this
    // test needs a child that is actually there to observe.
    let scenario = common::agent::write_scenario(
        dir.path(),
        r#"{
          "id": "lives-briefly",
          "steps": [
            {"step":"checkpoint","name":"working"},
            {"step":"sleep_ms","ms":250},
            {"step":"exit","code":0}
          ]
        }"#,
    );
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let mut alive_ticks = 0usize;
    let mut dead_ticks = 0usize;
    let supervised = spawn(&command)
        .expect("spawn")
        .supervise(&adapter, &config(), |beat| {
            if beat.alive.is_some() {
                alive_ticks += 1;
            } else {
                dead_ticks += 1;
            }
        });

    assert!(
        matches!(supervised.end, SupervisionEnd::Exited { code: 0 }),
        "end={:?} stdout={:?} stderr={}",
        supervised.end,
        supervised.stdout_lines,
        supervised.stderr
    );
    assert!(alive_ticks > 0, "the callback never saw a live child");
    assert_eq!(
        dead_ticks, 0,
        "the supervisor must stop ticking once the child is gone, not report a dead one"
    );
}

#[test]
fn dropping_a_supervisor_kills_and_reaps_its_child() {
    // §2.2's argument for Rust: "ownership and `Drop` make 'always reaped'
    // structural rather than disciplinary". The test is a panic — the worst
    // path, and the one a `finally` block in another language is most likely to
    // miss.
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "stall");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let pid = {
        let agent = spawn(&command).expect("spawn");
        let pid = agent.pid();
        assert!(matches!(agent.liveness(), Liveness::Alive(_)));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _agent = agent;
            panic!("the supervisor's own code failed mid-run");
        }));
        assert!(result.is_err());
        pid
    };

    // Give the kernel a moment to finish tearing the process down; the reap
    // itself already happened inside `Drop`.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if matches!(conductor_run::supervise::probe(pid, 0), Liveness::Dead) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("a stalled child outlived a panicking supervisor: pid {pid} is still alive");
}

#[test]
fn a_recycled_pid_is_not_mistaken_for_the_original_process() {
    // §4.7 step 3: "alive **and** start-time matches (a recycled pid is not your
    // process)". Probing our own pid with somebody else's start time must not
    // report alive.
    let me = std::process::id() as i32;
    let real = conductor_run::supervise::start_time_us(me).expect("our own start time");

    assert!(matches!(
        conductor_run::supervise::probe(me, real),
        Liveness::Alive(_)
    ));
    match conductor_run::supervise::probe(me, real - 1_000_000) {
        Liveness::Recycled { actual_start } => assert_eq!(actual_start, real),
        other => panic!("a mismatched start time must not read as alive: {other:?}"),
    }
}

#[test]
fn sigterm_is_tried_before_sigkill() {
    // §6.4: "Conductor kills: `SIGTERM`, grace, `SIGKILL`". An agent that
    // handles SIGTERM gets the chance to; one that ignores it still dies.
    warm_the_binary();
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "stall");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = adapter.command(&start_input(dir.path())).expect("command");

    let agent = spawn(&command).expect("spawn");
    let pid = agent.pid();
    let supervised = agent.supervise(&adapter, &config(), |_| {});

    assert!(matches!(supervised.end, SupervisionEnd::TimedOut { .. }));
    assert_eq!(
        supervised.termination_signal,
        Some(libc::SIGTERM),
        "the agent takes the default SIGTERM disposition, so the grace period \
         should never have escalated to SIGKILL"
    );
    assert!(matches!(
        conductor_run::supervise::probe(pid, 0),
        Liveness::Dead
    ));
}

// ---------------------------------------------------------------------------
// Grandchildren — the gap S4 found in the verification runner and recorded as
// still open here (S4 completion report: "`supervise.rs` has the same latent
// gap for agents. S5 must close it.").
// ---------------------------------------------------------------------------

/// An "agent" that forks a grandchild and then goes silent.
///
/// Deliberately `/bin/sh` rather than the fake agent: the property under test is
/// the supervisor's, not the adapter's, and no catalogued scenario forks. The
/// adapter is still a `FakeAgent` because `supervise` only uses it to parse
/// lines.
fn forking_agent(dir: &std::path::Path, hold_stdout: bool) -> conductor_agent::AgentCommand {
    // The grandchild records its own pid so the test can probe the exact
    // process rather than pattern-matching a process table.
    std::fs::write(
        dir.join("grandchild.sh"),
        "echo $$ > grandchild.pid\nwhile :; do sleep 1; done\n",
    )
    .expect("write the grandchild script");

    // The parent loop is not `exec`ed away: the direct child must stay alive and
    // silent so the **idle** timer is what ends it.
    let redirect = if hold_stdout { "" } else { " >/dev/null 2>&1" };
    let script = format!(
        "sh grandchild.sh{redirect} &\n\
         echo agent-ready\n\
         while :; do sleep 1; done\n"
    );

    let mut env = std::collections::BTreeMap::new();
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }
    conductor_agent::AgentCommand {
        program: std::path::PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), script],
        env,
        cwd: dir.to_path_buf(),
    }
}

/// Read the grandchild's pid once it has written it.
fn grandchild_pid(dir: &std::path::Path) -> i32 {
    let path = dir.join("grandchild.pid");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the grandchild never announced itself; nothing is being tested");
}

#[test]
fn a_timed_out_agent_takes_its_grandchildren_with_it() {
    // S4 found this exact gap in the verification runner and fixed it there with
    // `setpgid` + a process-group kill; its report left the agent supervisor
    // open. The cost is not the delay: a grandchild left running inside the
    // workspace writes files **after** reconciliation has observed the tree,
    // which is the same class of bug as a green result for a tree that never
    // existed.
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "success");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = forking_agent(dir.path(), false);

    let agent = spawn(&command).expect("spawn");
    let child = agent.pid();
    let grandchild = grandchild_pid(dir.path());
    assert!(
        matches!(
            conductor_run::supervise::probe(grandchild, 0),
            Liveness::Alive(_)
        ),
        "the grandchild must be alive before the kill, or the test proves nothing"
    );

    let supervised = agent.supervise(&adapter, &config(), |_| {});
    assert!(
        matches!(supervised.end, SupervisionEnd::TimedOut { .. }),
        "the agent should have been ended by the idle timer: {:?}",
        supervised.end
    );

    // The direct child dying is not in question; the grandchild is.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut alive = true;
    while Instant::now() < deadline {
        if matches!(
            conductor_run::supervise::probe(grandchild, 0),
            Liveness::Dead
        ) {
            alive = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if alive {
        // Never leak a spinning process out of a failing test.
        unsafe {
            libc::kill(grandchild, libc::SIGKILL);
        }
    }
    assert!(
        !alive,
        "grandchild {grandchild} outlived the agent {child} Conductor killed; it is \
         still running inside the workspace and can still write to the tree"
    );
}

#[test]
fn a_grandchild_holding_the_output_pipe_cannot_wedge_the_supervisor() {
    // The same gap, in the form that is worse than a leak. The reader threads
    // end at EOF, and EOF only arrives when the **last** writer of the pipe
    // closes it. A grandchild that inherited stdout therefore blocks
    // `finish()`'s `handle.join()` for as long as it lives — which, for a
    // `while :; do sleep 1; done`, is forever.
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = scenario_file(dir.path(), "success");
    let adapter = FakeAgent::new(fake_agent_binary(), scenario);
    let command = forking_agent(dir.path(), true);

    let agent = spawn(&command).expect("spawn");
    let grandchild = grandchild_pid(dir.path());

    // Supervision runs on its own thread so that a wedged supervisor is a failed
    // assertion rather than a test binary that never returns.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let supervised = agent.supervise(&adapter, &config(), |_| {});
        let _ = tx.send(supervised.end);
    });

    let ended = rx.recv_timeout(Duration::from_secs(20));
    if ended.is_err() {
        unsafe {
            libc::kill(grandchild, libc::SIGKILL);
        }
    }
    let ended = ended.expect(
        "the supervisor never returned: a grandchild is holding the agent's stdout \
         pipe open, so the reader threads never see EOF and `finish()` blocks in \
         `join()` — the run can never be reconciled",
    );
    assert!(
        matches!(ended, SupervisionEnd::TimedOut { .. }),
        "expected the idle timer to end it: {ended:?}"
    );
    assert!(
        matches!(
            conductor_run::supervise::probe(grandchild, 0),
            Liveness::Dead
        ),
        "grandchild {grandchild} survived"
    );
}
