//! S1 claim instrument: concurrency correctness and latency for the run claim,
//! measured against the **real store code** under `rusqlite`.
//!
//! ADR-0004 pre-registered a falsification trigger requiring S1 to re-measure,
//! because S0 measured SQLite-via-Python (3.53.3), not the shipping stack. Two
//! rules follow and are load-bearing:
//!
//! 1. **No reimplementation.** Every claim goes through
//!    `conductor_store::Store::claim_next_run` on a connection opened by the
//!    production `Store::open_existing` path, with the production pragmas.
//!    Measuring a reimplementation is exactly the mistake ADR-0004 flagged.
//! 2. **Separate processes, not threads** — matching S0, so the numbers are
//!    comparable.
//!
//! The four invariants, and the self-test that proves the checkers can fail:
//!
//! | | invariant |
//! |---|---|
//! | I1 | no duplicate ownership — every seeded run claimed exactly once, claims == rows |
//! | I2 | no partial transition — no `RUNNING` with a NULL owner, no `READY` with one |
//! | I3 | `lease_epoch` incremented exactly once per claim |
//! | I4 | `PRAGMA integrity_check` returns `ok` |
//!
//! Usage:
//! ```text
//! conductor-claim-bench --self-test
//! conductor-claim-bench --writers 1,4,16 --rows 300 --repeat 2 --think-ms 1 --out RESULT.json
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use conductor_store::{Store, StoreError, StoreResult, with_immediate};
use rusqlite::{ErrorCode, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const POLICY_HASH: &str = "blake3:0000000000000000000000000000000000000000000000000000000000000000";
const MAX_CONSECUTIVE_RETRIES: u32 = 200;

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Config {
    writers: Vec<usize>,
    rows: usize,
    repeat: usize,
    think_ms: f64,
    lease_ms: i64,
    fullfsync_off: bool,
    label: String,
    work_dir: Option<PathBuf>,
    out: Option<PathBuf>,
    rusqlite_version: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            writers: vec![1, 4, 16],
            rows: 300,
            repeat: 2,
            think_ms: 0.0,
            lease_ms: 60_000,
            fullfsync_off: false,
            label: "unlabelled".to_string(),
            work_dir: None,
            out: None,
            rusqlite_version: "unspecified".to_string(),
        }
    }
}

fn parse_config(args: &[String]) -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let mut value = || -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--writers" => {
                cfg.writers = value()?
                    .split(',')
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.trim().parse::<usize>().map_err(|e| e.to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            "--rows" => {
                cfg.rows = value()?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?
            }
            "--repeat" => {
                cfg.repeat = value()?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?
            }
            "--think-ms" => {
                cfg.think_ms = value()?
                    .parse()
                    .map_err(|e: std::num::ParseFloatError| e.to_string())?
            }
            "--lease-ms" => {
                cfg.lease_ms = value()?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?
            }
            "--fullfsync-off" => cfg.fullfsync_off = true,
            "--label" => cfg.label = value()?,
            "--work-dir" => cfg.work_dir = Some(PathBuf::from(value()?)),
            "--out" => cfg.out = Some(PathBuf::from(value()?)),
            "--rusqlite-version" => cfg.rusqlite_version = value()?,
            other => return Err(format!("unknown flag {other}")),
        }
        i += 1;
    }
    if cfg.writers.is_empty() {
        return Err("--writers must name at least one writer count".to_string());
    }
    if cfg.rows == 0 || cfg.repeat == 0 {
        return Err("--rows and --repeat must be positive".to_string());
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkerResult {
    worker_id: usize,
    owner: String,
    pid: u32,
    latencies_ms: Vec<f64>,
    /// `(run_id, lease_epoch)` as returned by the claim itself.
    claimed: Vec<(String, i64)>,
    iterations: u64,
    empty_attempts: u64,
    busy_errors: u64,
    other_errors: Vec<String>,
    elapsed_s: f64,
    fatal: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkerArgs {
    db: PathBuf,
    worker_id: usize,
    lease_ms: i64,
    think_ms: f64,
    fullfsync_off: bool,
    out: PathBuf,
    gate: PathBuf,
}

impl WorkerArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let [db, worker_id, lease_ms, think_ms, fullfsync_off, out, gate] = args else {
            return Err(
                "--worker needs: <db> <id> <lease_ms> <think_ms> <fullfsync_off> <out> <gate>"
                    .to_string(),
            );
        };
        Ok(WorkerArgs {
            db: PathBuf::from(db),
            worker_id: worker_id.parse().map_err(|_| "bad worker id")?,
            lease_ms: lease_ms.parse().map_err(|_| "bad lease_ms")?,
            think_ms: think_ms.parse().map_err(|_| "bad think_ms")?,
            fullfsync_off: fullfsync_off == "1",
            out: PathBuf::from(out),
            gate: PathBuf::from(gate),
        })
    }

    fn to_cli(&self) -> Vec<String> {
        vec![
            "--worker".to_string(),
            self.db.display().to_string(),
            self.worker_id.to_string(),
            self.lease_ms.to_string(),
            self.think_ms.to_string(),
            u8::from(self.fullfsync_off).to_string(),
            self.out.display().to_string(),
            self.gate.display().to_string(),
        ]
    }
}

fn is_busy(err: &StoreError) -> bool {
    fn busy(e: &rusqlite::Error) -> bool {
        matches!(
            e,
            rusqlite::Error::SqliteFailure(inner, _)
                if matches!(inner.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        )
    }
    match err {
        StoreError::Sqlite(e) => busy(e),
        StoreError::RollbackFailed { source, .. } => busy(source),
        _ => false,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One writer process: claims until the queue is empty.
fn run_worker(args: &WorkerArgs) -> ExitCode {
    let owner = format!("worker-{}-pid{}", args.worker_id, std::process::id());
    let mut result = WorkerResult {
        worker_id: args.worker_id,
        owner: owner.clone(),
        pid: std::process::id(),
        latencies_ms: Vec::new(),
        claimed: Vec::new(),
        iterations: 0,
        empty_attempts: 0,
        busy_errors: 0,
        other_errors: Vec::new(),
        elapsed_s: 0.0,
        fatal: None,
    };

    // The production open path, with the production pragmas.
    let mut store = match Store::open_existing(&args.db) {
        Ok(store) => store,
        Err(err) => {
            result.fatal = Some(format!("open: {err}"));
            write_json(&args.out, &result);
            return ExitCode::from(70);
        }
    };
    if args.fullfsync_off {
        // Measurement-only downgrade, for comparability with ADR-0004's
        // fullfsync=0 tables. Never a shipping configuration.
        if let Err(err) = store
            .conn()
            .execute_batch("PRAGMA fullfsync = 0; PRAGMA checkpoint_fullfsync = 0;")
        {
            result.fatal = Some(format!("fullfsync downgrade: {err}"));
            write_json(&args.out, &result);
            return ExitCode::from(70);
        }
    }

    println!("READY");
    let _ = std::io::stdout().flush();
    while !args.gate.exists() {
        std::thread::sleep(Duration::from_micros(200));
    }

    let started = Instant::now();
    let mut retries: u32 = 0;
    loop {
        result.iterations += 1;
        let t0 = Instant::now();
        match store.claim_next_run(&owner, now_ms(), args.lease_ms) {
            Ok(Some(claimed)) => {
                result
                    .latencies_ms
                    .push(t0.elapsed().as_secs_f64() * 1000.0);
                result
                    .claimed
                    .push((claimed.run_id.to_string(), claimed.lease_epoch));
                retries = 0;
                if args.think_ms > 0.0 {
                    // Outside the measured window: this is the work a real
                    // worker would do while holding the run, and it is what
                    // makes writers genuinely overlap.
                    std::thread::sleep(Duration::from_secs_f64(args.think_ms / 1000.0));
                }
            }
            Ok(None) => {
                result.empty_attempts += 1;
                break;
            }
            Err(err) => {
                if is_busy(&err) {
                    result.busy_errors += 1;
                } else {
                    result.other_errors.push(err.to_string());
                }
                retries += 1;
                if retries > MAX_CONSECUTIVE_RETRIES {
                    result.fatal = Some(format!("too many consecutive retries: {err}"));
                    break;
                }
                std::thread::sleep(Duration::from_millis(u64::from(retries.min(20))));
            }
        }
    }
    result.elapsed_s = started.elapsed().as_secs_f64();

    let fatal = result.fatal.is_some();
    write_json(&args.out, &result);
    if fatal {
        ExitCode::from(70)
    } else {
        ExitCode::SUCCESS
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    let json = serde_json::to_string(value).expect("serialize");
    fs::write(path, json).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

// ---------------------------------------------------------------------------
// fixture
// ---------------------------------------------------------------------------

/// Seed `rows` claimable runs. `run` needs a task, a plan version, a project and
/// a policy snapshot, and `ix_run_one_active_per_task` allows one active run per
/// task — so N claimable runs means N tasks.
fn seed(store: &mut Store, rows: usize) -> StoreResult<()> {
    let created = now_ms();
    with_immediate(store.conn_mut(), |tx| {
        tx.execute(
            "INSERT INTO project (id, root_path, repo_identity, default_branch, config_hash, created_at)
             VALUES ('p-bench', '/bench/repo', 'blake3:repo', 'main', 'blake3:cfg', ?1)",
            params![created],
        )?;
        tx.execute(
            "INSERT INTO plan_version (id, project_id, version, content_hash, state, source_path)
             VALUES ('pv-1', 'p-bench', 1, 'blake3:plan', 'APPROVED', '.conductor/plans/v1/plan.yaml')",
            [],
        )?;
        tx.execute(
            "INSERT INTO policy_snapshot (hash, canonical_blob, created_at) VALUES (?1, '{}', ?2)",
            params![POLICY_HASH, created],
        )?;
        for i in 1..=rows {
            let task_id = format!("T-{i:06}");
            let run_id = format!("r-{i:06}");
            tx.execute(
                "INSERT INTO task
                   (id, plan_version_id, slice_id, state, scope_globs, verification_profile,
                    attempt_budget, created_at)
                 VALUES (?1, 'pv-1', 'S1', 'READY', '[]', 'default', 3, ?2)",
                params![task_id, created + i as i64],
            )?;
            tx.execute(
                "INSERT INTO run
                   (id, task_id, policy_hash, base_commit, run_branch, state, priority,
                    lease_owner, lease_expires_at, lease_epoch, created_at)
                 VALUES (?1, ?2, ?3, 'abc123', 'conductor/run', 'READY', ?4, NULL, NULL, 0, ?5)",
                params![
                    run_id,
                    task_id,
                    POLICY_HASH,
                    (i % 8) as i64,
                    created + i as i64
                ],
            )?;
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// invariants
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct Check {
    pass: bool,
    detail: Value,
}

#[derive(Debug, Clone, Serialize)]
struct Invariants {
    i1_no_duplicate_ownership: Check,
    i2_no_partial_transition: Check,
    i3_lease_epoch_exactly_one: Check,
    i4_integrity_check: Check,
    all_pass: bool,
}

fn scalar(store: &Store, sql: &str) -> i64 {
    store
        .conn()
        .query_row(sql, [], |row| row.get(0))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn verify(db: &Path, rows: usize, results: &[WorkerResult]) -> Invariants {
    let store = Store::open_existing(db).expect("reopen for verification");

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut returned_epoch_bad = 0usize;
    let mut total_claims = 0usize;
    for r in results {
        for (run_id, epoch) in &r.claimed {
            *counts.entry(run_id.as_str()).or_insert(0) += 1;
            total_claims += 1;
            if *epoch != 1 {
                returned_epoch_bad += 1;
            }
        }
    }
    let duplicates: BTreeMap<&str, usize> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(id, n)| (*id, *n))
        .collect();

    // I1 — no duplicate ownership.
    let db_running = scalar(&store, "SELECT COUNT(*) FROM run WHERE state='RUNNING'");
    let db_ready = scalar(&store, "SELECT COUNT(*) FROM run WHERE state='READY'");
    let db_total = scalar(&store, "SELECT COUNT(*) FROM run");
    let ev_total = scalar(&store, "SELECT COUNT(*) FROM event");
    let ev_distinct = scalar(&store, "SELECT COUNT(DISTINCT run_id) FROM event");
    let ev_wrong_kind = scalar(
        &store,
        "SELECT COUNT(*) FROM event WHERE kind <> 'RUN_CLAIMED'",
    );
    let orphan_events = scalar(
        &store,
        "SELECT COUNT(*) FROM event e LEFT JOIN run r ON r.id = e.run_id WHERE r.id IS NULL",
    );
    let rows_i64 = rows as i64;
    let i1_pass = duplicates.is_empty()
        && total_claims == rows
        && counts.len() == rows
        && db_running == rows_i64
        && db_ready == 0
        && db_total == rows_i64
        && ev_total == rows_i64
        && ev_distinct == rows_i64
        && ev_wrong_kind == 0
        && orphan_events == 0;

    // I2 — no partial transition.
    let running_null_owner = scalar(
        &store,
        "SELECT COUNT(*) FROM run WHERE state='RUNNING' AND lease_owner IS NULL",
    );
    let ready_with_owner = scalar(
        &store,
        "SELECT COUNT(*) FROM run WHERE state='READY' AND lease_owner IS NOT NULL",
    );
    let running_null_expiry = scalar(
        &store,
        "SELECT COUNT(*) FROM run WHERE state='RUNNING' AND lease_expires_at IS NULL",
    );
    let claimed_without_event = scalar(
        &store,
        "SELECT COUNT(*) FROM run r LEFT JOIN event e ON e.run_id = r.id
         WHERE r.state='RUNNING' AND e.run_id IS NULL",
    );
    let i2_pass = running_null_owner == 0
        && ready_with_owner == 0
        && running_null_expiry == 0
        && claimed_without_event == 0;

    // I3 — lease_epoch incremented exactly once per claim.
    let epoch_not_one = scalar(&store, "SELECT COUNT(*) FROM run WHERE lease_epoch <> 1");
    let (epoch_min, epoch_max): (i64, i64) = store
        .conn()
        .query_row(
            "SELECT COALESCE(MIN(lease_epoch), -1), COALESCE(MAX(lease_epoch), -1) FROM run",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("epoch range");
    let i3_pass = epoch_not_one == 0 && returned_epoch_bad == 0;

    // I4 — structural integrity.
    let integrity = store
        .integrity_check()
        .unwrap_or_else(|e| vec![format!("integrity_check failed: {e}")]);
    let fk_violations = store.foreign_key_check().unwrap_or(usize::MAX);
    let i4_pass = integrity == ["ok"] && fk_violations == 0;

    Invariants {
        i1_no_duplicate_ownership: Check {
            pass: i1_pass,
            detail: json!({
                "total_claims_recorded_by_workers": total_claims,
                "rows_seeded": rows,
                "distinct_ids_claimed": counts.len(),
                "duplicate_run_ids": duplicates,
                "db_running": db_running,
                "db_ready": db_ready,
                "db_total": db_total,
                "event_rows": ev_total,
                "event_distinct_run_ids": ev_distinct,
                "event_wrong_kind": ev_wrong_kind,
                "orphan_events": orphan_events,
            }),
        },
        i2_no_partial_transition: Check {
            pass: i2_pass,
            detail: json!({
                "running_with_null_lease_owner": running_null_owner,
                "ready_with_lease_owner": ready_with_owner,
                "running_with_null_lease_expires_at": running_null_expiry,
                "running_without_claim_event": claimed_without_event,
            }),
        },
        i3_lease_epoch_exactly_one: Check {
            pass: i3_pass,
            detail: json!({
                "rows_with_epoch_not_1": epoch_not_one,
                "epoch_min": epoch_min,
                "epoch_max": epoch_max,
                "returned_epochs_not_1": returned_epoch_bad,
            }),
        },
        i4_integrity_check: Check {
            pass: i4_pass,
            detail: json!({
                "integrity_check": integrity,
                "foreign_key_check_violations": fk_violations,
            }),
        },
        all_pass: i1_pass && i2_pass && i3_pass && i4_pass,
    }
}

// ---------------------------------------------------------------------------
// statistics
// ---------------------------------------------------------------------------

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let k = (sorted.len() - 1) as f64 * (p / 100.0);
    let lo = k.floor() as usize;
    let hi = k.ceil() as usize;
    if lo == hi {
        Some(sorted[lo])
    } else {
        Some(sorted[lo] + (sorted[hi] - sorted[lo]) * (k - lo as f64))
    }
}

fn summarize(latencies: &[f64]) -> Value {
    if latencies.is_empty() {
        return json!({ "count": 0 });
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN latencies"));
    let sum: f64 = sorted.iter().sum();
    json!({
        "count": sorted.len(),
        "min_ms": sorted[0],
        "median_ms": percentile(&sorted, 50.0),
        "mean_ms": sum / sorted.len() as f64,
        "p95_ms": percentile(&sorted, 95.0),
        "p99_ms": percentile(&sorted, 99.0),
        "max_ms": sorted[sorted.len() - 1],
    })
}

// ---------------------------------------------------------------------------
// orchestration
// ---------------------------------------------------------------------------

struct RunOutcome {
    record: Value,
    latencies: Vec<f64>,
    invariants_pass: bool,
    duplicate_claims: usize,
    busy_errors: u64,
    other_errors: usize,
    active_writers: usize,
}

fn remove_db(db: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", db.display()));
        let _ = fs::remove_file(path);
    }
}

fn run_once(cfg: &Config, work_dir: &Path, writers: usize, repeat: usize) -> RunOutcome {
    let db = work_dir.join(format!("claim_w{writers}_r{repeat}.db"));
    remove_db(&db);
    let gate = work_dir.join(format!("gate_w{writers}_r{repeat}"));
    let _ = fs::remove_file(&gate);

    {
        let mut store = Store::open_or_create(&db).expect("create bench store");
        seed(&mut store, cfg.rows).expect("seed");
    }

    let exe = std::env::current_exe().expect("current_exe");
    let mut children: Vec<Child> = Vec::with_capacity(writers);
    let mut out_paths: Vec<PathBuf> = Vec::with_capacity(writers);

    for worker_id in 0..writers {
        let out = work_dir.join(format!("w{writers}_r{repeat}_{worker_id}.json"));
        let _ = fs::remove_file(&out);
        let args = WorkerArgs {
            db: db.clone(),
            worker_id,
            lease_ms: cfg.lease_ms,
            think_ms: cfg.think_ms,
            fullfsync_off: cfg.fullfsync_off,
            out: out.clone(),
            gate: gate.clone(),
        };
        let child = Command::new(&exe)
            .args(args.to_cli())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn worker");
        children.push(child);
        out_paths.push(out);
    }

    // Every writer must be open and waiting before any of them starts, or the
    // first writer drains the queue while the others are still linking.
    for (worker_id, child) in children.iter_mut().enumerate() {
        let stdout = child.stdout.take().expect("worker stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read worker readiness");
        assert_eq!(line.trim(), "READY", "worker {worker_id} never got ready");
    }

    let started = Instant::now();
    fs::write(&gate, b"go").expect("open the gate");

    let mut exit_codes = Vec::with_capacity(writers);
    for child in &mut children {
        let status = child.wait().expect("wait for worker");
        exit_codes.push(status.code().unwrap_or(-1));
    }
    let wall_s = started.elapsed().as_secs_f64();

    let results: Vec<WorkerResult> = out_paths
        .iter()
        .map(|p| {
            let text =
                fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
            serde_json::from_str(&text).expect("parse worker result")
        })
        .collect();

    let invariants = verify(&db, cfg.rows, &results);

    let latencies: Vec<f64> = results
        .iter()
        .flat_map(|r| r.latencies_ms.clone())
        .collect();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for r in &results {
        for (run_id, _) in &r.claimed {
            *seen.entry(run_id.as_str()).or_insert(0) += 1;
        }
    }
    let duplicate_claims: usize = seen.values().filter(|n| **n > 1).map(|n| n - 1).sum();
    let busy_errors: u64 = results.iter().map(|r| r.busy_errors).sum();
    let other_errors: usize = results.iter().map(|r| r.other_errors.len()).sum();
    let active_writers = results.iter().filter(|r| !r.claimed.is_empty()).count();
    let fatals: Vec<&String> = results.iter().filter_map(|r| r.fatal.as_ref()).collect();

    let record = json!({
        "writers": writers,
        "repeat": repeat,
        "rows": cfg.rows,
        "think_ms": cfg.think_ms,
        "fullfsync_off": cfg.fullfsync_off,
        "active_writers": active_writers,
        "idle_writers": writers - active_writers,
        "iterations": results.iter().map(|r| r.iterations).sum::<u64>(),
        "claims": latencies.len(),
        "empty_attempts": results.iter().map(|r| r.empty_attempts).sum::<u64>(),
        "busy_errors": busy_errors,
        "other_errors": results.iter().flat_map(|r| r.other_errors.clone()).collect::<Vec<_>>(),
        "duplicate_claims": duplicate_claims,
        "wall_s": wall_s,
        "throughput_claims_per_s": if wall_s > 0.0 { latencies.len() as f64 / wall_s } else { 0.0 },
        "latency": summarize(&latencies),
        "per_worker_claims": results.iter().map(|r| (r.worker_id.to_string(), r.claimed.len())).collect::<BTreeMap<_, _>>(),
        "worker_exit_codes": exit_codes,
        "fatals": fatals,
        "invariants": invariants,
    });

    let invariants_pass = record["invariants"]["all_pass"].as_bool().unwrap_or(false);
    remove_db(&db);
    let _ = fs::remove_file(&gate);

    RunOutcome {
        record,
        latencies,
        invariants_pass,
        duplicate_claims,
        busy_errors,
        other_errors,
        active_writers,
    }
}

// ---------------------------------------------------------------------------
// self-test — ADR-0004 decision 4
// ---------------------------------------------------------------------------

fn self_test() -> bool {
    let dir = std::env::temp_dir().join(format!("conductor_s1_selftest_{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create self-test dir");

    // --- logical corruption: I1, I2, I3 must fire; I4 must stay ok ----------
    let rows = 10usize;
    let db = dir.join("selftest.db");
    remove_db(&db);
    {
        let mut store = Store::open_or_create(&db).expect("open");
        seed(&mut store, rows).expect("seed");
        with_immediate(store.conn_mut(), |tx| {
            for i in 1..=8 {
                let run_id = format!("r-{i:06}");
                tx.execute(
                    "UPDATE run SET state='RUNNING', lease_owner='w', lease_expires_at=1,
                       lease_epoch=1 WHERE id=?1",
                    params![run_id],
                )?;
                tx.execute(
                    "INSERT INTO event (run_id, seq, kind, payload, at)
                     VALUES (?1, 1, 'RUN_CLAIMED', '{}', 0)",
                    params![run_id],
                )?;
            }
            // row 9: RUNNING with no owner and no expiry -> I2
            tx.execute(
                "UPDATE run SET state='RUNNING', lease_owner=NULL, lease_expires_at=NULL,
                   lease_epoch=1 WHERE id='r-000009'",
                [],
            )?;
            tx.execute(
                "INSERT INTO event (run_id, seq, kind, payload, at)
                 VALUES ('r-000009', 1, 'RUN_CLAIMED', '{}', 0)",
                [],
            )?;
            // row 10: claimed twice -> epoch 2 -> I3
            tx.execute(
                "UPDATE run SET state='RUNNING', lease_owner='w', lease_expires_at=1,
                   lease_epoch=2 WHERE id='r-000010'",
                [],
            )?;
            tx.execute(
                "INSERT INTO event (run_id, seq, kind, payload, at)
                 VALUES ('r-000010', 1, 'RUN_CLAIMED', '{}', 0)",
                [],
            )?;
            Ok(())
        })
        .expect("corrupt state");
    }

    // Two workers both report claiming r-000010 -> I1.
    let fake = vec![
        WorkerResult {
            worker_id: 0,
            owner: "w0".to_string(),
            pid: 0,
            latencies_ms: vec![],
            claimed: (1..=9)
                .map(|i| (format!("r-{i:06}"), 1))
                .chain([("r-000010".to_string(), 2)])
                .collect(),
            iterations: 0,
            empty_attempts: 0,
            busy_errors: 0,
            other_errors: vec![],
            elapsed_s: 0.0,
            fatal: None,
        },
        WorkerResult {
            worker_id: 1,
            owner: "w1".to_string(),
            pid: 0,
            latencies_ms: vec![],
            claimed: vec![("r-000010".to_string(), 2)],
            iterations: 0,
            empty_attempts: 0,
            busy_errors: 0,
            other_errors: vec![],
            elapsed_s: 0.0,
            fatal: None,
        },
    ];
    let inv = verify(&db, rows, &fake);
    remove_db(&db);

    // --- physical corruption: I4 must fire ---------------------------------
    let corrupt_db = dir.join("selftest_corrupt.db");
    remove_db(&corrupt_db);
    {
        let mut store = Store::open_or_create(&corrupt_db).expect("open");
        seed(&mut store, 200).expect("seed");
        // Move every page out of the WAL and into the main file, so scribbling
        // on the main file is actually visible.
        store
            .conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");
    }
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&corrupt_db)
            .expect("open db file for corruption");
        let len = file.metadata().expect("metadata").len();
        assert!(len > 16_384, "db too small to corrupt meaningfully");
        file.seek(SeekFrom::Start(8_192)).expect("seek");
        file.write_all(&[0xA5u8; 4_096]).expect("scribble");
        file.sync_all().expect("sync");
    }
    let i4_fires = match Store::open_existing(&corrupt_db) {
        Ok(store) => match store.integrity_check() {
            Ok(report) => report != ["ok"],
            Err(_) => true, // SQLite refused to even run it: also detection
        },
        Err(_) => true,
    };
    remove_db(&corrupt_db);
    let _ = fs::remove_dir_all(&dir);

    let checks: Vec<(&str, bool)> = vec![
        (
            "I1 detects duplicate ownership",
            !inv.i1_no_duplicate_ownership.pass
                && inv.i1_no_duplicate_ownership.detail["duplicate_run_ids"]["r-000010"] == 2,
        ),
        (
            "I2 detects partial transition",
            !inv.i2_no_partial_transition.pass
                && inv.i2_no_partial_transition.detail["running_with_null_lease_owner"] == 1,
        ),
        (
            "I3 detects epoch != 1",
            !inv.i3_lease_epoch_exactly_one.pass
                && inv.i3_lease_epoch_exactly_one.detail["rows_with_epoch_not_1"] == 1,
        ),
        ("I4 detects a corrupted database file", i4_fires),
        (
            "I4 reports ok on a structurally sound db",
            inv.i4_integrity_check.pass,
        ),
        ("all_pass is false when any checker fails", !inv.all_pass),
    ];

    println!("SELF-TEST of invariant checkers (deliberately corrupted state):");
    let mut ok = true;
    for (name, passed) in &checks {
        println!("  [{}] {name}", if *passed { "PASS" } else { "FAIL" });
        ok &= passed;
    }
    println!(
        "self-test: {}",
        if ok {
            "PASS -- checkers have teeth"
        } else {
            "FAIL"
        }
    );
    ok
}

// ---------------------------------------------------------------------------
// provenance
// ---------------------------------------------------------------------------

fn cmd_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.first().map(String::as_str) == Some("--worker") {
        return match WorkerArgs::parse(&argv[1..]) {
            Ok(args) => run_worker(&args),
            Err(err) => {
                eprintln!("{err}");
                ExitCode::from(64)
            }
        };
    }

    if argv.iter().any(|a| a == "--self-test") {
        return if self_test() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    let cfg = match parse_config(&argv) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("usage error: {err}");
            return ExitCode::from(64);
        }
    };

    // S0's report records a shared result path being silently clobbered by a
    // concurrent run. Refuse to overwrite.
    if let Some(out) = &cfg.out
        && out.exists()
    {
        eprintln!(
            "refusing to overwrite existing result file {}",
            out.display()
        );
        return ExitCode::from(64);
    }

    let owned_work_dir = cfg.work_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("conductor_s1_bench_{}", std::process::id()))
    });
    fs::create_dir_all(&owned_work_dir).expect("create work dir");

    // Pragmas actually in effect, read from a real store on the real path.
    let probe_db = owned_work_dir.join("probe.db");
    remove_db(&probe_db);
    let pragmas = {
        let store = Store::open_or_create(&probe_db).expect("probe store");
        if cfg.fullfsync_off {
            // The probe must be read through the same downgrade the workers
            // apply, or `meta.pragmas` would describe a configuration nothing
            // actually ran under.
            store
                .conn()
                .execute_batch("PRAGMA fullfsync = 0; PRAGMA checkpoint_fullfsync = 0;")
                .expect("probe downgrade");
        }
        store.pragmas().expect("read pragmas").values
    };
    remove_db(&probe_db);

    let mut runs: Vec<Value> = Vec::new();
    let mut summary: Vec<Value> = Vec::new();

    for &writers in &cfg.writers {
        let mut pooled: Vec<f64> = Vec::new();
        let mut all_pass = true;
        let mut duplicate_claims = 0usize;
        let mut busy_errors = 0u64;
        let mut other_errors = 0usize;
        let mut active_total = 0usize;
        let mut claims = 0usize;
        let mut iterations = 0u64;
        let mut throughput = 0.0f64;

        for repeat in 0..cfg.repeat {
            eprintln!(
                "  running writers={writers} repeat={}/{} ...",
                repeat + 1,
                cfg.repeat
            );
            let outcome = run_once(&cfg, &owned_work_dir, writers, repeat);
            pooled.extend(outcome.latencies.iter().copied());
            all_pass &= outcome.invariants_pass;
            duplicate_claims += outcome.duplicate_claims;
            busy_errors += outcome.busy_errors;
            other_errors += outcome.other_errors;
            active_total += outcome.active_writers;
            claims += outcome.record["claims"].as_u64().unwrap_or(0) as usize;
            iterations += outcome.record["iterations"].as_u64().unwrap_or(0);
            throughput += outcome.record["throughput_claims_per_s"]
                .as_f64()
                .unwrap_or(0.0);
            if !outcome.invariants_pass {
                eprintln!("    *** INVARIANT FAILURE ***");
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&outcome.record["invariants"]).unwrap_or_default()
                );
            }
            runs.push(outcome.record);
        }

        summary.push(json!({
            "writers": writers,
            "repeats": cfg.repeat,
            "iterations": iterations,
            "claims": claims,
            "busy_errors": busy_errors,
            "duplicate_claims": duplicate_claims,
            "other_error_count": other_errors,
            "latency": summarize(&pooled),
            "active_writers_mean": active_total as f64 / cfg.repeat as f64,
            "throughput_claims_per_s_mean": throughput / cfg.repeat as f64,
            "invariants_all_pass": all_pass,
        }));
    }

    let meta = json!({
        "instrument": "conductor-claim-bench",
        "slice": "S1",
        "label": cfg.label,
        "purpose": "ADR-0004 falsification trigger 1: re-measure the claim under the shipping rusqlite stack",
        "claim_goes_through": "conductor_store::Store::claim_next_run (production code, production open path)",
        "process_model": "separate writer processes, one connection each (matches S0)",
        "sqlite_version": rusqlite::version(),
        "rusqlite_version": cfg.rusqlite_version,
        "conductor_store_version": env!("CARGO_PKG_VERSION"),
        "rustc_version": cmd_output("rustc", &["--version"]),
        "os_product_version": cmd_output("sw_vers", &["-productVersion"]),
        "os_build_version": cmd_output("sw_vers", &["-buildVersion"]),
        "uname": cmd_output("uname", &["-mrs"]),
        "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "git_commit": cmd_output("git", &["rev-parse", "HEAD"]),
        "git_dirty": !cmd_output("git", &["status", "--porcelain"]).is_empty(),
        "started_utc": cmd_output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]),
        "started_unix_s": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        "pragmas": pragmas,
        "fullfsync_off": cfg.fullfsync_off,
        "rows": cfg.rows,
        "repeat": cfg.repeat,
        "writer_counts": cfg.writers,
        "think_ms": cfg.think_ms,
        "lease_ms": cfg.lease_ms,
        "claim_sql": conductor_store::claim::CLAIM_SQL,
        "latency_definition": "wall time of Store::claim_next_run: BEGIN IMMEDIATE -> UPDATE..RETURNING -> INSERT event -> COMMIT, successful claims only",
        "percentile_method": "linear interpolation on sorted successful-claim latencies",
        "work_dir": owned_work_dir.display().to_string(),
    });

    print_table(&summary, &cfg);

    let payload = json!({ "meta": meta, "summary": summary, "runs": runs });
    if let Some(out) = &cfg.out {
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).expect("create result dir");
        }
        fs::write(
            out,
            serde_json::to_string_pretty(&payload).expect("serialize"),
        )
        .unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
        eprintln!("JSON written to: {}", out.display());
    }

    if cfg.work_dir.is_none() {
        let _ = fs::remove_dir_all(&owned_work_dir);
    }

    let all_ok = summary
        .iter()
        .all(|s| s["invariants_all_pass"].as_bool().unwrap_or(false));
    if !all_ok {
        eprintln!("!!! CORRECTNESS INVARIANT FAILURE -- see JSON for evidence !!!");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn print_table(summary: &[Value], cfg: &Config) {
    let f = |v: &Value| -> String {
        v.as_f64()
            .map(|x| format!("{x:.3}"))
            .unwrap_or_else(|| "-".to_string())
    };
    println!();
    println!("{}", "=".repeat(104));
    println!(
        "S1: claim under rusqlite -- label={} rows={} repeat={} think_ms={} fullfsync={}",
        cfg.label,
        cfg.rows,
        cfg.repeat,
        cfg.think_ms,
        if cfg.fullfsync_off { "0" } else { "1" }
    );
    println!("{}", "=".repeat(104));
    println!(
        "{:>7} {:>5} {:>8} {:>8} {:>9} {:>9} {:>9} {:>9} {:>6} {:>6} {:>6} {:>5}",
        "writers",
        "act",
        "iters",
        "claims",
        "median",
        "p95",
        "p99",
        "max",
        "busy",
        "dup",
        "errs",
        "inv"
    );
    println!("{}", "-".repeat(104));
    for s in summary {
        let lat = &s["latency"];
        println!(
            "{:>7} {:>5} {:>8} {:>8} {:>9} {:>9} {:>9} {:>9} {:>6} {:>6} {:>6} {:>5}",
            s["writers"],
            f(&s["active_writers_mean"]),
            s["iterations"],
            s["claims"],
            f(&lat["median_ms"]),
            f(&lat["p95_ms"]),
            f(&lat["p99_ms"]),
            f(&lat["max_ms"]),
            s["busy_errors"],
            s["duplicate_claims"],
            s["other_error_count"],
            if s["invariants_all_pass"].as_bool().unwrap_or(false) {
                "PASS"
            } else {
                "FAIL"
            }
        );
    }
    println!("{}", "-".repeat(104));
    println!("latency in ms. act = mean writers that claimed >=1 row; act << writers means the");
    println!("queue drained before the others contended, so that row is NOT a contention result.");
    println!("busy = SQLITE_BUSY/LOCKED count. busy=0 is not evidence of low contention: ");
    println!("busy_timeout absorbs contention into latency (ADR-0004). Budget p99, never median.");
}
