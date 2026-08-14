//! The fake agent: a real subprocess that behaves badly on request.
//!
//! Master plan §6.1: "Conductor drives agents as subprocesses, never in-process
//! SDK calls. A subprocess can be `SIGKILL`ed, inspected, resource-limited,
//! launched with a scrubbed environment, and — decisively — **survives the
//! supervisor's own death**." A fake that lived inside the test process could
//! not demonstrate any of those, so this one is a genuine binary that genuinely
//! dies.
//!
//! **Its first line is always `agent.ready`.** The supervisor starts the agent's
//! wall-clock and idle budgets at that line rather than at spawn, because M29
//! measured macOS taking 21.7 s to scan a freshly built binary before its first
//! instruction runs. Charging that to the agent would make a cold binary
//! indistinguishable from a stalled one.
//!
//! **Every run has a hard lifetime ceiling.** Tests deliberately kill the
//! supervisor and leave this process running, so nothing else can guarantee the
//! suite does not leak processes.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use conductor_agent::scenario::{Scenario, Step};

/// Exit code when the scenario file cannot be used. Distinct from any code a
/// scenario itself produces, so "the harness is broken" never looks like "the
/// agent failed".
const EXIT_BAD_SCENARIO: i32 = 90;
/// Exit code when a claimed path was already taken (duplicate attempt).
const EXIT_PATH_TAKEN: i32 = 91;
/// Exit code when the hard lifetime ceiling fired.
const EXIT_LIFETIME: i32 = 97;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario_path = flag(&args, "--scenario")
        .or_else(|| std::env::var(conductor_agent::fake::SCENARIO_ENV).ok())
        .map(PathBuf::from);
    let report_path = flag(&args, "--report")
        .or_else(|| std::env::var(conductor_agent::fake::REPORT_ENV).ok())
        .map(PathBuf::from);
    let max_lifetime_ms: u64 = flag(&args, "--max-lifetime-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(60_000);

    let Some(scenario_path) = scenario_path else {
        eprintln!("fake agent: no --scenario");
        std::process::exit(EXIT_BAD_SCENARIO);
    };

    arm_lifetime_watchdog(max_lifetime_ms);

    let raw = match std::fs::read_to_string(&scenario_path) {
        Ok(raw) => raw,
        Err(error) => {
            eprintln!(
                "fake agent: cannot read {}: {error}",
                scenario_path.display()
            );
            std::process::exit(EXIT_BAD_SCENARIO);
        }
    };
    let scenario: Scenario = match serde_json::from_str(&raw) {
        Ok(scenario) => scenario,
        Err(error) => {
            eprintln!(
                "fake agent: {} is not a scenario: {error}",
                scenario_path.display()
            );
            std::process::exit(EXIT_BAD_SCENARIO);
        }
    };

    // The readiness line, before any scenario step. Everything the supervisor
    // times is measured from here.
    emit(&format!(
        "{{\"kind\":\"agent.ready\",\"scenario\":{},\"pid\":{}}}",
        json_string(&scenario.id),
        std::process::id()
    ));

    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for step in &scenario.steps {
        run_step(step, &workspace, report_path.as_deref());
    }

    // A scenario that runs off the end exits cleanly; the catalogue's scenarios
    // all end explicitly, and this keeps a hand-written one from hanging.
    emit("{\"kind\":\"agent.finished\"}");
    std::process::exit(0);
}

fn run_step(step: &Step, workspace: &Path, report_path: Option<&Path>) {
    match step {
        Step::Emit { kind, detail } => emit(&format!(
            "{{\"kind\":{},\"detail\":{},\"path\":{}}}",
            json_string(kind),
            json_string(detail),
            json_string(detail)
        )),
        Step::EmitRaw { line } => emit(line),
        Step::Checkpoint { name } => emit(&format!(
            "{{\"kind\":\"checkpoint\",\"name\":{}}}",
            json_string(name)
        )),
        Step::WriteFile { path, contents } => {
            let target = workspace.join(path);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(error) = std::fs::write(&target, contents) {
                emit(&format!(
                    "{{\"kind\":\"agent.error\",\"message\":{}}}",
                    json_string(&format!("write {}: {error}", target.display()))
                ));
            }
        }
        Step::DeleteFile { path } => {
            let _ = std::fs::remove_file(workspace.join(path));
        }
        Step::Git { args } => {
            let output = Command::new("git")
                .args(args)
                .current_dir(workspace)
                .output();
            let ok = output.map(|o| o.status.success()).unwrap_or(false);
            emit(&format!(
                "{{\"kind\":\"command.run\",\"command\":{},\"ok\":{ok}}}",
                json_string(&format!("git {}", args.join(" ")))
            ));
        }
        Step::WriteReport {
            claim,
            files_touched,
            summary,
        } => {
            if let Some(path) = report_path {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, report_json(claim, files_touched, summary));
            }
        }
        Step::WriteRawReport { contents } => {
            if let Some(path) = report_path {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, contents);
            }
        }
        Step::ReportOnStdout {
            claim,
            files_touched,
            summary,
        } => emit(&format!(
            "{{\"kind\":\"agent.report\",\"report\":{}}}",
            report_json(claim, files_touched, summary)
        )),
        Step::ConnectUnix { path_env } => {
            let path = std::env::var(path_env).unwrap_or_default();
            let connected = if path.is_empty() {
                false
            } else {
                match UnixStream::connect(&path) {
                    Ok(mut stream) => {
                        let _ = stream.write_all(b"grant approval please\n");
                        true
                    }
                    Err(_) => false,
                }
            };
            emit(&format!(
                "{{\"kind\":\"control_socket.attempt\",\"path\":{},\"connected\":{connected}}}",
                json_string(&path)
            ));
        }
        Step::ClaimPath { path_env } => {
            let path = std::env::var(path_env).unwrap_or_default();
            if path.is_empty() {
                return;
            }
            // `create_new` is the whole mechanism: the second process to reach
            // here fails rather than overwriting the first one's file.
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let owner = std::env::var(conductor_agent::fake::ATTEMPT_ENV)
                        .unwrap_or_else(|_| "unknown".to_string());
                    let _ = writeln!(file, "{}|{}", owner, std::process::id());
                    emit(&format!(
                        "{{\"kind\":\"path.claimed\",\"path\":{}}}",
                        json_string(&path)
                    ));
                }
                Err(_) => {
                    emit(&format!(
                        "{{\"kind\":\"agent.error\",\"message\":{}}}",
                        json_string(&format!("{path} is already claimed"))
                    ));
                    std::process::exit(EXIT_PATH_TAKEN);
                }
            }
        }
        Step::SleepMs { ms } => std::thread::sleep(Duration::from_millis(*ms)),
        Step::Stall => {
            // Silence, until something kills us or the watchdog fires. No
            // output at all: an agent that keeps talking is not stalling.
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Step::Spin { interval_ms } => loop {
            emit("{\"kind\":\"agent.progress\"}");
            std::thread::sleep(Duration::from_millis(*interval_ms));
        },
        Step::Exit { code } => std::process::exit(*code),
        Step::Abort => std::process::abort(),
        Step::KillSelf { signal } => {
            // A signal to ourselves at an exact point in the script. Stronger
            // than an external kill racing a sleep: the death happens after the
            // preceding step and before the next one, every time.
            unsafe {
                libc::kill(std::process::id() as i32, *signal);
            }
            // Only reachable if the signal was ignorable.
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// A thread that ends this process no matter what the scenario is doing.
///
/// `_exit` rather than `exit`: no destructors, no flushing, no chance of a
/// scenario's own state interfering with the ceiling.
fn arm_lifetime_watchdog(max_lifetime_ms: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(max_lifetime_ms));
        unsafe { libc::_exit(EXIT_LIFETIME) }
    });
}

fn emit(line: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{line}");
    // Flushed on every line. A checkpoint that sat in a buffer would let a test
    // kill the process before the line the test is waiting for ever arrives,
    // which is the deadlock every "just add a sleep" harness eventually grows.
    let _ = lock.flush();
}

fn report_json(claim: &str, files_touched: &[String], summary: &str) -> String {
    let files: Vec<String> = files_touched.iter().map(|f| json_string(f)).collect();
    format!(
        "{{\"claim\":{},\"files_touched\":[{}],\"summary\":{}}}",
        json_string(claim),
        files.join(","),
        json_string(summary)
    )
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
