# S3 — Completion Report

**Slice:** S3 — Fake agent, supervision, crash recovery *(merged)*
**Date:** 2026-08-13
**Status:** COMPLETE — stop point reached (the crash matrix is green)
**Starting commit:** `747fe9e`
**Ending commit:** see "Push status"

---

## Objective

Spawn, supervise and classify an agent subprocess; recover every failure mode from
evidence alone. The master plan puts this third deliberately: *"if recovery is not proven
early, everything built on it is unfalsifiable."*

## Provenance — read this before trusting the TDD claims

The working tree was **not empty** when the implementing subagent started. An earlier
dispatch of this same slice was interrupted mid-flight and had left roughly **9,000 lines
of S3-shaped work** on disk.

The subagent reconciled state per the resume protocol, and then made the right call: it
**declined to claim red-green for code it did not write**. Instead it *falsified* the
inherited work — mutation testing, which is stronger evidence than having watched a test
go red, because it proves the test detects the defect rather than proving the author saw
a failure once.

That falsification found **two real defects** in the inherited code, both fixed with a
genuine red-green cycle. So the honest summary is: ~9k lines inherited and adversarially
verified; the corrections written test-first.

## Both S1-surfaced contradictions resolved

**(a) `RECOVERING` was never a state.** The `RunState::Recovering` variant is deleted;
the predicate is `state IN ('READY','RECONCILING')`. A `RECONCILING` run whose lease
expired is exactly what a restarting worker must take, and `lease_expires_at` already
protects a live worker. The claim now also **preserves** `RECONCILING` instead of
overwriting it with `RUNNING`, because §5.2 has no `RECONCILING → RUNNING` edge.

**Re-measured, because ADR-0005's evidence is tied to this exact statement:** 39,400
claims, **0 duplicates, 0 invariant failures**; worst-case latency 5147 ms → **4950 ms**.
The 60 s lease keeps its ~12× margin. M26 stands.

**(b) `attempt` had no `state` column.** Schema **v2** (forward migration, v1 untouched)
adds `attempt.state TEXT NOT NULL DEFAULT 'CREATED'` and a partial index on the in-flight
set. The default is deliberate: a pre-existing row reads as *in flight*, so recovery goes
and looks at the world rather than assuming the attempt finished — failing safe.

## Verification — commands and results

All run by me.

| Check | Result |
|---|---|
| `cargo fmt --check` | **PASS** |
| `cargo clippy --all-targets --all-features -- -D warnings` | **PASS** (forced re-analysis) |
| `cargo test --all` | **341 passed, 0 failed** — 3 m 24 s wall |
| Regression | all 198 prior tests still pass |
| Claim re-measurement | 39,400 claims, 0 duplicates, worst case 4950 ms |
| Dependencies added | `blake3` (+4 transitive) only — required by §4.7's `operation_id`, authorized by §2.2 |
| `tokio` | **not added** — 0 occurrences in `Cargo.lock` |
| `deny_unknown_fields` | 0 usages (9 matches are all comments *forbidding* it) |
| `libgit2`/`gix` | 0 usages (comments only) |

## The two invariants I verified myself rather than accepting

**1. Fencing (Part 9 row 27) is non-vacuous.** I disabled `check_fence` in
`conductor-store/src/lease.rs` and re-ran:

```
---- a_stale_worker_that_wakes_after_lease_expiry_cannot_write ----
panicked: a stale epoch must not be able to move the run: Reconciling
---- the_epoch_moves_at_lease_expiry_even_before_a_successor_claims ----
panicked: expiry alone must fence the old owner out: 1770000120001
test result: FAILED. 4 passed; 3 failed
```

Restored; 7/7 pass. The stale-worker guarantee is genuinely tested.

**2. `RUNNING → COMPLETE` is unrepresentable, not merely untested.** §5.2 demands type-
system enforcement. Verified in the source:

- `leave_running()` takes a `TerminalAttempt` token — obtainable only from an attempt
  that actually reached a terminal state — and returns exactly **one** destination
  (`Reconciling`). There is no second destination to pass, so the illegal transition
  cannot be expressed.
- `ReconciledRoute` — the only type describing where a reconciled run goes — has **no
  `Complete` variant at all**. Until S4 supplies verification, no code path, tested or
  untested, can route a run to `COMPLETE`.
- Both carry `compile_fail` doctests **plus a non-vacuity control** that compiles the
  same shape with a variant that does exist.

## Two findings that change how later slices must test

### The prescribed 12 Conductor kill points were insufficient

There was no kill point between `git clone` returning and `attach_workspace` committing.
That gap held a real bug: `create_workspace` refuses an existing path and every attempt
derives the same path, so one badly-timed crash **stranded the run permanently**.

The subagent verified by experiment that the matrix *as specified* would not have caught
it — with the fix disabled, `killing_conductor_at_every_point_converges_without_a_human`
still passed. A 13th point was added, and the run now adopts an unrecorded workspace whose
descriptor names it (reusing S2's descriptor mechanism).

**`assert_converged` is necessary but not sufficient.** Convergence to "a state needing no
human" can hide a run that will never make progress. S6 must carry this.

### A test can race its own timer, and be wrong while the product is right

`agent-kill-7-after-git-change` failed intermittently. Root cause: the agent-kill matrix
asserted `CRASHED` while running under a 1500 ms idle budget, and that scenario shells out
to `git`. Under load the idle timer fired first and the supervisor recorded `TIMED_OUT` —
which is **correct**, because §6.4 deliberately makes a timeout outrank the signal
Conductor used to enforce it.

The product was right; the test was racing its own timer. Fixed by separating budgets:
generous where the test asserts *how an agent ended itself*, tight only where a timeout is
the subject. Verified across 3 matrix runs (one under deliberate load) and 3 full-suite
runs.

This is the third slice running in which a test was wrong in a way that looked green. It
is now a standing expectation, not a surprise.

## M29 (macOS binary scan) handling

Three deadlines rather than one:

- `startup_timeout` (60 s, from spawn) — **absorbs the OS scan**;
- `idle_timeout` (from last output);
- `wall_timeout` (from **first** output).

The cold-start scan is charged to startup and never to the agent, so a 21.7 s first-run
scan cannot be misread as a stall. The fake agent's first line is always `agent.ready`,
tests synchronise on named checkpoints rather than sleeps, and `warm_the_binary()` pays
the scan once per test binary outside any deadline.

## `tokio` — not added

§2.2 authorizes it for process supervision, but one child needs two reader threads and
`recv_timeout`; the genuinely hard part is `Drop`, which is identical with or without an
async runtime. Reasoning recorded in `supervise.rs`'s module docs.

## Known limitations — stated, not papered over

1. **`workspace.create` is not routed through the side-effect ledger** in the worker (only
   `artifact.write` is). The resulting hole was closed by descriptor-based adoption, which
   is smaller and reuses S2's mechanism, but the ledger kind remains test-only.
2. **A run routed to `REPAIRING` cannot be re-claimed within S3.** `CLAIMABLE` is
   `[READY, RECONCILING]`; `REPAIRING → READY` belongs to S6. "Converges" at those kill
   points means *reaches a state needing no human*, not *a further attempt runs*.
3. **`ArtifactRoot::claim_attempt_dir` has a narrow race**: between `create_dir` and
   writing provenance, a competitor sees `Unattributed`. It **fails closed** (refuses), so
   it is safe, but the error names no owner.
4. Suite wall time is now 3 m 24 s. Acceptable, but it is the primary CI harness forever
   and will need watching.

## Rust falsification tracking (S1–S5)

| Metric | S3 value |
|---|---|
| Median `cargo check --all-targets` | **3.60 s** (3.32 / 3.41 / 3.60 / 3.78 / 4.79) — threshold 90 s |
| Commits primarily type/serde plumbing | 0 of 4 slice commits |

Check time has grown 0.33 s → 3.60 s across three slices as the workspace grew. Still 25×
inside the threshold, but the trend is worth carrying to the S5 review rather than
discovering at S5.

## Skills used

`superpowers:test-driven-development`, `superpowers:systematic-debugging` (the flake
root-cause), `superpowers:verification-before-completion`.

## Push status

Slice-scoped commit on `main`, pushed to `origin/main`. Working tree clean; local and
remote identical.

## Recommendation

**S3 COMPLETE — CONTINUING AUTOMATICALLY TO S4 (verification runner).**

S4 must deliver the `VOID` outcome with the same rigour: a result whose tree moved under
it is not `PASS`. Given this slice's record, the mid-check mutation test should be proven
non-vacuous by construction — show it fails when the tree-hash comparison is removed.
