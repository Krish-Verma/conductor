//! A process that is Codex on the wire and a fixture underneath — S10's crash
//! matrix without the money.
//!
//! S10's risk note is explicit: *"Keep the fake agent as the primary CI harness
//! **forever**; real-agent tests are a separate, non-blocking suite."* The Verify
//! line is equally explicit that *"the entire S3 crash matrix passes with Codex
//! substituted for the fake agent"*. Both are satisfiable at once only if
//! something can play Codex's part deterministically, thirteen times, on every
//! developer's machine.
//!
//! This binary is that something, and it is **not** a second fake agent. The
//! fake agent speaks Conductor's own scenario language; this speaks
//! **`codex exec`'s**. It is launched by [`conductor_agent::codex::CodexAgent`]
//! through the real [`the real attempt path`], with the real
//! argv that adapter builds, and everything it writes to stdout is bytes
//! recorded from codex-cli 0.142.0 in
//! `crates/conductor-agent/tests/fixtures/codex-jsonl/`. Every layer between the
//! process boundary and the database is the production one. Only the model is
//! absent, and the model is the one component a crash matrix is not about.
//!
//! # It is also a positive control on the adapter's argv
//!
//! §6.2 chose Codex for exactly one reason — `--sandbox workspace-write` is the
//! only measured mechanism on a host with no container runtime that denies
//! writes outside the workspace (M6), network (M9) and the control socket (M10).
//! An adapter that quietly stopped passing that flag would still pass every
//! parsing test in `conductor-agent/tests/codex.rs`, because those tests never
//! spawn anything.
//!
//! So this refuses to run at all unless the argv it was handed carries
//! `--sandbox workspace-write`, `--ignore-user-config`, `--ignore-rules`,
//! `--json`, `--cd`, `--output-schema` and `--output-last-message`, and unless
//! the `--output-schema` file **exists and parses** — which is the only place in
//! the system that checks §6.1's "the adapter never writes this file; the caller
//! does". A missing flag exits `97`, which no fixture ever produces.
//!
//! # Control channel
//!
//! Through the environment, delivered by `WorkerConfig::agent_env_extra` —
//! §4.9's "the adapter's own auth variable" clause, which is the only door into
//! the child's environment and is deliberately by-name:
//!
//! | Variable | Effect |
//! |---|---|
//! | `CONDUCTOR_REPLAY_FIXTURE` | the JSONL to replay (required) |
//! | `CONDUCTOR_REPLAY_REPORT` | a file whose contents become `--output-last-message`'s; unset means the agent wrote no report |
//! | `CONDUCTOR_REPLAY_APPLY` | `1` to actually perform the fixture's `file_change` edits in the workspace |
//! | `CONDUCTOR_REPLAY_KILL_AFTER` | emit N lines, then `SIGKILL` **itself** — S3's agent-kill axis |
//! | `CONDUCTOR_REPLAY_STALL_MS` | go silent for this long before finishing — S3's stall axis |
//! | `CONDUCTOR_REPLAY_FINAL` | `kill` to `SIGKILL` itself at the very end instead of exiting |
//! | `CONDUCTOR_REPLAY_GIT` | a `git` command (space-separated) to run in the workspace first — S3's agent-kill point 7, "after mutating repository structure" |
//! | `CONDUCTOR_REPLAY_EXIT` | the exit code (default `0`) |
//!
//! `SIGKILL` on itself rather than a kill delivered from the test, for S3's
//! reason: an external kill has to race a sleep to land between two particular
//! lines, and this one lands there every time.
//!
//! # `/workspace` is rewritten to the real workspace, on purpose
//!
//! The recordings were made in a workspace called `/workspace`, and §6.2's
//! second measured finding is that `files_touched` comes back **absolute**. If
//! this binary emitted the literal recorded path, the normalisation the adapter
//! performs would be tested against a path that is outside every real workspace
//! — which is the one case §6.2 says to leave alone. Rewriting `/workspace/` to
//! the actual `--cd` makes the recorded report describe the tree that actually
//! exists, so reconciliation compares like with like and the "false
//! `CONTRADICTED` on every happy path" bug is reachable by a test rather than
//! only by a real run. `/workspace-other/` does not match the rewrite and stays
//! outside, which is what keeps the escaped-path fixture meaningful.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The argv this binary was handed is not one the Codex adapter builds.
const EXIT_BAD_ARGV: i32 = 97;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cd = check_argv(&args);

    let fixture = PathBuf::from(env_required("CONDUCTOR_REPLAY_FIXTURE"));
    let raw = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|e| die(&format!("reading {}: {e}", fixture.display())));
    let text = rewrite(&raw, &cd);

    let apply = std::env::var("CONDUCTOR_REPLAY_APPLY").as_deref() == Ok("1");
    let kill_after: Option<usize> = std::env::var("CONDUCTOR_REPLAY_KILL_AFTER")
        .ok()
        .and_then(|v| v.parse().ok());

    // S3's agent-kill point 7 is "after mutating repository structure", and the
    // mutation has to be real — §4.8 reads it out of git, never out of the
    // stream, so a fixture that merely *said* `git remote add` would prove
    // nothing about the audit.
    if let Ok(command) = std::env::var("CONDUCTOR_REPLAY_GIT") {
        let _ = std::process::Command::new("git")
            .args(command.split_whitespace())
            .current_dir(&cd)
            .output();
    }

    // A fixture that ends without a newline ends without one here too: a
    // half-written final line is exactly what a killed agent leaves behind, and
    // `truncated.jsonl` is a recording of that.
    let trailing_newline = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();

    let stdout = std::io::stdout();
    for (index, line) in lines.iter().enumerate() {
        if apply {
            apply_changes(line, &cd);
        }
        let mut handle = stdout.lock();
        if index + 1 == lines.len() && !trailing_newline {
            let _ = write!(handle, "{line}");
        } else {
            let _ = writeln!(handle, "{line}");
        }
        // Flushed every line: a line still in a buffer when `SIGKILL` lands is a
        // line the supervisor never saw, and the whole point of the kill points
        // is to control exactly how much it saw.
        let _ = handle.flush();
        drop(handle);

        if kill_after == Some(index + 1) {
            unsafe {
                libc::kill(std::process::id() as i32, libc::SIGKILL);
            }
            // Unreachable: SIGKILL cannot be caught, blocked or ignored.
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    }

    if let Some(ms) = std::env::var("CONDUCTOR_REPLAY_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    if let Ok(source) = std::env::var("CONDUCTOR_REPLAY_REPORT") {
        let body = std::fs::read_to_string(&source)
            .unwrap_or_else(|e| die(&format!("reading report {source}: {e}")));
        let target = flag(&args, "--output-last-message").expect("checked by check_argv");
        std::fs::write(&target, rewrite(&body, &cd))
            .unwrap_or_else(|e| die(&format!("writing {target}: {e}")));
    }

    // "After writing the report, before exiting" and "silent, then killed" are
    // two of S3's eight agent-kill points, and both are *this* moment.
    if std::env::var("CONDUCTOR_REPLAY_FINAL").as_deref() == Ok("kill") {
        unsafe {
            libc::kill(std::process::id() as i32, libc::SIGKILL);
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    std::process::exit(
        std::env::var("CONDUCTOR_REPLAY_EXIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    );
}

/// Refuse any argv the Codex adapter would not have produced.
///
/// Returns the workspace `--cd` names, which every rewrite is relative to.
fn check_argv(args: &[String]) -> String {
    if args.first().map(String::as_str) != Some("exec") {
        refuse("the first argument must be `exec`");
    }
    for flag_name in ["--json", "--ignore-user-config", "--ignore-rules"] {
        if !args.iter().any(|a| a == flag_name) {
            refuse(&format!("{flag_name} is missing"));
        }
    }
    // The reason Codex is the first adapter at all (§6.2, M6/M9/M10). An adapter
    // that stops passing this is an uncontained run wearing a contained run's
    // record, and no parsing test would notice.
    if flag(args, "--sandbox").as_deref() != Some("workspace-write") {
        refuse("--sandbox must be workspace-write");
    }
    if args
        .iter()
        .any(|a| a == "--dangerously-bypass-approvals-and-sandbox")
        || flag(args, "--sandbox").as_deref() == Some("danger-full-access")
    {
        refuse("containment was bypassed");
    }

    let schema = flag(args, "--output-schema").unwrap_or_else(|| {
        refuse("--output-schema is missing");
    });
    // §6.1 keeps I/O out of adapters, so the *caller* writes this file. Nothing
    // else in the system checks that it did.
    let body = std::fs::read_to_string(&schema)
        .unwrap_or_else(|e| refuse(&format!("--output-schema {schema} is unreadable: {e}")));
    if serde_json::from_str::<serde_json::Value>(&body).is_err() {
        refuse(&format!("--output-schema {schema} is not JSON"));
    }

    if flag(args, "--output-last-message").is_none() {
        refuse("--output-last-message is missing");
    }
    let cd = flag(args, "--cd").unwrap_or_else(|| refuse("--cd is missing"));

    // Codex blocks forever reading stdin when the prompt is not in argv, and
    // `supervise::spawn` hands the child a null stdin that will never satisfy
    // it. The adapter refuses an empty prompt; this checks the refusal held.
    match args.last() {
        Some(prompt) if !prompt.trim().is_empty() && !prompt.starts_with("--") => {}
        _ => refuse("the last argument must be a non-empty prompt"),
    }
    cd
}

/// Perform the edits a `file_change` item announces.
///
/// The *content* is this harness's invention — the recordings carry paths and
/// kinds, not diffs. That is the right amount of fidelity for what is under
/// test: §4.8 reconciles the report against `git status`, so what matters is
/// that the named paths really changed, and a real Codex writing real code is
/// what the `#[ignore]`d tests are for.
fn apply_changes(line: &str, cd: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    if value.get("type").and_then(|v| v.as_str()) != Some("item.started") {
        return;
    }
    let Some(item) = value.get("item") else {
        return;
    };
    if item.get("type").and_then(|v| v.as_str()) != Some("file_change") {
        return;
    }
    let Some(changes) = item.get("changes").and_then(|v| v.as_array()) else {
        return;
    };
    for change in changes {
        let Some(path) = change.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        // Never write outside the workspace, whatever a fixture says. The
        // escaped-path fixtures exist to be *reported*, not performed.
        let path = Path::new(path);
        if !path.starts_with(cd) {
            continue;
        }
        match change.get("kind").and_then(|v| v.as_str()) {
            Some("delete") => {
                let _ = std::fs::remove_file(path);
            }
            Some("add") => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, "pub fn added() -> u32 { 1 }\n");
            }
            _ => {
                let mut body = std::fs::read_to_string(path).unwrap_or_default();
                body.push_str("pub fn double(value: u32) -> u32 { value * 2 }\n");
                let _ = std::fs::write(path, body);
            }
        }
    }
}

/// Recorded `/workspace/...` becomes this run's workspace. See the module note.
fn rewrite(text: &str, cd: &str) -> String {
    text.replace("/workspace/", &format!("{}/", cd.trim_end_matches('/')))
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn env_required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| die(&format!("{name} is required")))
}

fn refuse(why: &str) -> ! {
    eprintln!("conductor-s10-codex-replay: not an argv the Codex adapter builds: {why}");
    std::process::exit(EXIT_BAD_ARGV);
}

fn die(why: &str) -> ! {
    eprintln!("conductor-s10-codex-replay: {why}");
    std::process::exit(96);
}
