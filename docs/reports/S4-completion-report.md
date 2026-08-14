# S4 — Completion Report

**Slice:** S4 — Verification runner
**Date:** 2026-08-13
**Status:** COMPLETE — stop point reached (cache correctness proven)
**Starting commit:** `e69e190`
**Ending commit:** see "Push status"

---

## Objective

Run checks, bind every result to the exact tree it observed, classify four outcomes.
**Verification is authoritative; the agent's report is not.**

## Verification — commands and results

All run by me, not taken from the subagent's report.

| Check | Result |
|---|---|
| `cargo fmt --check` | **PASS** |
| `cargo clippy --all-targets --all-features -- -D warnings` | **PASS** (forced re-analysis) |
| `cargo test --all` | **458 passed, 0 failed** — 2 m 18 s wall |
| Regression | all 341 prior tests still pass |
| Slice filter | `cargo test -p conductor-run verify::` → 43; `verify` → 73 |
| Dependencies added | `serde_yaml 0.9.34` (+4 transitive) — authorized by name in §2.2 |
| `sha2` / `regex` / `tokio` | **none added** |

Suite wall time did **not** grow despite +117 tests (S3: 3 m 24 s at 341; S4: 2 m 18 s at
458). The subagent was right to flag that as build-state rather than a real speedup, and I
have not re-measured S3 under identical conditions — "not materially slower" is what the
evidence supports.

## The `VOID` invariant — verified by me, not accepted

§4.5's most important property: a result whose tree moved under it is `VOID`, never
`PASS`. I removed the after-check tree comparison from `runner.rs` and re-ran:

```
test verify_a_tree_mutated_by_a_background_process_mid_check_is_void ... FAILED
assertion `left == right` failed: a green check on a tree that moved under it must be VOID
  left: Pass
 right: Void
test result: FAILED. 24 passed; 2 failed
```

Restored; 26/26 pass. That is exactly the hole §4.5 describes — "a green result for a tree
that never existed".

**The test is not a coin flip**, which mattered given this project's history. The
background write is forced *inside* the check window by **two FIFOs**, not sleeps: the
check blocks until the writer arrives, the writer blocks until the check acknowledges. No
interleaving puts the write outside the window. The FIFOs live outside the workspace —
inside, they would move the tree themselves and the test would pass proving nothing.
Measured 25/25 clean, plus 10/10 under deliberate 8-way CPU load.

## The four outcomes

| Outcome | Produced by |
|---|---|
| `PASS` | exit 0, tree held |
| `FAIL` | non-zero exit; `on_timeout: fail`; program-fault signal (SIGSEGV/ABRT/BUS/FPE/ILL/TRAP/SYS) |
| `INCONCLUSIVE` | timeout (default); spawn failure; log unwritable; externally-imposed signal; **flaky-retry disagreement** |
| `VOID` | tree moved, or tree unhashable |

`VOID` outranks everything, including exit 1 — a `FAIL` on a vanished tree would send a
repair agent after a defect that may exist in no tree at all.

## Cache

Key: `(tree_hash, check_id, command_hash, toolchain_fingerprint)`. **Proven to miss, not
assumed**: a test varies each component alone, asserts a miss, then asserts the original
still hits. "Did it run?" is measured by a counter file **outside** the workspace, so it
is a fact rather than an inference.

`INCONCLUSIVE` and `VOID` are deliberately **not** cached (`ix_verif_cache` is partial).
The reasoning is right and worth preserving: `INCONCLUSIVE` describes a *moment* — a
timeout, a vanished toolchain, a full disk — so caching it freezes a transient condition
into that tree's permanent answer and makes §4.5's "bounded infra retry" unreachable.
`VOID` is the *absence* of a result; filing it under a tree hash as that tree's answer is
a lie by construction.

A same-key contradiction (PASS then FAIL) keeps the first answer and raises
`VERIFICATION_NONDETERMINISM` rather than overwriting — otherwise the cache would hide
the nondeterminism permanently.

## Completion criteria — 5 of 7 enforced, 2 deferred but not stubbed

Enforced: required checks PASS at the current tree hash · conditional checks · invariants ·
zero unresolved findings · reconciliation verdict ∈ {`CLEAN_COMPLETE`, `CLEAN_NO_REPORT`}.
Deferred: acceptance bindings (S11), policy grants (S7).

**The deferred two are not hardcoded true.** Each has an evidence enum with exactly one
variant — `NotEvaluated { owner }` — so today there is no way to *write down* "policy is
satisfied". When S7 adds a satisfied variant, the gate's exhaustive `match` **stops
compiling**, forcing S7 to decide at this gate. Same for S11, and for any eighth criterion.

Also enforced: an empty required-check set **refuses** to complete — "all required checks
passed" is vacuously true of a profile with none.

`ReconciledRoute::Complete` now exists, but carries a `VerifiedComplete` whose fields are
private and whose only constructor is the gate — the same shape as S3's `TerminalAttempt`.
`Deserialize` was removed from `ReconciledRoute`, because a deserialisable route would let
`{"COMPLETE":{...}}` arriving from a file or socket mint the token.

## Two rulings I made

### 1. `sha256` vs BLAKE3 → ADR-0007

The subagent escalated this rather than deciding, which was right. §4.5 said "sha256
recorded" and Part 5.1 declared `artifact.sha256`, but §2.2 authorizes `blake3` and **no
SHA-2 implementation**, and S3 had already established `content_hash()` = `blake3:<hex>`.

**Ruling: BLAKE3 is Conductor's only content digest; the column is renamed.** §2.2's
dependency list is a deliberate architectural constraint; "sha256" in §4.5 is a generic
word for "a hash of the file". Letting incidental prose add an unauthorized dependency is
exactly how dependency lists rot. Implemented as **schema v3** (forward migration, written
test-first — I watched it fail with `columns were [... "sha256" ...]`). Nothing had written
the table, so the migration was free.

### 2. M29 was wrong, and I re-measured it myself

S4 reported that the macOS binary scan is far smaller than S2.5 recorded. I measured it
independently over 3 fresh binaries:

```
pair 1: cold=234.1ms warm=3.3ms
pair 2: cold=228.4ms warm=3.6ms
pair 3: cold=224.1ms warm=4.8ms
```

**M29's recorded "21.7 s cold vs 3.3 s warm" was wrong** — S2.5 timed an entire
`codex sandbox` probe run and attributed all of it to the OS scan. The real figure is
~228 ms vs ~3.9 ms, two orders of magnitude smaller. The scan also lands *after* `spawn()`
returns, so "start the clock after spawn" would not absorb it; a grace period until *first
output* does. Part 0's M29 row is corrected and labelled as a correction.

## Spec defects found

1. **`tree_hash` was under-specified and the obvious reading is wrong.** The existing
   `Baseline::tree_hash` is `HEAD^{tree}`; binding to that makes every uncommitted agent
   edit invisible and `VOID` undetectable. S4 uses a **working**-tree hash, which must
   honour `.gitignore` or any check writing to `target/` voids itself. Recorded in §4.5.
2. **§4.5's `conditional` block is not implementable as written** — `commands: [x]` gives
   no `check_id`, yet `check_id` is a cache-key component. Both spellings now accepted.
3. **`command:` as a string cannot express a quoted argument.** An argv-list form was
   added; both hash identically; quotes in the string form are refused rather than
   half-parsed.
4. **The log name `<check>-<attempt>.log` is not unique** — §4.5 itself requires re-running
   a `VOID` check at the new tree, same check and attempt. Names are now qualified, and
   `create_new` makes a collision an outcome rather than an overwrite.

## Two defects S4's own failure injection found

Both are the same shape as bugs S3 found, which is now a recognisable pattern:

- **A killed worker permanently blocked its successor from logging.** Same
  permanent-stranding shape as S3's `create_workspace`. The guard was redundant: the run's
  lease already excludes two live workers.
- **An overrunning check leaked its grandchildren.** `sh -c 'echo x; sleep 60'` with a 1 s
  budget was killed on time, but `sleep` inherited the pipe and the test took 60 s. The
  correctness cost is worse than the delay: a killed `cargo test` would leave compilers
  running **inside the workspace**, writing after the after-check hash. Fixed with
  `setpgid` + process-group kill (60.6 s → 1.6 s for that file).
  **`supervise.rs` has the same latent gap for agents. S5 must close it.**

## Known limitations

1. **No genuine `ENOSPC`** — no unprivileged filesystem mount available on this host.
   Substituted `RLIMIT_FSIZE` in a dedicated child (same class: the open succeeds, the
   write fails part-way). `StorageFull` itself is covered only by the pure classifier.
2. **A killed Conductor cannot reap its own check** — `SIGKILL` runs no destructor. Same
   class as S3's agent gap. Bounded and *detected* rather than missed: an orphan writing
   into the workspace moves the tree, so the next check is `VOID`, not a false `PASS`.
3. Evidence types have public fields, so the gate's *caller* can lie to it. The guarantee
   is that `COMPLETE` cannot be **named** without going through the gate — not that the
   gate is unfoolable by its own caller.
4. The secret detector is minimal and its blind spots are enumerated in code and asserted
   by a test: bare high-entropy strings, secrets split across lines, encoded secrets,
   non-UTF-8 output, positional passwords. **S9 owns the real scanner.** It was tested
   with 15 planted synthetic secrets and a false-positive suite over real build output.
5. `serde_yaml` is archived upstream. The entire surface is one `from_str` call, so a swap
   is a one-file change; recorded in `Cargo.toml`.

## Rust falsification tracking (S1–S5)

| Metric | S4 |
|---|---|
| Median `cargo check --all-targets` | **2.21 s** (down from S3's 3.60 s) — threshold 90 s |
| Type/serde plumbing | ~15% of the slice; no commit primarily plumbing |

## Skills used

`superpowers:test-driven-development`, `superpowers:verification-before-completion`.
`systematic-debugging` was **not** formally invoked — the subagent reported diagnosing the
three defects directly from reproducible test output, which I accept as honest rather than
a skipped step.

One honesty note the subagent volunteered and I am preserving: it wrote
`conductor-git/src/tree.rs` implementation-first by mistake, then **deleted the file**,
wrote the test, observed the failure, and re-derived it. That is the Iron Law applied
against its own work.

## Push status

Slice-scoped commit on `main`, pushed to `origin/main`. Working tree clean.

## Recommendation

**S4 COMPLETE — CONTINUING AUTOMATICALLY TO S5 (first complete vertical).**

S5 must close `supervise.rs`'s process-group gap (above), and at its end the **Rust
falsification trigger is formally evaluated** over S1–S5.
