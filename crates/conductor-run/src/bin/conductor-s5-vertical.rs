//! The S5 vertical in its **own process**, so that Conductor can be killed
//! during integration and the assertions can survive to be checked.
//!
//! `--die-at <point>` makes it send **itself** `SIGKILL` when integration
//! reaches a named [`IntegrationPoint`]. Self-inflicted rather than delivered
//! from outside, for the reason S3's worker gives: an external kill has to race
//! a sleep to land between two particular statements, and this one lands there
//! every time. `SIGKILL` because it cannot be caught — no unwinding, no `Drop`,
//! no flush, which is what the database sees during a power failure.
//!
//! `--resume` runs §4.7's restart instead: sweep the lapsed lease, take the run
//! back, resolve the `INTENDED` ledger row by re-checking the precondition
//! against the world, and carry on to completion.
//!
//! Not a product binary. `conductor task run` is the product; this exists so the
//! failure modes are injected into something that can actually die.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_agent::fake::FakeAgent;
use conductor_run::effects::{IntegrationObserver, IntegrationPoint};
use conductor_run::supervise::SupervisorConfig;
use conductor_run::vertical::{VerticalConfig, VerticalOutcome, resume_task, run_task};
use conductor_run::worker::{RunObserver, RunPoint};
use conductor_store::Store;

/// The vertical stopped short of `COMPLETE`.
const EXIT_STOPPED: i32 = 3;
/// The vertical could not run at all.
const EXIT_ERROR: i32 = 4;

/// Kills this process when integration reaches `point`.
struct KillAt {
    point: Option<IntegrationPoint>,
}

impl RunObserver for KillAt {
    fn at(&mut self, point: RunPoint) {
        announce("run", point.as_str());
    }
}

impl IntegrationObserver for KillAt {
    fn at(&mut self, point: IntegrationPoint) {
        // Announce **and flush** before dying: the test reads this to know how
        // far the process actually got, and a line still in a buffer when
        // `SIGKILL` lands is a line that never existed.
        announce("integration", point.as_str());
        if self.point == Some(point) {
            unsafe {
                libc::kill(std::process::id() as i32, libc::SIGKILL);
            }
            // Unreachable: SIGKILL cannot be caught, blocked or ignored.
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

fn announce(stage: &str, point: &str) {
    use std::io::Write;
    println!("{{\"stage\":\"{stage}\",\"at\":\"{point}\"}}");
    let _ = std::io::stdout().flush();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(required(&args, "--root"));
    let worker_id = flag(&args, "--worker").unwrap_or_else(|| "worker-1".to_string());
    let resuming = args.iter().any(|a| a == "--resume");

    let mut store = Store::open_or_create(root.join("conductor.db")).expect("open store");

    let config = VerticalConfig {
        task_id: conductor_core::TaskId::new(
            flag(&args, "--task").unwrap_or_else(|| "T-0012".to_string()),
        )
        .expect("task id"),
        worker_id,
        source_repo: root.join("source"),
        workspaces_root: root.join("workspaces"),
        artifacts_root: root.join("artifacts"),
        quarantine_root: root.join("quarantine"),
        profile_path: root.join("verification.yaml"),
        scratch_index: root.join("scratch").join("index"),
        supervisor: SupervisorConfig {
            // Generous on purpose: what this binary injects is a kill of
            // **Conductor** at a named integration point, and an agent the
            // supervisor timed out first would change which point is reached.
            startup_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(60),
            wall_timeout: Duration::from_secs(120),
            terminate_grace: Duration::from_millis(300),
            poll_interval: Duration::from_millis(10),
        },
        lease_ms: conductor_store::LEASE_MS,
        heartbeat_interval: Duration::from_millis(200),
        startup_grace: Duration::from_secs(30),
        sensitive: conductor_git::SensitivePatterns::default(),
        agent_env_extra: Default::default(),
        // The fake agent authenticates against nothing.
        credential_home: None,
        // This harness kills Conductor at named integration points; the tasks
        // it drives declare no `execution_requirements`, so §4.2's gate
        // compares an empty vector and proceeds without consulting the cache.
        // The key is still named honestly rather than left blank — if a future
        // fixture *does* declare a requirement, this misses the cache and the
        // launch is refused, which is the safe direction.
        probe_key: conductor_run::containment::cache::ProbeKey::new(
            "fake",
            "s5-vertical",
            "none",
            "n/a",
            "unprobed",
        ),
    };

    let mut observer = KillAt {
        point: flag(&args, "--die-at").and_then(|name| IntegrationPoint::parse(&name)),
    };

    if resuming {
        // A time past the lease the killed process was holding. Not a sleep: the
        // predicate is `expires_at < now` and `now` is an argument precisely so a
        // restart need not wait sixty seconds for a lease it can see is dead.
        let now = now_ms() + conductor_store::LEASE_MS * 2;
        match resume_task(&mut store, &config, now, &mut observer) {
            Ok(resumed) => report(&resumed.outcome),
            Err(error) => {
                eprintln!("resume error: {error}");
                std::process::exit(EXIT_ERROR);
            }
        }
        return;
    }

    let scenario = PathBuf::from(required(&args, "--scenario"));
    let fake_agent = PathBuf::from(required(&args, "--fake-agent"));
    let adapter = FakeAgent::new(fake_agent, scenario).with_max_lifetime_ms(20_000);

    match run_task(&mut store, &adapter, &config, &mut observer) {
        Ok(result) => report(&result.outcome),
        Err(error) => {
            eprintln!("vertical error: {error}");
            std::process::exit(EXIT_ERROR);
        }
    }
}

fn report(outcome: &VerticalOutcome) {
    match outcome {
        VerticalOutcome::Complete {
            commit, fetched, ..
        } => {
            println!(
                "{{\"outcome\":\"COMPLETE\",\"commit\":{:?},\"ref\":{:?},\"sha\":{:?}}}",
                commit.sha, fetched.reference, fetched.sha
            );
        }
        VerticalOutcome::Stopped { state, reason } => {
            println!(
                "{{\"outcome\":\"STOPPED\",\"state\":\"{state}\",\"reason\":{}}}",
                serde_json::to_string(reason).unwrap_or_else(|_| "\"\"".to_string())
            );
            std::process::exit(EXIT_STOPPED);
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
