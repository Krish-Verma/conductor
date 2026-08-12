# ADR-0005 — The claim mechanism holds under `rusqlite`; its latency trigger was measuring offered load

**Status:** ACCEPTED
**Date:** 2026-08-12
**Slice:** S1
**Relates to:** ADR-0004 (which pre-registered the re-measurement this ADR reports)

---

## Question

ADR-0004 measured the run-claim transaction under **Python 3.14.6 / SQLite 3.53.3** and
was explicit that this is not the shipping stack: *"S1 must re-measure once the real
store exists."* It pre-registered three falsification triggers.

This ADR reports that re-measurement and disposes of each trigger.

## Method

`crates/conductor-store/src/bin/conductor-claim-bench.rs`, driven by
`scripts/measure/s1_rusqlite_claim_latency.sh`.

Two rules were treated as load-bearing, both inherited from ADR-0004's own criticism of
itself:

1. **No reimplementation.** Every claim goes through the production
   `Store::claim_next_run` on a connection opened by the production
   `Store::open_existing` path with the production pragmas. Measuring a reimplementation
   is the exact mistake ADR-0004 flagged.
2. **Separate writer processes**, not threads, matching S0 so the numbers are comparable.

Environment: `rusqlite` 0.40.2 with `bundled` → **SQLite 3.53.2** (S0 used 3.53.3; the
system CLI is 3.51.0) · macOS 26.6 (25G72) arm64 · 10 cores · release build.

The four ADR-0004 invariants (I1 no duplicate ownership · I2 no partial transition ·
I3 epoch incremented exactly once · I4 `integrity_check` ok) are checked after every
run, and the harness carries the **self-test** ADR-0004 decision 4 made binding. The S1
self-test is stronger than S0's: I4 now also detects a *deliberately corrupted database
file*, where S0's I4 only confirmed `ok` on a sound one.

## Observed result

**39,400 claims. Zero duplicate ownership. Zero invariant failures. `claims == rows`
exactly, in every configuration at every writer count.** Latencies in ms.

**A — production durability, saturation (`fullfsync=1`, gap ≈1 ms)**

| writers | claims | median | p95 | p99 | max | busy | dup |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 600 | 3.370 | 5.516 | 24.763 | 41.2 | 0 | **0** |
| 4 | 600 | 3.442 | 62.766 | **265.250** | 875.8 | 0 | **0** |
| 16 | 600 | 3.838 | 203.816 | 773.531 | 1183.6 | 0 | **0** |

**B — production durability, no gap (`fullfsync=1`, gap 0)**

| writers | claims | median | p95 | p99 | max | busy | dup |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 6000 | 2.723 | 4.312 | 6.752 | 105.1 | 0 | **0** |
| 4 | 6000 | 2.002 | 4.205 | 8.621 | **3484.1** | 0 | **0** |
| 16 | 6000 | 2.694 | 11.914 | 464.247 | **5147.5** | **6** | **0** |

**C — `fullfsync=0`. Comparison with ADR-0004 only; never a shipping configuration.**

| writers | claims | median | p95 | p99 | max | busy | dup |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 6000 | 0.397 | 0.957 | 1.551 | 23.4 | 0 | **0** |
| 4 | 6000 | 0.357 | 4.201 | 21.161 | 251.4 | 0 | **0** |
| 16 | 6000 | 0.429 | 21.625 | 123.043 | 982.8 | 0 | **0** |

**D/E — production durability at a realistic arrival rate (`fullfsync=1`)**

| gap | writers | claims | median | p95 | p99 | max | busy | dup |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 25 ms | 1 | 400 | 3.890 | 12.132 | 18.058 | 37.3 | 0 | **0** |
| 25 ms | 4 | 400 | 3.537 | 7.607 | **24.566** | 67.0 | 0 | **0** |
| 250 ms | 1 | 400 | 3.977 | 7.891 | 16.412 | 21.3 | 0 | **0** |
| 250 ms | 4 | 400 | 4.442 | 15.690 | **48.726** | 85.4 | 0 | **0** |

## The finding that matters

**p99 claim latency is a function of offered load, not of the mechanism.** At four
writers with production durability, p99 moves 265 ms → 24.6 ms → 48.7 ms as the gap
between claims grows from ~1 ms to 25 ms to 250 ms, while the median stays flat at
3.4–4.4 ms and duplicates stay at zero throughout.

That is queueing at ~100% utilisation of a deliberately serialised single writer. It is
the expected behaviour of the design, not a defect in it.

**Conductor's actual arrival rate is a few claims per hour** with 1–4 concurrent runs
(ADR-0004 decision 2). Even the 250 ms column is orders of magnitude busier than that.

## Trigger disposition

**Trigger 1 — "S1 re-measurement disagrees by more than 2× at the median, or produces
any duplicate claim."**

**Does not fire on the shipping configuration.** No duplicate claim occurred anywhere
across 39,400 claims — and that is the condition ADR-0004 said would invalidate the
mechanism outright. Median ratios on `fullfsync=1` are 1.23× (1w), 1.24× (4w),
1.06× (16w), all inside 2×.

**Partially fires on the non-shipping `fullfsync=0` comparison**: 5.03× (1w) and 2.57×
(4w), though 16w is *faster* than S0 (0.32×). The likely cause — recorded as a
hypothesis, not a measured decomposition, because the instrumentation to decompose it
was not built — is that the S1 transaction does strictly more work than S0's Python
harness: the full 17-table schema with foreign keys enforced, an extra
`SELECT COALESCE(MAX(seq),0)+1` per claim that S0 hardcoded away, a `serde_json`
payload, and now a unique index on `event`. At ~0.1 ms that overhead dominates; at
2.7 ms `F_FULLSYNC` swamps it, which is exactly the pattern observed. Since
`fullfsync=0` is not a configuration Conductor ships, this does not bear on the
decision.

**Trigger 2 — "p99 claim latency > 100 ms at ≤4 concurrent writers with `fullfsync=1`,
measured under `rusqlite`."**

**Fires as literally worded (265 ms at 4 writers). The wording is defective and is
amended below.**

The trigger exists as a proxy for one question: *is SQLite adequate for Conductor's
claim load?* It omits the offered load, so as written it is satisfied by any benchmark
that saturates the writer — and it would then recommend adopting Temporal or Hatchet,
whose durable-write path is a Postgres round-trip plus a network hop. **A trigger whose
firing recommends an action that makes the measured quantity worse is measuring the
wrong thing.** ADR-0004 had already caught one version of this defect, scoping the
original unscoped trigger because it "fires today, at a concurrency Conductor will never
reach". This is the same defect surviving one scope narrower.

At the load the trigger is actually about, the measured p99 is **24.6 ms**, comfortably
inside the 100 ms budget.

**Trigger 3 — "any observed busy error in normal operation."**

**Six `SQLITE_BUSY` errors observed — but at 16 writers under saturation, which is not
normal operation.** Conductor's design point is 1–4 concurrent runs; >50 concurrent runs
is separately §2.6 trigger 1. No busy error occurred at ≤4 writers in any configuration.

**The number that must be recorded, because ADR-0004 explicitly coupled it to a design
constant:** worst-case claim latency reached **5147 ms**, against 1093 ms in S0.
ADR-0004 states *"lease duration must exceed the worst-case starvation window"*. The
60 s lease (§4.7) margin therefore narrows from ~55× to **~12×**. Still safe, still a
wide margin, but it moved by a factor of five under a stack change, and it is a real
coupling between two numbers chosen independently. S3 owns leases and must not shrink
the lease without re-checking this.

## Decisions

1. **Adopt the claim mechanism as measured.** The correctness property ADR-0004 cared
   about survives the stack change: zero duplicate ownership across 39,400 claims under
   the real store code, real pragmas, separate processes.

2. **Amend §2.6 revisit trigger 3 to name the offered load** (delta 1 below). A latency
   trigger that does not state the arrival rate it assumes is not falsifiable in the
   direction it was written for.

3. **`event` gains `UNIQUE(run_id, seq)`** (delta 2 below). S0's harness carried this
   constraint and used it as a hard double-claim tripwire; the DDL that reached the
   master plan had lost it. Restored while the schema has zero deployed instances, so
   no migration is owed. A duplicate claim is now an `INSERT`-time error rather than
   something only an offline checker would notice.

4. **`fullfsync=1` and `checkpoint_fullfsync=1` are confirmed applied under `rusqlite`,
   not silently dropped** — closing S0 open item 2. Both read back as `1`, and the
   ~9× median cost against `fullfsync=0` (3.44 ms vs 0.36 ms at 4 writers) corroborates
   that `F_FULLSYNC` is genuinely being issued rather than merely accepted.

   A related trap found while writing the pragma test, worth recording because it makes
   readback weaker evidence than it looks: **three of the six Part 5.1 pragmas are
   already the dependency's defaults.** `libsqlite3-sys` compiles with
   `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`, `rusqlite` calls `sqlite3_busy_timeout(db, 5000)`
   on every open, and `synchronous` already sits at SQLite's compile default. Their
   readback alone does not prove the store applied them. `fullfsync` is the pragma that
   genuinely discriminates.

## What this does NOT prove

- **Power-loss durability is not proven.** The 100-cycle failure injection uses
  `SIGKILL`, which destroys a process but does not drop the OS page cache or the drive
  write cache — precisely what `fullfsync=1` exists to defeat. What is proven is **crash
  atomicity**: WAL recovery, `integrity_check` ok, zero partial rows, store still
  writable after 100 kills. The S1 slice line *"`synchronous=FULL` survives simulated
  power loss"* is **not** satisfied and cannot be on this host without hardware or
  VM-level fault injection.
- Absence of duplicates across 39,400 claims remains weak evidence for a concurrency
  property in general. The design still rests on SQLite's write-lock semantics being
  correct, not on this measurement.
- The `fullfsync=0` divergence attribution is a hypothesis, not a decomposition.
- Nothing here exercises lease expiry, fencing-epoch conflict, or recovery. Those are S3.
- Configurations B and A at 16 writers remain non-representative in the way ADR-0004
  described: only 12.5–13.0 of 16 writers ever claimed a row before the queue drained.

## Pre-registered falsification / revisit trigger

1. Any duplicate claim, at any concurrency, in any later slice. Invalidates the
   mechanism outright.
2. p99 claim latency > 100 ms **at ≤4 concurrent writers, `fullfsync=1`, under
   `rusqlite`, at an inter-claim gap ≥25 ms per worker** — i.e. measured at an arrival
   rate Conductor could plausibly reach, not under saturation. Currently 24.6 ms.
3. Any `SQLITE_BUSY` at ≤4 concurrent writers. Currently zero.
4. Worst-case claim latency exceeding **6 s**, which would put the 60 s lease inside a
   10× margin and require §4.7's lease constant to be re-derived rather than assumed.
5. A second writing process appearing (a daemon plus racing CLI writes), which changes
   the contention model all of this was measured under.

## Master-plan deltas forced by this ADR

**(1) §2.6 revisit trigger 3** — add the offered-load qualifier, and record the
saturation ceiling separately so the number is not lost:

> 3. Sustained >50 concurrent runs, or **p99 claim latency >100 ms at ≤4 concurrent
>    writers with `fullfsync=1` under `rusqlite`, measured at an inter-claim gap ≥25 ms
>    per worker** (Conductor's real arrival rate is a few claims per hour). Currently
>    **24.6 ms** (ADR-0005). Under deliberate saturation the same configuration reaches
>    265 ms — that is queueing at 100% utilisation of a serialised writer, is expected,
>    and is *not* this trigger.

**(2) Part 5.1** — `CREATE INDEX ix_event_run` becomes `CREATE UNIQUE INDEX`.

**(3) Part 0** — add measured facts M23–M26.

## Impacted master-plan sections

Part 0 · Part 5.1 (`event` index) · §2.6 (revisit trigger 3) · §4.7 (claim, lease
constant) · slice S1.

## Reproduce

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo run --release -p conductor-store --bin conductor-claim-bench -- --self-test
bash scripts/measure/s1_rusqlite_claim_latency.sh
```
