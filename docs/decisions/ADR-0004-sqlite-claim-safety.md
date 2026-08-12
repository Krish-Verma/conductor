# ADR-0004 — `BEGIN IMMEDIATE` single-statement claim is the run-claim mechanism

**Status:** ACCEPTED
**Date:** 2026-08-12
**Slice:** S0, Question B

---

## Question

Is the proposed atomic run-claim transaction — a single `UPDATE … RETURNING` inside
`BEGIN IMMEDIATE`, with the `RUN_CLAIMED` event written in the same transaction —
**correct** under concurrency, and fast enough to matter?

Correctness first. Speed is secondary and was never the risk.

## Why the answer matters to Conductor

The claim transaction is the one place where two workers could take ownership of the
same run. A duplicate claim means two agents in the same workspace, two lease epochs
believing they are current, and a reconciliation that cannot attribute changes. The
master plan names this as a transaction boundary that must be atomic (§4.7) and treats
it as the foundation the whole runtime stands on.

## Experiment / evidence

`scripts/measure/s0_sqlite_claim_latency.py`. Disposable temp database, `multiprocessing`
with **separate processes and separate connections** (not threads), explicit
`BEGIN IMMEDIATE` with `isolation_level=None`. Four configurations plus an instrument
self-test.

Four invariants checked after every run:

| | invariant |
|---|---|
| I1 | no duplicate ownership — every seeded row claimed exactly once, total claims == rows |
| I2 | no partial transition — no row `RUNNING` with a NULL owner, none `READY` with an owner |
| I3 | `lease_epoch` incremented exactly once per claim |
| I4 | `PRAGMA integrity_check` returns `ok` |

## Observed result

All latencies in ms. `dup` = rows claimed more than once. `busy` = "database is locked".

**Baseline — `think_ms=0`, `fullfsync=0`, 2000 rows × 3 repeats**

| writers | iters | claims | median | p95 | p99 | max | busy | dup | inv |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 6003 | 6000 | 0.065 | 0.126 | 0.218 | 2.095 | 0 | **0** | PASS |
| 4 | 6012 | 6000 | 0.064 | 0.150 | 0.317 | 2.535 | 0 | **0** | PASS |
| 16 | 6048 | 6000 | 0.066 | 0.144 | 0.295 | 118.5 | 0 | **0** | PASS |

**Contended — `think_ms=1`, `fullfsync=0`** (holds the claim long enough that writers actually overlap)

| writers | claims | median | p95 | p99 | max | busy | dup | inv |
|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 6000 | 0.079 | 0.154 | 0.338 | 6.77 | 0 | **0** | PASS |
| 4 | 6000 | 0.139 | 3.841 | 9.928 | 115.7 | 0 | **0** | PASS |
| 16 | 6000 | 1.333 | 10.02 | 91.30 | 677.3 | 0 | **0** | PASS |

**True durability — `fullfsync=1`, `think_ms=1`, 300 rows × 2 repeats**

| writers | claims | median | p95 | p99 | max | busy | dup | inv |
|---:|---:|---:|---:|---:|---:|---:|---:|---|
| 1 | 600 | 2.733 | 3.176 | 3.822 | 7.54 | 0 | **0** | PASS |
| 4 | 600 | 2.780 | 42.73 | 116.8 | 777.6 | 0 | **0** | PASS |
| 16 | 600 | 3.626 | 151.4 | 778.1 | 1093 | 0 | **0** | PASS |

**Zero duplicate claims, zero busy errors, zero integrity failures, and
`claims == rows` exactly, in every configuration at every writer count.**

**Instrument validation — `--self-test`**, which deliberately corrupts database state
and asserts each checker fires:

```
[PASS] I1 detects duplicate ownership
[PASS] I2 detects partial transition
[PASS] I3 detects epoch != 1
[PASS] I4 reports ok on a structurally sound db
self-test: PASS -- checkers have teeth
```

**Negative control — `BEGIN DEFERRED`, state guard removed, 16 writers.** This did
**not** produce duplicates. It produced **1264 busy errors** and still 0 duplicates.
WAL refuses to upgrade a deferred read-then-write snapshot rather than losing an
update, so the weakened configuration degraded into lock contention, not into
incorrectness. Recorded because it is the opposite of what a negative control is
supposed to show: the *self-test*, not the negative control, is what establishes the
detector works.

**Zero busy errors is not evidence of low contention.** SQLite's busy handler backs
off exponentially and `busy_timeout=5000` absorbs waiting into *latency* rather than
surfacing it as an error. An incumbent writer in a tight loop starves the others: max
latency reached 118 ms (baseline, 16w), 677 ms (contended, 16w) and 1093 ms
(fullfsync, 16w) while the busy counter stayed at 0. Two consequences the design must
respect:

- Any per-claim budget must be stated as **p99, never median** — the median hides the
  entire contention story here.
- **Lease duration must exceed the worst-case starvation window.** The master plan's
  60 s lease (§4.7) clears the measured 1.1 s worst case by a wide margin, so this is
  currently safe — but it is a real coupling between two numbers that were chosen
  independently, and it should stay explicit.

## Decision

1. **Adopt the mechanism as specified.** `BEGIN IMMEDIATE` + single-statement
   `UPDATE … RETURNING` + same-transaction event insert. `UPDATE … RETURNING` with a
   subquery worked on this build; no fallback to SELECT-then-UPDATE was needed.
2. **Latency is not a constraint at Conductor's real load.** Conductor claims a run
   when a run starts — a few times per hour, not thousands per second — with 1–4
   concurrent runs. Even the worst measured configuration is orders of magnitude
   inside that budget.
3. **`synchronous=FULL` alone is amended to `synchronous=FULL` + `fullfsync=1`** on
   macOS. See the master-plan delta below. This is the substantive change this ADR
   forces.
4. **Every future concurrency harness carries a self-test.** The negative control here
   was uninformative; the self-test was not. An invariant checker that has never been
   shown to fail is not evidence.

## What this DOES prove

- On this build, the claim transaction gave zero duplicate ownership across 18,600+
  claims spanning 1, 4 and 16 concurrent writer processes and four configurations.
- The four correctness invariants hold, and the checkers that assert them have been
  independently shown to fire on corrupted state.
- `UPDATE … RETURNING` with a subquery is usable for the claim.
- The measurement is reproducible: it was re-run independently and reproduced.

## What this DOES NOT prove

- **It does not measure the stack Conductor will ship.** Python 3.14.6 bundles
  **SQLite 3.53.3**; the system CLI is **3.51.0**; `rusqlite` with the `bundled`
  feature will link a third version. Locking semantics are stable across these, but
  this is a measurement of SQLite-via-Python, not of `rusqlite`. **S1 must re-measure
  once the real store exists.**
- The baseline configuration is **not representative at high writer counts**: with
  `think_ms=0` the queue drained before most writers contended (the harness reports a
  mean of ~4 of 16 writers ever claiming a row). Only the contended and fullfsync runs
  describe genuine 16-way contention. Quoting the baseline 16-writer number as a
  contention result would be wrong.
- It does not prove behaviour under a writer that **crashes mid-transaction** — that
  is S1's kill-restart test, not this one.
- It says nothing about lease expiry, fencing-epoch conflicts, or recovery. Those are
  S3.
- It does not prove absence of duplicates in general, only across the volume tested.
  Absence of an observed failure is weak evidence for a concurrency property; the
  design still relies on SQLite's write-lock semantics being correct, not on this
  measurement.

## Pre-registered falsification / revisit trigger

1. **S1 re-measurement under `rusqlite` disagrees** with these numbers by more than
   2× at the median, or produces any duplicate claim. Any duplicate claim at any
   concurrency invalidates the mechanism outright, not just the numbers.
2. **p99 claim latency exceeds 100 ms at ≤4 concurrent writers with `fullfsync=1`.**
   Already marginal — measured 116.8 ms at 4 writers. See delta (2) below.
3. Any observed `busy` error in normal operation, which would mean `busy_timeout=5000`
   is being exhausted and the single-writer assumption is wrong.
4. A future need for more than one writing process (a second daemon, or CLI writes
   racing the daemon), which changes the contention model this was measured under.

## Master-plan delta forced by this ADR

**(1) Part 5.1 pragma block — `synchronous=FULL` is insufficient on macOS.**

The plan justifies `synchronous=FULL` as "this database is the recovery record, and a
lost commit on power failure is exactly the case it exists for". On macOS that
reasoning is not satisfied by `synchronous=FULL` alone: it issues `fsync()`, which does
not flush the drive's write cache. `PRAGMA fullfsync=1` issues `F_FULLSYNC`, which does.

The measured cost of actually honouring the plan's stated intent is **median 0.065 ms
→ 2.733 ms**, roughly 40×. At Conductor's claim rate that is irrelevant, and the plan's
own justification demands it. Amend Part 5.1 to:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = FULL;
PRAGMA fullfsync    = 1;              -- macOS: F_FULLSYNC. FULL alone does not
PRAGMA checkpoint_fullfsync = 1;      -- flush the drive write cache.
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
```

**(2) §2.6 revisit trigger 3 is unscoped and would misfire.**

It currently reads "p99 claim latency > 100 ms". Under `fullfsync=1` that is exceeded
at **4 writers (116.8 ms)** and massively at 16 (778 ms) — so as written the trigger
fires today, at a concurrency Conductor will never reach, and would push toward
adopting Temporal for no reason. Amend to name the durability mode and the realistic
concurrency:

> p99 claim latency > 100 ms **at ≤4 concurrent writers with `fullfsync=1`, measured
> under `rusqlite`** — currently 116.8 ms under Python/SQLite 3.53.3, to be re-measured
> at S1.

**(3) Part 0 — add measured facts M18–M20** (claim correctness, fullfsync cost,
baseline non-representativeness).

## Impacted master-plan sections

Part 0 · Part 5.1 (pragmas) · §2.6 (revisit triggers) · §4.7 (claim transaction) · slice S1.

## Reproduce

```bash
python3 scripts/measure/s0_sqlite_claim_latency.py --self-test
python3 scripts/measure/s0_sqlite_claim_latency.py --writers 1,4,16 --rows 2000 --repeat 3
python3 scripts/measure/s0_sqlite_claim_latency.py --writers 1,4,16 --rows 2000 --repeat 3 \
        --think-ms 1 --out scripts/measure/results/s0_sqlite_claim_latency_contended.json
python3 scripts/measure/s0_sqlite_claim_latency.py --writers 1,4,16 --rows 300 --repeat 2 \
        --think-ms 1 --fullfsync --out scripts/measure/results/s0_sqlite_claim_latency_fullfsync.json
python3 scripts/measure/s0_sqlite_claim_latency.py --negative-control --writers 16 --rows 500 --repeat 2 \
        --think-ms 1 --out scripts/measure/results/s0_sqlite_claim_latency_negative_control.json
```
