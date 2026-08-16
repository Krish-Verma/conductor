//! A foreground worker: claim one run, supervise one attempt, reconcile, route.
//!
//! §2.4: "**Foreground supervisor for S1–S13.** `kill -9` on a foreground
//! process is the cheapest crash test in existence, and S1–S9 are almost
//! entirely about surviving it." This binary is that foreground process. The
//! daemon is S14.
//!
//! `--die-at <point>` makes the worker send **itself** `SIGKILL` when the
//! sequence reaches a named [`RunPoint`]. Self-inflicted rather than delivered
//! by the test, because an external kill has to race a sleep to land in the
//! right place: this one lands between two statements, every time. There is no
//! unwinding, no `Drop`, no flush — which is exactly what a power failure looks
//! like from the database's point of view, and what makes the agent survive
//! (§6.1: an agent "survives the supervisor's own death").
//!
//! `--recover` runs §4.7's nine steps instead, which is what the crash matrix
//! does after each kill.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_agent::fake::FakeAgent;
use conductor_core::Fence;
use conductor_git::{Scope, SensitivePatterns};
use conductor_run::recovery::{RecoveryConfig, recover};
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
    let quarantine = PathBuf::from(required(&args, "--quarantine"));
    let worker_id = flag(&args, "--worker").unwrap_or_else(|| "worker-1".to_string());
    let recovering = args.iter().any(|a| a == "--recover");

    let mut store = Store::open_or_create(&store_path).expect("open store");

    let scope = Scope::new(
        flag(&args, "--scope")
            .unwrap_or_else(|| "src/**".to_string())
            .split(',')
            .map(str::to_string)
            .collect::<Vec<_>>(),
    );

    if recovering {
        let config = RecoveryConfig {
            worker_id,
            workspaces_root: workspaces,
            quarantine_root: quarantine,
            artifacts_root: artifacts,
            adopt_live_agents: args.iter().any(|a| a == "--adopt"),
            lease_ms: conductor_store::LEASE_MS,
            scope,
            sensitive: SensitivePatterns::default(),
        };
        let report = recover(&mut store, &config, now_ms()).expect("recover");
        println!(
            "{}",
            serde_json::to_string(&report).expect("serialize recovery report")
        );
        return;
    }

    let scenario = PathBuf::from(required(&args, "--scenario"));
    let fake_agent = PathBuf::from(required(&args, "--fake-agent"));
    let adapter = FakeAgent::new(fake_agent, scenario).with_max_lifetime_ms(
        flag(&args, "--agent-lifetime-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_000),
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
            idle_timeout: millis(&args, "--idle-timeout-ms", 3_000),
            wall_timeout: millis(&args, "--wall-timeout-ms", 20_000),
            terminate_grace: millis(&args, "--grace-ms", 500),
            poll_interval: Duration::from_millis(10),
        },
        lease_ms: flag(&args, "--lease-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(conductor_store::LEASE_MS),
        heartbeat_interval: millis(&args, "--heartbeat-ms", 200),
        scope,
        sensitive: SensitivePatterns::default(),
        // §4.9's "the adapter's own auth variable" clause. The row-28 scenario
        // uses it to learn the control socket's path, which is what M10
        // measured: "path was known in every case".
        agent_env_extra: match flag(&args, "--socket") {
            Some(path) => [("CONDUCTOR_SOCK".to_string(), path)].into_iter().collect(),
            None => Default::default(),
        },
        // The fake agent authenticates against nothing.
        credential_home: None,
        // S3's worker runs one attempt; §4.6's session policy is repair's.
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
