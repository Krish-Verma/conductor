#!/usr/bin/env python3
"""
S0 measurement: is SQLite `BEGIN IMMEDIATE` + single-statement atomic claim
safe and fast enough for Conductor's run-claim transaction under concurrent
writer PROCESSES?

Scope: measurement only. Touches nothing but a disposable temp database.

Correctness invariants checked after every run (these matter more than speed):
  I1  no duplicate ownership   -- every claimed row claimed exactly once,
                                  total claims == rows seeded
  I2  no partial transition    -- no RUNNING row with NULL lease_owner,
                                  no READY row with a lease_owner
  I3  lease_epoch == 1         -- incremented exactly once per claim
  I4  PRAGMA integrity_check   -- 'ok'
plus DB-level corroboration: event table has exactly one RUN_CLAIMED per run,
and UNIQUE(run_id, seq) makes a double-claim raise IntegrityError at insert
time (counted separately as hard evidence of a duplicate).

Usage:
    python3 scripts/measure/s0_sqlite_claim_latency.py
    python3 scripts/measure/s0_sqlite_claim_latency.py --writers 1,4,16 --rows 2000 --repeat 3

stdlib only.
"""

from __future__ import annotations

import argparse
import json
import math
import multiprocessing as mp
import os
import platform
import shutil
import sqlite3
import statistics
import sys
import tempfile
import time
from collections import Counter

# ----------------------------------------------------------------------------
# schema / pragmas
# ----------------------------------------------------------------------------

PRAGMAS = (
    ("journal_mode", "WAL"),
    ("synchronous", "FULL"),
    ("foreign_keys", "ON"),
    ("busy_timeout", "5000"),
)

SCHEMA = """
CREATE TABLE run (
    id               INTEGER PRIMARY KEY,
    state            TEXT    NOT NULL,
    priority         INTEGER NOT NULL,
    created_at       INTEGER NOT NULL,
    lease_owner      TEXT,
    lease_expires_at INTEGER,
    lease_epoch      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE event (
    id      INTEGER PRIMARY KEY,
    run_id  INTEGER NOT NULL REFERENCES run(id),
    seq     INTEGER NOT NULL,
    kind    TEXT    NOT NULL,
    payload TEXT,
    at      INTEGER NOT NULL,
    UNIQUE (run_id, seq)
);

CREATE INDEX idx_run_ready ON run (state, priority, created_at);
"""

# The transaction under test. UPDATE ... RETURNING requires SQLite >= 3.35.
CLAIM_RETURNING = """
UPDATE run
   SET state = 'RUNNING',
       lease_owner = ?,
       lease_expires_at = ?,
       lease_epoch = lease_epoch + 1
 WHERE id = (SELECT id FROM run
              WHERE state = 'READY'
                AND (lease_expires_at IS NULL OR lease_expires_at < ?)
              ORDER BY priority, created_at
              LIMIT 1)
RETURNING id, lease_epoch
"""

# Fallback: SELECT-then-UPDATE inside the SAME BEGIN IMMEDIATE transaction.
CLAIM_SELECT = """
SELECT id FROM run
 WHERE state = 'READY'
   AND (lease_expires_at IS NULL OR lease_expires_at < ?)
 ORDER BY priority, created_at
 LIMIT 1
"""

CLAIM_UPDATE = """
UPDATE run
   SET state = 'RUNNING',
       lease_owner = ?,
       lease_expires_at = ?,
       lease_epoch = lease_epoch + 1
 WHERE id = ? AND state = 'READY'
"""

# Negative control only: the classic broken read-then-write claim, with no
# state re-check. Used with BEGIN DEFERRED to prove the invariant checks below
# can actually detect a double-claim (otherwise "0 duplicates" proves nothing).
CLAIM_UPDATE_UNGUARDED = """
UPDATE run
   SET state = 'RUNNING',
       lease_owner = ?,
       lease_expires_at = ?,
       lease_epoch = lease_epoch + 1
 WHERE id = ?
"""

INSERT_EVENT = """
INSERT INTO event (run_id, seq, kind, payload, at) VALUES (?, ?, 'RUN_CLAIMED', ?, ?)
"""


def apply_pragmas(conn: sqlite3.Connection) -> dict:
    """Apply the design PRAGMAs and return what SQLite actually reports back."""
    applied = {}
    for name, value in PRAGMAS:
        conn.execute(f"PRAGMA {name}={value}")
    for name, _ in PRAGMAS:
        row = conn.execute(f"PRAGMA {name}").fetchone()
        applied[name] = row[0] if row else None
    return applied


def connect(db_path: str, fullfsync: bool = False) -> sqlite3.Connection:
    # isolation_level=None -> no implicit transaction management from Python;
    # every BEGIN/COMMIT below is explicit.
    conn = sqlite3.connect(db_path, isolation_level=None, timeout=5.0)
    apply_pragmas(conn)
    if fullfsync:
        # macOS: plain fsync() only pushes to the drive's write cache. F_FULLFSYNC
        # is what actually forces media. Off by default in SQLite.
        conn.execute("PRAGMA fullfsync=1")
        conn.execute("PRAGMA checkpoint_fullfsync=1")
    return conn


# ----------------------------------------------------------------------------
# worker (top-level so it is picklable under the 'spawn' start method)
# ----------------------------------------------------------------------------


def worker(db_path, worker_id, mode, lease_ms, barrier, out_q, max_retries,
           think_ms=0.0, fullfsync=False, begin="IMMEDIATE", guard=True):
    """One writer PROCESS with its own connection. Loops claiming until empty.

    think_ms simulates the work a real Conductor worker does after claiming a
    run. With think_ms=0 the queue drains faster than processes can interleave,
    so most writers observe an empty queue and never actually contend; a
    non-zero think time is what produces genuine N-way write contention.
    """
    latencies = []          # ms, successful claims only
    claimed = []            # (run_id, returned_lease_epoch)
    iterations = 0          # every BEGIN IMMEDIATE .. COMMIT attempt
    busy_errors = 0
    integrity_errors = 0    # UNIQUE(run_id,seq) violation == duplicate claim
    other_errors = Counter()
    empty_attempts = 0
    fatal = None
    owner = f"worker-{worker_id}-pid{os.getpid()}"

    try:
        conn = connect(db_path, fullfsync=fullfsync)
    except Exception as exc:  # pragma: no cover - connection failure is fatal
        out_q.put({"worker_id": worker_id, "fatal": f"connect: {exc!r}"})
        return

    if barrier is not None:
        barrier.wait()

    t_start = time.perf_counter()
    retries = 0
    try:
        while True:
            now_ms = int(time.time() * 1000)
            iterations += 1
            t0 = time.perf_counter()
            try:
                conn.execute(f"BEGIN {begin}")

                if mode == "returning":
                    rows = conn.execute(
                        CLAIM_RETURNING, (owner, now_ms + lease_ms, now_ms)
                    ).fetchall()
                    if len(rows) > 1:
                        raise RuntimeError(
                            f"claim matched {len(rows)} rows, expected <=1"
                        )
                    got = (rows[0][0], rows[0][1]) if rows else None
                else:  # select-then-update, same transaction
                    sel = conn.execute(CLAIM_SELECT, (now_ms,)).fetchone()
                    if sel is None:
                        got = None
                    else:
                        rid = sel[0]
                        cur = conn.execute(
                            CLAIM_UPDATE if guard else CLAIM_UPDATE_UNGUARDED,
                            (owner, now_ms + lease_ms, rid),
                        )
                        if guard and cur.rowcount != 1:
                            raise RuntimeError(
                                f"select-then-update rowcount={cur.rowcount} for id={rid}"
                            )
                        ep = conn.execute(
                            "SELECT lease_epoch FROM run WHERE id=?", (rid,)
                        ).fetchone()[0]
                        got = (rid, ep)

                if got is None:
                    conn.execute("COMMIT")
                    empty_attempts += 1
                    break

                run_id, epoch = got
                # seq=1 in the real design, so UNIQUE(run_id,seq) is itself a
                # double-claim tripwire. The negative control uses a per-worker
                # seq so a duplicate is *recorded* rather than rejected -- that
                # is what exercises the duplicate_claims counter.
                conn.execute(
                    INSERT_EVENT,
                    (
                        run_id,
                        1 if guard else worker_id + 1,
                        json.dumps({"owner": owner, "epoch": epoch}),
                        now_ms,
                    ),
                )
                conn.execute("COMMIT")
                latencies.append((time.perf_counter() - t0) * 1000.0)
                claimed.append((run_id, epoch))
                retries = 0
                if think_ms:
                    time.sleep(think_ms / 1000.0)  # outside the measured window

            except sqlite3.IntegrityError as exc:
                integrity_errors += 1
                try:
                    conn.execute("ROLLBACK")
                except sqlite3.Error:
                    pass
                other_errors[f"IntegrityError: {exc}"] += 1
                retries += 1
                if retries > max_retries:
                    fatal = f"too many retries after IntegrityError: {exc}"
                    break
            except sqlite3.OperationalError as exc:
                msg = str(exc)
                if "locked" in msg or "busy" in msg:
                    busy_errors += 1
                else:
                    other_errors[f"OperationalError: {msg}"] += 1
                try:
                    conn.execute("ROLLBACK")
                except sqlite3.Error:
                    pass
                retries += 1
                if retries > max_retries:
                    fatal = f"too many retries after OperationalError: {msg}"
                    break
                time.sleep(0.001 * min(retries, 20))
            except Exception as exc:  # anything else is a hard failure
                try:
                    conn.execute("ROLLBACK")
                except sqlite3.Error:
                    pass
                other_errors[f"{type(exc).__name__}: {exc}"] += 1
                fatal = f"{type(exc).__name__}: {exc}"
                break
    finally:
        elapsed = time.perf_counter() - t_start
        try:
            conn.close()
        except sqlite3.Error:
            pass

    out_q.put(
        {
            "worker_id": worker_id,
            "owner": owner,
            "pid": os.getpid(),
            "latencies_ms": latencies,
            "claimed": claimed,
            "iterations": iterations,
            "empty_attempts": empty_attempts,
            "busy_errors": busy_errors,
            "integrity_errors": integrity_errors,
            "other_errors": dict(other_errors),
            "elapsed_s": elapsed,
            "fatal": fatal,
        }
    )


# ----------------------------------------------------------------------------
# setup / verification
# ----------------------------------------------------------------------------


def seed(db_path: str, rows: int, fullfsync: bool = False) -> dict:
    conn = connect(db_path, fullfsync=fullfsync)
    applied = {k: v for k, v in apply_pragmas(conn).items()}
    conn.executescript(SCHEMA)
    conn.execute("BEGIN IMMEDIATE")
    now_ms = int(time.time() * 1000)
    conn.executemany(
        "INSERT INTO run (id, state, priority, created_at, lease_owner, "
        "lease_expires_at, lease_epoch) VALUES (?, 'READY', ?, ?, NULL, NULL, 0)",
        [(i, i % 8, now_ms + i) for i in range(1, rows + 1)],
    )
    conn.execute("COMMIT")
    extra = {
        "page_size": conn.execute("PRAGMA page_size").fetchone()[0],
        "fullfsync": conn.execute("PRAGMA fullfsync").fetchone()[0],
        "checkpoint_fullfsync": conn.execute(
            "PRAGMA checkpoint_fullfsync"
        ).fetchone()[0],
        "wal_autocheckpoint": conn.execute(
            "PRAGMA wal_autocheckpoint"
        ).fetchone()[0],
        "checkpoint_fullfsync_effective": conn.execute(
            "PRAGMA checkpoint_fullfsync"
        ).fetchone()[0],
    }
    conn.close()
    applied.update(extra)
    return applied


def probe_returning(tmpdir: str) -> tuple[bool, str | None]:
    """Verify UPDATE...RETURNING with a subquery behaves, on a scratch db."""
    path = os.path.join(tmpdir, "probe.db")
    try:
        conn = connect(path)
        conn.executescript(SCHEMA)
        conn.execute("BEGIN IMMEDIATE")
        conn.executemany(
            "INSERT INTO run (id, state, priority, created_at) VALUES (?, 'READY', 0, ?)",
            [(1, 1), (2, 2)],
        )
        conn.execute("COMMIT")
        conn.execute("BEGIN IMMEDIATE")
        rows = conn.execute(CLAIM_RETURNING, ("probe", 10**13, 10**12)).fetchall()
        conn.execute("COMMIT")
        ok = len(rows) == 1 and rows[0][1] == 1
        state = conn.execute(
            "SELECT state, lease_owner, lease_epoch FROM run WHERE id=?", (rows[0][0],)
        ).fetchone() if rows else None
        conn.close()
        os.remove(path)
        for suffix in ("-wal", "-shm"):
            if os.path.exists(path + suffix):
                os.remove(path + suffix)
        if not ok:
            return False, f"probe returned rows={rows}"
        if state != ("RUNNING", "probe", 1):
            return False, f"probe row state after claim = {state}"
        return True, None
    except Exception as exc:
        return False, f"{type(exc).__name__}: {exc}"


def verify(db_path: str, rows: int, worker_results: list) -> dict:
    """Check the four invariants. Returns a dict with pass/fail + evidence."""
    conn = connect(db_path)

    all_claimed_ids = []
    all_returned_epochs = []
    for r in worker_results:
        for rid, ep in r["claimed"]:
            all_claimed_ids.append(rid)
            all_returned_epochs.append(ep)

    counts = Counter(all_claimed_ids)
    dupes = {rid: n for rid, n in counts.items() if n > 1}
    total_claims = len(all_claimed_ids)

    # DB-side corroboration
    db_running = conn.execute(
        "SELECT COUNT(*) FROM run WHERE state='RUNNING'"
    ).fetchone()[0]
    db_ready = conn.execute("SELECT COUNT(*) FROM run WHERE state='READY'").fetchone()[0]
    db_total = conn.execute("SELECT COUNT(*) FROM run").fetchone()[0]
    ev_total = conn.execute("SELECT COUNT(*) FROM event").fetchone()[0]
    ev_distinct = conn.execute("SELECT COUNT(DISTINCT run_id) FROM event").fetchone()[0]
    ev_kind_bad = conn.execute(
        "SELECT COUNT(*) FROM event WHERE kind<>'RUN_CLAIMED'"
    ).fetchone()[0]
    orphan_events = conn.execute(
        "SELECT COUNT(*) FROM event e LEFT JOIN run r ON r.id=e.run_id WHERE r.id IS NULL"
    ).fetchone()[0]

    i1_ok = (
        not dupes
        and total_claims == rows
        and set(counts) == set(range(1, rows + 1))
        and db_running == rows
        and db_ready == 0
        and db_total == rows
        and ev_total == rows
        and ev_distinct == rows
        and ev_kind_bad == 0
        and orphan_events == 0
    )

    # I2 partial transition
    partial_running = conn.execute(
        "SELECT COUNT(*) FROM run WHERE state='RUNNING' AND lease_owner IS NULL"
    ).fetchone()[0]
    partial_ready = conn.execute(
        "SELECT COUNT(*) FROM run WHERE state='READY' AND lease_owner IS NOT NULL"
    ).fetchone()[0]
    partial_no_expiry = conn.execute(
        "SELECT COUNT(*) FROM run WHERE state='RUNNING' AND lease_expires_at IS NULL"
    ).fetchone()[0]
    claimed_without_event = conn.execute(
        "SELECT COUNT(*) FROM run r LEFT JOIN event e ON e.run_id=r.id "
        "WHERE r.state='RUNNING' AND e.run_id IS NULL"
    ).fetchone()[0]
    i2_ok = (
        partial_running == 0
        and partial_ready == 0
        and partial_no_expiry == 0
        and claimed_without_event == 0
    )

    # I3 lease_epoch
    epoch_not_one = conn.execute(
        "SELECT COUNT(*) FROM run WHERE lease_epoch <> 1"
    ).fetchone()[0]
    epoch_min, epoch_max = conn.execute(
        "SELECT MIN(lease_epoch), MAX(lease_epoch) FROM run"
    ).fetchone()
    returned_epoch_bad = sum(1 for e in all_returned_epochs if e != 1)
    i3_ok = epoch_not_one == 0 and returned_epoch_bad == 0

    # I4 integrity
    integrity = [r[0] for r in conn.execute("PRAGMA integrity_check").fetchall()]
    fk_violations = conn.execute("PRAGMA foreign_key_check").fetchall()
    i4_ok = integrity == ["ok"] and not fk_violations

    conn.close()

    return {
        "I1_no_duplicate_ownership": {
            "pass": i1_ok,
            "total_claims_recorded_by_workers": total_claims,
            "rows_seeded": rows,
            "duplicate_run_ids": dupes,
            "distinct_ids_claimed": len(counts),
            "db_running": db_running,
            "db_ready": db_ready,
            "db_total": db_total,
            "event_rows": ev_total,
            "event_distinct_run_ids": ev_distinct,
            "event_wrong_kind": ev_kind_bad,
            "orphan_events": orphan_events,
        },
        "I2_no_partial_transition": {
            "pass": i2_ok,
            "running_with_null_lease_owner": partial_running,
            "ready_with_lease_owner": partial_ready,
            "running_with_null_lease_expires_at": partial_no_expiry,
            "running_without_claim_event": claimed_without_event,
        },
        "I3_lease_epoch_exactly_one": {
            "pass": i3_ok,
            "rows_with_epoch_not_1": epoch_not_one,
            "epoch_min": epoch_min,
            "epoch_max": epoch_max,
            "returned_epochs_not_1": returned_epoch_bad,
        },
        "I4_integrity_check": {
            "pass": i4_ok,
            "integrity_check": integrity,
            "foreign_key_check_violations": len(fk_violations),
        },
        "all_pass": i1_ok and i2_ok and i3_ok and i4_ok,
    }


# ----------------------------------------------------------------------------
# stats
# ----------------------------------------------------------------------------


def self_test(tmpdir):
    """Deterministically corrupt a DB and confirm verify() flags every invariant.

    Needed because the concurrency negative control cannot produce a duplicate:
    in WAL mode a BEGIN DEFERRED read-then-write simply fails to upgrade
    (SQLITE_BUSY) rather than losing an update. So the duplicate/partial/epoch
    detectors are validated here directly instead.
    """
    rows = 10
    path = os.path.join(tmpdir, "selftest.db")
    seed(path, rows)
    conn = connect(path)
    conn.execute("BEGIN IMMEDIATE")
    # rows 1..8 claimed normally
    for rid in range(1, 9):
        conn.execute(
            "UPDATE run SET state='RUNNING', lease_owner='w', lease_expires_at=1,"
            " lease_epoch=1 WHERE id=?", (rid,))
        conn.execute(INSERT_EVENT, (rid, 1, "{}", 0))
    # row 9: RUNNING but NULL lease_owner  -> I2 must fail
    conn.execute("UPDATE run SET state='RUNNING', lease_owner=NULL,"
                 " lease_expires_at=NULL, lease_epoch=1 WHERE id=9")
    conn.execute(INSERT_EVENT, (9, 1, "{}", 0))
    # row 10: claimed twice -> lease_epoch 2, I3 must fail
    conn.execute("UPDATE run SET state='RUNNING', lease_owner='w',"
                 " lease_expires_at=1, lease_epoch=2 WHERE id=10")
    conn.execute(INSERT_EVENT, (10, 1, "{}", 0))
    conn.execute("COMMIT")
    conn.close()

    # worker records claiming row 10 twice -> I1 must fail
    fake = [
        {"worker_id": 0, "claimed": [(i, 1) for i in range(1, 10)] + [(10, 2)]},
        {"worker_id": 1, "claimed": [(10, 2)]},
    ]
    inv = verify(path, rows, fake)
    checks = {
        "I1 detects duplicate ownership": inv["I1_no_duplicate_ownership"]["pass"] is False
        and inv["I1_no_duplicate_ownership"]["duplicate_run_ids"] == {10: 2},
        "I2 detects partial transition": inv["I2_no_partial_transition"]["pass"] is False
        and inv["I2_no_partial_transition"]["running_with_null_lease_owner"] == 1,
        "I3 detects epoch != 1": inv["I3_lease_epoch_exactly_one"]["pass"] is False
        and inv["I3_lease_epoch_exactly_one"]["rows_with_epoch_not_1"] == 1,
        "I4 reports ok on a structurally sound db":
            inv["I4_integrity_check"]["pass"] is True,
        "all_pass is False": inv["all_pass"] is False,
    }
    print("\nSELF-TEST of invariant checkers (deliberately corrupted state):")
    for name, ok in checks.items():
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(path + suffix)
        except FileNotFoundError:
            pass
    return all(checks.values())


def percentile(values, p):
    """Linear-interpolation percentile (same convention as numpy default)."""
    if not values:
        return None
    s = sorted(values)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * (p / 100.0)
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return s[int(k)]
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def summarize(latencies):
    if not latencies:
        return {"count": 0}
    return {
        "count": len(latencies),
        "min_ms": min(latencies),
        "median_ms": statistics.median(latencies),
        "mean_ms": statistics.fmean(latencies),
        "p95_ms": percentile(latencies, 95),
        "p99_ms": percentile(latencies, 99),
        "max_ms": max(latencies),
    }


# ----------------------------------------------------------------------------
# one experiment run
# ----------------------------------------------------------------------------


def run_once(tmpdir, writers, rows, mode, lease_ms, repeat_idx, max_retries, keep,
             think_ms=0.0, fullfsync=False, begin="IMMEDIATE", guard=True):
    db_path = os.path.join(tmpdir, f"claim_w{writers}_r{repeat_idx}.db")
    pragmas = seed(db_path, rows, fullfsync=fullfsync)

    ctx = mp.get_context("spawn")
    out_q = ctx.Queue()
    barrier = ctx.Barrier(writers)
    procs = [
        ctx.Process(
            target=worker,
            args=(db_path, i, mode, lease_ms, barrier, out_q, max_retries,
                  think_ms, fullfsync, begin, guard),
            daemon=False,
        )
        for i in range(writers)
    ]

    t0 = time.perf_counter()
    for p in procs:
        p.start()
    results = [out_q.get() for _ in procs]  # drain before join to avoid deadlock
    for p in procs:
        p.join(timeout=120)
    wall = time.perf_counter() - t0

    exitcodes = [p.exitcode for p in procs]
    fatals = [r.get("fatal") for r in results if r.get("fatal")]

    inv = verify(db_path, rows, results)

    latencies = [x for r in results for x in r["latencies_ms"]]
    other_errors = Counter()
    for r in results:
        other_errors.update(r.get("other_errors", {}))

    per_worker = {r["worker_id"]: len(r["claimed"]) for r in results}
    active = sum(1 for v in per_worker.values() if v > 0)

    run = {
        "writers": writers,
        "repeat": repeat_idx,
        "rows": rows,
        "mode": mode,
        "think_ms": think_ms,
        "fullfsync": fullfsync,
        "active_writers": active,
        "idle_writers": writers - active,
        "max_worker_share": (max(per_worker.values()) / rows) if rows else None,
        "iterations": sum(r["iterations"] for r in results),
        "claims": len(latencies),
        "empty_attempts": sum(r["empty_attempts"] for r in results),
        "busy_errors": sum(r["busy_errors"] for r in results),
        "integrity_errors": sum(r["integrity_errors"] for r in results),
        "other_errors": dict(other_errors),
        "duplicate_claims": sum(
            n - 1 for n in Counter(
                rid for r in results for rid, _ in r["claimed"]
            ).values() if n > 1
        ),
        "wall_s": wall,
        "throughput_claims_per_s": (len(latencies) / wall) if wall > 0 else None,
        "latency": summarize(latencies),
        "per_worker_claims": per_worker,
        "worker_exitcodes": exitcodes,
        "fatals": fatals,
        "invariants": inv,
        "pragmas": pragmas,
    }

    if not keep:
        for suffix in ("", "-wal", "-shm"):
            try:
                os.remove(db_path + suffix)
            except FileNotFoundError:
                pass

    return run, latencies


# ----------------------------------------------------------------------------
# main
# ----------------------------------------------------------------------------


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--writers", default="1,4,16",
                    help="comma-separated writer-process counts (default 1,4,16)")
    ap.add_argument("--rows", type=int, default=2000,
                    help="READY rows seeded per run (default 2000)")
    ap.add_argument("--repeat", type=int, default=3,
                    help="repetitions per writer count (default 3)")
    ap.add_argument("--mode", choices=("auto", "returning", "select-then-update"),
                    default="auto",
                    help="claim implementation (default auto: probe RETURNING, fall back)")
    ap.add_argument("--lease-ms", type=int, default=30000,
                    help="lease duration in ms (default 30000)")
    ap.add_argument("--max-retries", type=int, default=200,
                    help="consecutive retries per worker before giving up (default 200)")
    ap.add_argument("--think-ms", type=float, default=0.0,
                    help="simulated per-run work after each successful claim, in ms "
                         "(default 0). NOT included in measured latency. With 0 the "
                         "queue drains before slower processes interleave, so most "
                         "writers never contend; use >0 to measure true N-way "
                         "write contention.")
    ap.add_argument("--fullfsync", action="store_true",
                    help="also set PRAGMA fullfsync=1 / checkpoint_fullfsync=1. On "
                         "macOS, plain fsync() only reaches the drive write cache; "
                         "this forces media and shows the true durability cost.")
    ap.add_argument("--self-test", action="store_true",
                    help="validate the invariant checkers against deliberately "
                         "corrupted state, then exit")
    ap.add_argument("--negative-control", action="store_true",
                    help="INSTRUMENT VALIDATION: run the known-broken claim "
                         "(BEGIN DEFERRED + select-then-update with no state "
                         "re-check). Expected to FAIL the invariants; if it "
                         "passes, the invariant checks have no teeth.")
    ap.add_argument("--keep-temp", action="store_true",
                    help="do not delete the temp directory (debugging)")
    ap.add_argument("--out", default=None,
                    help="JSON output path (default scripts/measure/results/"
                         "s0_sqlite_claim_latency.json next to this script)")
    args = ap.parse_args(argv)

    writer_counts = [int(w) for w in args.writers.split(",") if w.strip()]
    here = os.path.dirname(os.path.abspath(__file__))
    out_path = args.out or os.path.join(
        here, "results", "s0_sqlite_claim_latency.json"
    )

    tmpdir = tempfile.mkdtemp(prefix="conductor_s0_")
    print(f"temp database directory: {tmpdir}")
    print("(disposable; no existing database is touched)")

    started = time.time()
    try:
        if args.self_test:
            ok = self_test(tmpdir)
            print(f"\nself-test: {'PASS -- checkers have teeth' if ok else 'FAIL'}")
            return 0 if ok else 1

        # decide claim implementation
        probe_ok, probe_err = probe_returning(tmpdir)
        if args.mode == "auto":
            mode = "returning" if probe_ok else "select-then-update"
        else:
            mode = args.mode
        begin, guard = "IMMEDIATE", True
        if args.negative_control:
            mode, begin, guard = "select-then-update", "DEFERRED", False
            print("*** NEGATIVE CONTROL: broken claim on purpose; "
                  "invariants are EXPECTED to fail ***")
        print(f"UPDATE...RETURNING probe: {'ok' if probe_ok else 'FAILED: ' + str(probe_err)}")
        print(f"claim mode: {mode} / BEGIN {begin} / state-guard={guard}")

        meta = {
            "script": os.path.abspath(__file__),
            "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(started)),
            "python": sys.version.split()[0],
            "sqlite_library_version": sqlite3.sqlite_version,
            "platform": platform.platform(),
            "machine": platform.machine(),
            "cpu_count": os.cpu_count(),
            "mp_start_method": "spawn",
            "temp_dir": tmpdir,
            "rows": args.rows,
            "repeat": args.repeat,
            "writer_counts": writer_counts,
            "lease_ms": args.lease_ms,
            "max_retries": args.max_retries,
            "think_ms": args.think_ms,
            "fullfsync": args.fullfsync,
            "claim_mode": mode,
            "begin_mode": begin,
            "state_guard": guard,
            "negative_control": args.negative_control,
            "returning_probe_ok": probe_ok,
            "returning_probe_error": probe_err,
            "pragmas_requested": dict(PRAGMAS),
            "percentile_method": "linear interpolation on sorted successful-claim latencies",
            "latency_definition": "wall time of BEGIN IMMEDIATE -> claim -> INSERT event -> COMMIT, successful claims only",
        }

        runs = []
        pooled = {}
        for w in writer_counts:
            pooled[w] = []
            for rep in range(args.repeat):
                print(f"  running writers={w} repeat={rep + 1}/{args.repeat} ...",
                      flush=True)
                run, lats = run_once(
                    tmpdir, w, args.rows, mode, args.lease_ms, rep,
                    args.max_retries, args.keep_temp,
                    think_ms=args.think_ms, fullfsync=args.fullfsync,
                    begin=begin, guard=guard,
                )
                runs.append(run)
                pooled[w].extend(lats)
                inv = run["invariants"]
                if not inv["all_pass"]:
                    print("    *** INVARIANT FAILURE ***")
                    print(json.dumps(inv, indent=2))

        summary = []
        for w in writer_counts:
            rs = [r for r in runs if r["writers"] == w]
            other = Counter()
            for r in rs:
                other.update(r["other_errors"])
            summary.append({
                "writers": w,
                "repeats": len(rs),
                "iterations": sum(r["iterations"] for r in rs),
                "claims": sum(r["claims"] for r in rs),
                "busy_errors": sum(r["busy_errors"] for r in rs),
                "integrity_errors": sum(r["integrity_errors"] for r in rs),
                "duplicate_claims": sum(r["duplicate_claims"] for r in rs),
                "other_error_count": sum(other.values()),
                "other_errors": dict(other),
                "latency": summarize(pooled[w]),
                "active_writers_mean": statistics.fmean(
                    [r["active_writers"] for r in rs]) if rs else None,
                "max_worker_share_mean": statistics.fmean(
                    [r["max_worker_share"] for r in rs]) if rs else None,
                "throughput_claims_per_s_mean": statistics.fmean(
                    [r["throughput_claims_per_s"] for r in rs]
                ) if rs else None,
                "invariants_all_pass": all(r["invariants"]["all_pass"] for r in rs),
                "invariant_failures": [
                    {"repeat": r["repeat"], "invariants": r["invariants"]}
                    for r in rs if not r["invariants"]["all_pass"]
                ],
            })

        payload = {"meta": meta, "summary": summary, "runs": runs}
        os.makedirs(os.path.dirname(out_path), exist_ok=True)
        with open(out_path, "w") as fh:
            json.dump(payload, fh, indent=2, sort_keys=False)

        print_table(summary, runs)
        print(f"\nJSON written to: {out_path}")

        all_ok = all(s["invariants_all_pass"] for s in summary)
        if args.negative_control:
            dupes = sum(s["duplicate_claims"] for s in summary)
            integ = sum(s["integrity_errors"] for s in summary)
            busy = sum(s["busy_errors"] for s in summary)
            detected = (not all_ok) or dupes or integ
            print(f"\nNEGATIVE CONTROL: invariants_all_pass={all_ok} "
                  f"duplicate_claims={dupes} integrity_errors={integ} "
                  f"busy_errors={busy}")
            if detected:
                print("  -> the broken claim produced observable corruption.")
            elif busy:
                print("  -> no corruption, but "
                      f"{busy} SQLITE_BUSY errors: WAL refused the read-then-write "
                      "upgrade rather than losing an update. The race is blocked by "
                      "WAL itself, at a large retry cost. Checker teeth are proven "
                      "separately by --self-test.")
            else:
                print("  -> WARNING: broken claim produced neither corruption nor "
                      "busy errors; this control is not exercising anything.")
            return 0 if (detected or busy) else 1
        if not all_ok:
            print("\n!!! CORRECTNESS INVARIANT FAILURE -- see JSON for evidence !!!")
        return 0 if all_ok else 1
    finally:
        if args.keep_temp:
            print(f"temp dir kept at: {tmpdir}")
        else:
            shutil.rmtree(tmpdir, ignore_errors=True)
            print(f"temp dir removed: {tmpdir}")


def print_table(summary, runs):
    def f(x, nd=3):
        return "-" if x is None else f"{x:.{nd}f}"

    print()
    print("=" * 112)
    print("S0: SQLite BEGIN IMMEDIATE atomic claim -- latency & correctness")
    print("=" * 112)
    hdr = (f"{'writers':>7} {'act':>4} {'iters':>7} {'claims':>7} {'median':>9} "
           f"{'p95':>9} {'p99':>9} {'max':>9} {'busy':>6} {'dupes':>6} "
           f"{'errs':>6} {'claims/s':>10} {'inv':>5}")
    print(hdr)
    print("-" * 112)
    for s in summary:
        lat = s["latency"]
        print(f"{s['writers']:>7} {f(s.get('active_writers_mean'), 1):>4} "
              f"{s['iterations']:>7} {s['claims']:>7} "
              f"{f(lat.get('median_ms')):>9} {f(lat.get('p95_ms')):>9} "
              f"{f(lat.get('p99_ms')):>9} {f(lat.get('max_ms')):>9} "
              f"{s['busy_errors']:>6} {s['duplicate_claims']:>6} "
              f"{s['other_error_count'] + s['integrity_errors']:>6} "
              f"{f(s['throughput_claims_per_s_mean'], 1):>10} "
              f"{'PASS' if s['invariants_all_pass'] else 'FAIL':>5}")
    print("-" * 112)
    print("latency in ms; iters = total BEGIN IMMEDIATE attempts (incl. the terminal empty one)")
    print("busy = 'database is locked'/busy OperationalError count; dupes = rows claimed more than once")
    print("act = mean number of writers that claimed >=1 row. If act << writers the")
    print("      queue drained before the others contended -- latency is then NOT")
    print("      representative of that writer count. Raise --think-ms.")
    print()
    print("Per-run invariants:")
    for r in runs:
        inv = r["invariants"]
        flags = " ".join(
            f"{k.split('_')[0]}={'ok' if v['pass'] else 'FAIL'}"
            for k, v in inv.items() if k != "all_pass"
        )
        print(f"  writers={r['writers']:<3} repeat={r['repeat']}  {flags}  "
              f"claims={r['claims']}/{r['rows']}  "
              f"integrity_check={inv['I4_integrity_check']['integrity_check']}  "
              f"epoch_range=[{inv['I3_lease_epoch_exactly_one']['epoch_min']},"
              f"{inv['I3_lease_epoch_exactly_one']['epoch_max']}]  "
              f"exitcodes={r['worker_exitcodes']}")


if __name__ == "__main__":
    sys.exit(main())
