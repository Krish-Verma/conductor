//! S3's worker with S10's adapter: one attempt driven by
//! [`conductor_agent::codex::CodexAgent`], killable at every [`RunPoint`].
//!
//! S10's Verify line is *"the entire S3 crash matrix passes with Codex
//! substituted for the fake agent"*, and S3's matrix kills **Conductor** at
//! thirteen named points. A kill that lands between two statements cannot be
//! delivered from outside, so — exactly as `conductor-s3-worker` does — this
//! process sends *itself* `SIGKILL` when the sequence reaches `--die-at`.
//!
//! This is a copy of `conductor-s3-worker` with one line changed: the adapter.
//! That is the point. If the crash matrix needed anything else to differ, S10
//! says so itself — *"If any scenario needs adapter-specific handling, that is a
//! design smell to fix in the interface, not the adapter"* — and the difference
//! would be visible here as a second `if`. There is none.
//!
//! # Where the `--output-schema` file goes, and why not where S10 says
//!
//! §6.1 keeps I/O out of adapters, so the caller writes
//! [`REPORT_SCHEMA_JSON`](conductor_agent::codex::REPORT_SCHEMA_JSON) and passes
//! the path to [`CodexAgent::new`]. The natural home is the attempt's artifact
//! directory — but `CodexAgent::new` needs the path *before*
//! [`run_one_attempt`] has computed the attempt ordinal, and the ordinal is what
//! names that directory. The schema is therefore written to the **run's**
//! artifact tree (`<artifacts>/<run>/agent/report-schema.json`), which is stable
//! across attempts and is the same content every attempt would receive anyway.
//! Recorded rather than worked around: a schema that differed per attempt would
//! need the adapter to be constructed inside the worker.
//!
//! Not a product binary. `conductor task run` is the product; this exists so the
//! failure modes are injected into something that can actually die.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_agent::codex::{CodexAgent, REPORT_SCHEMA_JSON};
use conductor_core::Fence;
use conductor_git::{Scope, SensitivePatterns};
use conductor_run::supervise::SupervisorConfig;
use conductor_run::worker::{RunObserver, RunPoint, WorkerConfig, run_one_attempt};
use conductor_store::Store;

/// Exit code when the worker found nothing to claim.
const EXIT_NOTHING_TO_CLAIM: i32 = 3;
/// Exit code when the attempt itself failed in a way the worker cannot handle.
const EXIT_WORKER_ERROR: i32 = 4;

/// The observer that ends this process at a named point.
struct KillAt {
    point: Option<RunPoint>,
}

impl RunObserver for KillAt {
    fn at(&mut self, point: RunPoint) {
        // Announce first, and flush: the test reads this to know how far the
        // worker actually got, and a line still sitting in a buffer when the
        // process dies is a line that never existed.
        println!("{{\"at\":\"{}\"}}", point.as_str());
        use std::io::Write;
        let _ = std::io::stdout().flush();

        if self.point == Some(point) {
            unsafe {
                libc::kill(std::process::id() as i32, libc::SIGKILL);
            }
            // Unreachable: SIGKILL cannot be caught, blocked or ignored.
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let store_path = PathBuf::from(required(&args, "--store"));
    let source = PathBuf::from(required(&args, "--source"));
    let workspaces = PathBuf::from(required(&args, "--workspaces"));
    let artifacts = PathBuf::from(required(&args, "--artifacts"));
    let worker_id = flag(&args, "--worker").unwrap_or_else(|| "worker-1".to_string());
    let run_id = flag(&args, "--run").unwrap_or_else(|| "r-0041".to_string());

    let mut store = Store::open_or_create(&store_path).expect("open store");

    let scope = Scope::new(
        flag(&args, "--scope")
            .unwrap_or_else(|| "src/**".to_string())
            .split(',')
            .map(str::to_string)
            .collect::<Vec<_>>(),
    );

    // The workspace path `run_one_attempt` will derive. The adapter needs it up
    // front because it normalises reported paths against it (§6.2's second
    // measured finding), and `CodexAgent::command` refuses a `StartInput` whose
    // workspace disagrees — so a mistake here is a refusal, not a wrong answer.
    let workspace = workspaces.join(&run_id);

    let schema_path = artifacts
        .join(&run_id)
        .join("agent")
        .join("report-schema.json");
    std::fs::create_dir_all(schema_path.parent().expect("a parent")).expect("artifact dir");
    std::fs::write(&schema_path, REPORT_SCHEMA_JSON).expect("write the report schema");

    let adapter = CodexAgent::new(
        PathBuf::from(required(&args, "--codex")),
        workspace,
        schema_path,
    )
    .with_prompt(
        flag(&args, "--prompt")
            .unwrap_or_else(|| "Add a function named double to lib.rs.".to_string()),
    );

    let config = WorkerConfig {
        worker_id: worker_id.clone(),
        workspaces_root: workspaces,
        artifacts_root: artifacts,
        source_repo: source,
        supervisor: SupervisorConfig {
            // Generous, for M29: this budget covers the operating system's
            // first-execution binary scan, not the agent's work.
            startup_timeout: millis(&args, "--startup-timeout-ms", 60_000),
            idle_timeout: millis(&args, "--idle-timeout-ms", 60_000),
            wall_timeout: millis(&args, "--wall-timeout-ms", 120_000),
            terminate_grace: millis(&args, "--grace-ms", 500),
            poll_interval: Duration::from_millis(10),
        },
        lease_ms: flag(&args, "--lease-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(conductor_store::LEASE_MS),
        heartbeat_interval: millis(&args, "--heartbeat-ms", 200),
        scope,
        sensitive: SensitivePatterns::default(),
        // §4.9's "the adapter's own auth variable" clause, by name and nothing
        // else. For Codex this carries `CODEX_HOME`; for the replay harness it
        // carries the fixture to replay.
        agent_env_extra: agent_env(&args),
        // One attempt; §4.6's session policy is repair's.
        agent_session_id: None,
    };

    let Some(claimed) = store
        .claim_next_run(&worker_id, now_ms(), config.lease_ms)
        .expect("claim")
    else {
        eprintln!("nothing to claim");
        std::process::exit(EXIT_NOTHING_TO_CLAIM);
    };
    let fence: Fence = claimed.fence();
    println!(
        "{{\"claimed\":\"{}\",\"state\":\"{}\",\"epoch\":{}}}",
        claimed.run_id, claimed.state, claimed.lease_epoch
    );

    let mut observer = KillAt {
        point: flag(&args, "--die-at").and_then(|name| RunPoint::parse(&name)),
    };

    match run_one_attempt(&mut store, &fence, &adapter, &config, &mut observer) {
        Ok(outcome) => {
            println!(
                "{{\"attempt\":\"{}\",\"attempt_state\":\"{}\",\"verdict\":\"{}\",\
                  \"route\":\"{}\",\"findings\":{}}}",
                outcome.attempt_id,
                outcome.attempt_state,
                outcome.verdict,
                outcome.route.state(),
                outcome.findings.len()
            );
        }
        Err(error) => {
            eprintln!("worker error: {error}");
            std::process::exit(EXIT_WORKER_ERROR);
        }
    }
}

/// Every `--agent-env KEY=VALUE`, in order.
fn agent_env(args: &[String]) -> BTreeMap<String, String> {
    let mut extra = BTreeMap::new();
    for (index, arg) in args.iter().enumerate() {
        if arg != "--agent-env" {
            continue;
        }
        let Some(pair) = args.get(index + 1) else {
            continue;
        };
        if let Some((key, value)) = pair.split_once('=') {
            extra.insert(key.to_string(), value.to_string());
        }
    }
    extra
}

fn required(args: &[String], name: &str) -> String {
    flag(args, name).unwrap_or_else(|| panic!("{name} is required"))
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn millis(args: &[String], name: &str, default: u64) -> Duration {
    Duration::from_millis(
        flag(args, name)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
