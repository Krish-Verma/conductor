//! S4's failure injection — the master plan's "kill mid-check · fill the disk ·
//! remove the toolchain between runs".
//!
//! Each must reach a **defined** outcome, and the rule that binds them is D8's:
//! **infrastructure failure is `INCONCLUSIVE`, never `FAIL`.** §4.5 is explicit
//! about why — `FAIL` routes to repair and spends agent attempts, so classifying
//! a full disk as a failing test is how "a broken cache turns into three wasted
//! agent attempts".
//!
//! Removing the toolchain is injected in `verification.rs`, where the cache
//! tests already have the fixture for it.

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use conductor_core::RunId;
use conductor_store::Store;

const RUN: &str = "r-0041";
const NOW: i64 = 1_770_000_000_000;

/// The verifier binary from **this** build.
fn verifier_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let target = exe
        .parent()
        .and_then(Path::parent)
        .expect("target directory");
    let path = target.join("conductor-s4-verifier");
    assert!(
        path.exists(),
        "conductor-s4-verifier is missing at {}; `cargo test --all` builds it",
        path.display()
    );
    path
}

struct World {
    dir: tempfile::TempDir,
    workspace: PathBuf,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).expect("mkdir");
        std::fs::write(workspace.join("src/lib.rs"), "pub fn a() {}\n").expect("write");
        git(&workspace, &["init", "--initial-branch=main"]);
        git(&workspace, &["config", "user.name", "Fixture"]);
        git(&workspace, &["config", "user.email", "fixture@localhost"]);
        git(&workspace, &["config", "commit.gpgsign", "false"]);
        git(&workspace, &["add", "-A"]);
        git(&workspace, &["commit", "-m", "initial"]);

        let mut store = Store::open_or_create(dir.path().join("conductor.db")).expect("store");
        conductor_store::with_immediate(store.conn_mut(), |tx| {
            tx.execute(
                "INSERT INTO project (id, root_path, repo_identity, default_branch, config_hash, created_at)
                 VALUES ('p-1', '/fixture', 'blake3:repo', 'main', 'blake3:cfg', 0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO plan_version (id, project_id, version, content_hash, state, source_path)
                 VALUES ('pv-1', 'p-1', 1, 'blake3:plan', 'APPROVED', 'plan.yaml')",
                [],
            )?;
            tx.execute(
                "INSERT INTO policy_snapshot (hash, canonical_blob, created_at) VALUES (?1, '{}', 0)",
                rusqlite::params![common::agent::POLICY_HASH],
            )?;
            tx.execute(
                "INSERT INTO task (id, plan_version_id, slice_id, state, scope_globs,
                                   verification_profile, attempt_budget, created_at)
                 VALUES ('T-0012', 'pv-1', 'S4', 'READY', '[\"src/**\"]', 'default', 3, 0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO run (id, task_id, policy_hash, base_commit, run_branch, state,
                                  priority, lease_epoch, created_at)
                 VALUES (?1, 'T-0012', ?2, 'abc123', 'conductor/T-0012/r-0041', 'READY', 100, 0, 0)",
                rusqlite::params![RUN, common::agent::POLICY_HASH],
            )?;
            Ok(())
        })
        .expect("seed");

        World { dir, workspace }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("conductor.db")
    }

    fn store(&self) -> Store {
        Store::open_or_create(self.db()).expect("store")
    }

    fn write_profile(&self, yaml: &str) -> PathBuf {
        let path = self.dir.path().join("verification.yaml");
        std::fs::write(&path, yaml).expect("write profile");
        path
    }

    fn spawn_verifier(&self, profile: &Path, worker: &str, extra: &[&str]) -> Child {
        self.spawn_verifier_at(profile, worker, NOW, extra)
    }

    fn spawn_verifier_at(&self, profile: &Path, worker: &str, now: i64, extra: &[&str]) -> Child {
        let mut command = Command::new(verifier_binary());
        command
            .arg("--db")
            .arg(self.db())
            .arg("--run")
            .arg(RUN)
            .arg("--worker")
            .arg(worker)
            .arg("--now")
            .arg(now.to_string())
            .arg("--workspace")
            .arg(&self.workspace)
            .arg("--artifacts")
            .arg(self.dir.path().join("artifacts"))
            .arg("--scratch-index")
            .arg(self.dir.path().join("scratch").join("index"))
            .arg("--profile")
            .arg(profile)
            .arg("--out")
            .arg(self.dir.path().join("report.json"))
            .args(extra)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command.spawn().expect("spawn the verifier")
    }
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn sh(script: &str) -> String {
    format!(
        "[\"/bin/sh\", \"-c\", {}]",
        serde_json::to_string(script).expect("json")
    )
}

fn verification_rows(store: &Store) -> Vec<(String, String)> {
    store
        .conn()
        .prepare("SELECT check_id, outcome FROM verification_check")
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows")
}

// ---------------------------------------------------------------------------
// kill mid-check
// ---------------------------------------------------------------------------

#[test]
fn verify_killing_conductor_mid_check_never_leaves_a_result_behind() {
    // The invariant: a result is written *after* the check completes and after
    // the tree has been hashed a second time. Kill in between and the database
    // must hold nothing at all — no PASS, no FAIL, not even a partial row —
    // because §4.7 step 6 then correctly re-runs at the unchanged tree.
    let world = World::new();

    // The check announces itself through a FIFO, so the kill lands while it is
    // genuinely running rather than after a hopeful sleep.
    let fifo = world.path().join("check-started");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must be available");
    assert!(status.success());

    let profile = world.write_profile(&format!(
        "verification:\n  required:\n    - id: slow\n      command: {}\n      timeout_seconds: 120\n",
        sh(&format!(
            "echo running > {}; sleep 120",
            fifo.display()
        ))
    ));
    let mut verifier = world.spawn_verifier(&profile, "worker-doomed", &[]);

    // Blocks until the check opens the FIFO for writing. Exact rendezvous.
    let mut announcement = String::new();
    std::fs::File::open(&fifo)
        .expect("open the fifo")
        .read_to_string(&mut announcement)
        .expect("read the fifo");
    assert_eq!(announcement.trim(), "running");

    let verifier_pid = verifier.id() as i32;
    unsafe {
        libc::kill(verifier_pid, libc::SIGKILL);
    }
    let status = verifier.wait().expect("reap");
    assert!(!status.success(), "the verifier must have been killed");

    let store = world.store();
    assert_eq!(
        verification_rows(&store),
        Vec::<(String, String)>::new(),
        "a check that never finished must not have left a verdict"
    );

    // And the tree is unchanged, so §4.7 step 6's "re-run only if the tree hash
    // has no cached valid result" re-runs it — with a decided outcome this time.
    let profile = world.write_profile(&format!(
        "verification:\n  required:\n    - id: slow\n      command: {}\n",
        sh("exit 0")
    ));
    // The successor arrives after the dead worker's 60-second lease has
    // lapsed; before that, §4.7's predicate correctly protects the run.
    let out = world
        .spawn_verifier_at(&profile, "worker-successor", NOW + 120_000, &[])
        .wait_with_output()
        .expect("second run");
    assert!(
        out.status.success(),
        "the successor failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        verification_rows(&world.store()),
        vec![("slow".to_string(), "PASS".to_string())]
    );

    // Housekeeping: the killed verifier could not run its own `Drop`, so its
    // check may be orphaned. See the note in the test below.
    reap_stragglers();
}

#[test]
fn verify_a_killed_conductor_leaves_the_run_reclaimable_by_a_successor() {
    // §4.7's ordinary recovery path, exercised through a real kill rather than
    // a simulated one: the lease lapses, a successor sweeps and claims.
    let world = World::new();
    let fifo = world.path().join("check-started");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo")
            .success()
    );

    let profile = world.write_profile(&format!(
        "verification:\n  required:\n    - id: slow\n      command: {}\n      timeout_seconds: 120\n",
        sh(&format!("echo running > {}; sleep 120", fifo.display()))
    ));
    let mut verifier = world.spawn_verifier(&profile, "worker-doomed", &[]);
    let mut announcement = String::new();
    std::fs::File::open(&fifo)
        .expect("open")
        .read_to_string(&mut announcement)
        .expect("read");
    unsafe {
        libc::kill(verifier.id() as i32, libc::SIGKILL);
    }
    verifier.wait().expect("reap");

    let mut store = world.store();
    store.expire_leases(NOW + 120_000).expect("sweep");
    let claimed = store
        .claim_run(
            &RunId::new(RUN).expect("id"),
            "worker-successor",
            NOW + 120_000,
            60_000,
        )
        .expect("claim")
        .expect("a swept run is claimable");
    assert!(
        claimed.fence().lease_epoch() > 1,
        "the epoch must have moved past the dead worker's"
    );

    reap_stragglers();
}

/// Kill anything the killed verifier orphaned.
///
/// **A known limitation, stated rather than hidden.** `SIGKILL` runs no
/// destructor, so the verifier's `Drop` — which kills the check's process group
/// — does not run. The orphaned check survives its supervisor. This is the same
/// class of gap S3 has for agents, and it is not new here: §4.7's recovery
/// answers it by probing recorded pids on restart, which is S5's wiring. The
/// consequence for S4 is bounded and *detected* rather than missed: an orphan
/// still writing into the workspace moves the tree, and the next check's
/// before/after hashes turn that into `VOID`, not a false `PASS`.
fn reap_stragglers() {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let out = Command::new("pkill").args(["-f", "sleep 120"]).status();
        if out.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// a filesystem that refuses the write
// ---------------------------------------------------------------------------

#[test]
fn verify_a_log_the_filesystem_refuses_to_store_is_inconclusive_never_fail() {
    // D8's "fill the disk", staged as `RLIMIT_FSIZE` rather than as a real
    // `ENOSPC`.
    //
    // **Why not a real ENOSPC.** A genuine one needs a filesystem that can be
    // filled without disturbing the host, i.e. a small mounted image. On this
    // machine that is unavailable to an unprivileged process: `hdiutil attach
    // -nomount ram://…` gives a device, and `mount_hfs` then fails with
    // `error on mount(): error = -1`. Measured, not assumed.
    //
    // `RLIMIT_FSIZE` is the same *class* of failure and arguably a sharper
    // test: the file opens successfully and the write fails **part-way
    // through**, which is exactly the shape of ENOSPC and not the shape of a
    // permissions error. The kernel produces the error; nothing is stubbed.
    //
    // The `StorageFull` errno itself is covered where it can be covered
    // honestly — `verify::classify`'s unit tests, over a constructed
    // `Termination::Infrastructure`.
    let world = World::new();

    // A limit above whatever SQLite and git need, and far below what the check
    // is about to emit. Measured rather than guessed, because a limit that
    // caught the database would be testing something else entirely.
    let db_bytes = std::fs::metadata(world.db()).expect("db").len();
    let limit = db_bytes.max(256 * 1024) + 256 * 1024;

    let profile = world.write_profile(&format!(
        "verification:\n  required:\n    - id: noisy\n      command: {}\n      timeout_seconds: 60\n",
        sh("i=0; while [ $i -lt 40000 ]; do echo \
            aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; \
            i=$((i+1)); done; exit 0")
    ));

    let out = world
        .spawn_verifier(&profile, "worker-1", &["--fsize-limit", &limit.to_string()])
        .wait_with_output()
        .expect("run");
    assert!(
        out.status.success(),
        "the verifier itself must survive a log it cannot write: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(world.path().join("report.json")).expect("report"))
            .expect("json");
    let outcome = report["results"][0]["outcome"]
        .as_str()
        .expect("an outcome");
    assert_eq!(
        outcome, "INCONCLUSIVE",
        "a filesystem that refused the log is infrastructure, never a failing \
         check; report was {report}"
    );

    // And it is on the record as evidence, while not being cacheable.
    assert_eq!(
        verification_rows(&world.store()),
        vec![("noisy".to_string(), "INCONCLUSIVE".to_string())]
    );
}

#[test]
fn verify_a_log_directory_that_refuses_writes_is_inconclusive_never_fail() {
    // The other half of the same rule, injected differently so that one bug
    // cannot hide both: here the log cannot be created at all.
    //
    // The first attempt at this injection put a *directory* in the log's exact
    // path — and the runner routed around it, because §4.5's log name is not
    // unique and the runner qualifies it on collision. That is correct
    // behaviour, so the injection had to become one that cannot be routed
    // around: a verification directory the process may not write to.
    let world = World::new();

    // One ordinary run first, so the directory is created *with provenance* by
    // the worker that owns it. Creating it by hand would be refused — correctly,
    // by S3's `Unattributed` rule — and the test would then be measuring path
    // ownership rather than log failure.
    let warmup = world.write_profile(&format!(
        "verification:\n  required:\n    - id: warmup\n      command: {}\n",
        sh("exit 0")
    ));
    let out = world
        .spawn_verifier(&warmup, "worker-1", &[])
        .wait_with_output()
        .expect("warmup");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let verification_dir = world
        .path()
        .join("artifacts")
        .join(RUN)
        .join("verification");
    let restore = std::fs::metadata(&verification_dir)
        .expect("metadata")
        .permissions();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&verification_dir, std::fs::Permissions::from_mode(0o555))
            .expect("make the log directory read-only");
    }

    let profile = world.write_profile(&format!(
        "verification:\n  required:\n    - id: blocked\n      command: {}\n",
        sh("exit 0")
    ));
    let out = world
        .spawn_verifier_at(&profile, "worker-1", NOW + 120_000, &[])
        .wait_with_output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // Restore before asserting, so a failure does not leave an undeletable
    // temporary directory behind.
    std::fs::set_permissions(&verification_dir, restore).expect("restore");

    assert!(out.status.success(), "{stderr}");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(world.path().join("report.json")).expect("report"))
            .expect("json");
    assert_eq!(
        report["results"][0]["outcome"].as_str(),
        Some("INCONCLUSIVE"),
        "a log Conductor cannot write is infrastructure, never a failing check; \
         report was {report}"
    );
}
