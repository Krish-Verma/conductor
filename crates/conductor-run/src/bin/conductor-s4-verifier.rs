//! A verification runner in its **own process** — the S4 failure-injection payload.
//!
//! Two of D8's injections cannot be staged inside the test binary.
//!
//! **Kill mid-check.** The point of "kill Conductor while a check is running"
//! is that Conductor dies and the check does not; a runner living inside the
//! test process would take the assertions down with it. Same reason S3 needed
//! `conductor-s3-worker`.
//!
//! **A filesystem that refuses the write.** `RLIMIT_FSIZE` is per-process, so
//! setting it inside a test binary would apply to every test sharing that
//! process — `cargo` runs tests as threads. Here it is confined to one child.
//!
//! Not a product binary. It exists so that the failure modes are injected into
//! something that can actually die.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use conductor_core::RunId;
use conductor_run::paths::{ArtifactRoot, Owner};
use conductor_run::verify::profile;
use conductor_run::verify::runner::{RunnerConfig, run_profile};
use conductor_store::Store;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut opts: BTreeMap<String, String> = BTreeMap::new();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .unwrap_or_else(|| fail(&format!("{flag} needs a value")));
        opts.insert(flag.trim_start_matches("--").to_string(), value);
    }

    let get = |name: &str| -> String {
        opts.get(name)
            .cloned()
            .unwrap_or_else(|| fail(&format!("--{name} is required")))
    };

    if let Some(limit) = opts.get("fsize-limit") {
        let bytes: u64 = limit.parse().unwrap_or_else(|_| fail("--fsize-limit"));
        set_file_size_limit(bytes);
    }

    let db = PathBuf::from(get("db"));
    let run_id = RunId::new(get("run")).unwrap_or_else(|e| fail(&e.to_string()));
    let worker = get("worker");
    let now: i64 = get("now").parse().unwrap_or_else(|_| fail("--now"));

    let mut store = Store::open_or_create(&db).unwrap_or_else(|e| fail(&e.to_string()));
    // A restarted process takes the run the way §4.7 says: sweep the lapsed
    // lease, then claim.
    store
        .expire_leases(now)
        .unwrap_or_else(|e| fail(&e.to_string()));
    let claimed = store
        .claim_run(&run_id, &worker, now, 60_000)
        .unwrap_or_else(|e| fail(&e.to_string()))
        .unwrap_or_else(|| fail("the run was not claimable"));
    let fence = claimed.fence();

    let mut env = BTreeMap::new();
    if let Ok(path) = std::env::var("PATH") {
        env.insert("PATH".to_string(), path);
    }

    let config = RunnerConfig {
        workspace: PathBuf::from(get("workspace")),
        scratch_index: PathBuf::from(get("scratch-index")),
        artifacts: ArtifactRoot::new(get("artifacts")),
        run_id,
        attempt_ordinal: 1,
        attempt_id: None,
        owner: Owner::new(worker, std::process::id() as i32),
        env,
        commit_sha: opts
            .get("commit")
            .cloned()
            .unwrap_or_else(|| "abc123".to_string()),
        changed_paths: Vec::new(),
        startup_grace: Duration::from_secs(30),
    };

    let text = std::fs::read_to_string(get("profile")).unwrap_or_else(|e| fail(&e.to_string()));
    let loaded = profile::parse(&text).unwrap_or_else(|e| fail(&e.to_string()));

    // Announce readiness before anything long starts, so a test never has to
    // guess whether the process got as far as running.
    println!("verifier.ready");

    let report = run_profile(&mut store, &fence, &config, &loaded, now)
        .unwrap_or_else(|e| fail(&e.to_string()));

    let json = serde_json::to_string(&report).unwrap_or_else(|e| fail(&e.to_string()));
    if let Some(out) = opts.get("out") {
        std::fs::write(out, &json).unwrap_or_else(|e| fail(&e.to_string()));
    }
    println!("{json}");
}

/// Make the kernel refuse writes past `bytes` — a genuine `EFBIG`, in the same
/// family as `ENOSPC`: the open succeeds, and the write fails part-way through.
///
/// `SIGXFSZ` is ignored first. Its default action is to terminate the process,
/// which would turn "the log could not be written" into "the verifier died" and
/// test the wrong thing entirely.
fn set_file_size_limit(bytes: u64) {
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        let limit = libc::rlimit {
            rlim_cur: bytes,
            rlim_max: bytes,
        };
        if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
            fail("setrlimit(RLIMIT_FSIZE) failed");
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("conductor-s4-verifier: {message}");
    std::process::exit(2);
}
