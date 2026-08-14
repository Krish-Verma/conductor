# S5 — Completion Report

**Slice:** S5 — First vertical: task → agent → reconcile → verify → commit
**Date:** 2026-08-13
**Status:** COMPLETE — stop point reached (the spine works)
**Starting commit:** `f3d5366`
**Ending commit:** see "Push status"

---

## Objective

One fake-agent task from `PENDING` to `COMPLETE`, end to end, with a real commit — before
policy, approvals or packets add surface.

## Verification — run by me

| Check | Result |
|---|---|
| `cargo fmt --check` | **PASS** |
| `cargo clippy --all-targets --all-features -- -D warnings` | **PASS** (forced re-analysis) |
| `cargo test --all` | **536 passed, 0 failed** — 3 m 23 s |
| Regression | all 458 prior tests still pass |
| End-to-end | `conductor task run` exit 0, real commit with three real trailers, `main` untouched |

Schema **v4** adds `run.target_branch` — Part 5.1's `run` recorded *what* the target ref
pointed at (`base_commit`) but not *which ref it was*, so §4.1's row-16 divergence check
was unexpressible.

## The finding that matters most: row 22's acceptance assertion is insufficient

Part 9 row 22 says to assert **exactly one commit and one ref update**. S5 tried to prove
that assertion non-vacuous by disabling the precondition re-check — and **the whole matrix
stayed green.**

The reason is that **git is itself idempotent for both effects** under a fixed
`operation_id`:

- `git fetch` of an unchanged ref performs **no ref update at all**. Measured directly:
  `reflog after first fetch: 1`, `reflog after second fetch: 1`. A blind retry is
  indistinguishable from none.
- git **refuses an empty commit**, so a second commit never appears. Disabling *that*
  re-check failed the matrix, but on the progress assertion, not on a count.

So the counting assertion can never fail, and a test that cannot fail is not evidence.
The property that *can* fail is the one worth testing: an effect Conductor **cannot
decide** must be recorded `AMBIGUOUS` and halt, never overwritten. That test —
`a_ref_conductor_did_not_move_is_noticed_rather_than_overwritten` — was written and then
falsified by reducing the precondition to two values:

```
the undecidable effect must be recorded as AMBIGUOUS
resume error: git fetch … exited 1: ! [rejected] conductor/T-0012/r-0041 (non-fast-forward)
  left: 0   right: 1        test result: FAILED
```

With the 3-valued precondition: the effect goes `AMBIGUOUS`, a `CRITICAL` finding is
raised, **the stranger's sha is untouched**, and the run halts at `AWAITING_REVIEW` with no
lease. `effects.rs` was restored byte-identical (`diff -q`).

Part 9 now carries this note, because a future reader would otherwise "simplify" the
matrix back to the vacuous form.

## The six kill points

| # | Point | Window |
|---|---|---|
| 1 | `before-commit-intent` | staged; nothing durable knows |
| 2 | `after-commit-intended` | `INTENDED`, **no commit** — intent→effect |
| 3 | `after-commit-created` | commit exists, **no receipt** — effect→confirm |
| 4 | `after-commit-confirmed` | receipt durable, fetch not intended |
| 5 | `after-fetch-intended` | `INTENDED`, **ref not moved** — intent→effect |
| 6 | `after-fetch-performed` | ref moved, **no receipt** — effect→confirm |

A separate test asserts the world's actual state after each kill *before* any restart, so
no point can be silently mislabelled.

## S3's methodology carry-forward, answered

S3 found `assert_converged` insufficient — a run can converge to "no human needed" and
still be unable to progress. S5's matrix asserts `run.state == COMPLETE` **and**
`task.state == COMPLETE` **and** the commit exists **and** the ref moved.

That stronger assertion **earned its keep during the slice**: it caught a real bug where,
after committing, the index matched `HEAD`, so re-entry saw nothing to stage and **the
fetch would never happen** — converged, no human needed, permanently stuck. Exactly S3's
class of bug, caught because the assertion was not merely "converged".

## Git trailers — what is real, and what is deliberately absent

**Populated:** `Conductor-Run`, `Conductor-Policy` (the hash of the empty snapshot
actually in force), `Conductor-Verification` (BLAKE3 over sorted
`(check_id, outcome, tree_hash, command_hash)` + toolchain fingerprint — **recomputable
from the artifacts**, which is what makes it audit rather than decoration).

**Absent, not invented:** `Conductor-Plan` (S11), `Conductor-Approval` (S8). §3.4 exists so
the audit trail survives total local state loss — and a reader recovering from nothing
cannot distinguish a fabricated hash from a real one, so **one made-up value poisons all
five**. A test asserts both the presence of the three and the absence of the two.

## The S4 carry-forward is closed — and the gap was worse than S4 knew

S4 reported that `supervise.rs` had the same grandchild leak as the verification runner.
The RED showed it was worse than a delay:

```
grandchild 40548 outlived the agent 40546 Conductor killed; it is still running
inside the workspace and can still write to the tree

the supervisor never returned: a grandchild is holding the agent's stdout pipe open,
so the reader threads never see EOF and finish() blocks in join() — the run can never
be reconciled: Timeout
```

A leaked grandchild could **permanently wedge the run**, not just write late. Closed with
`setpgid(0,0)` in `pre_exec` (a failing `pre_exec` makes `spawn` fail, so holding a
`SpawnedAgent` proves the group exists and `kill(-pid, …)` can never name Conductor's own
group), a group sweep in `finish()` on **every** return path including clean exit, and a
sweep in `Drop`. That test file went 20.04 s with 2 failures → 0.75 s with 15 passing.

## Rulings I made on contradictions S5 surfaced

**1. §4.5 criterion 4 vs Part 9 row 5 — a direct contradiction.** Criterion 4 said "zero
unresolved findings"; row 5 says a malformed report reaches `COMPLETE` + finding with no
human. Read absolutely, criterion 4 blocks that forever, since findings never auto-resolve
— one cosmetic finding would permanently strand a task whose tests all pass.

**Ruling: zero unresolved findings of blocking severity (`CRITICAL`).** This is also the
reading consistent with the product thesis — verification is authoritative and the agent's
report is not, so a garbled report is *evidence quality*, recorded rather than obeyed.
`CRITICAL` is already the severity S3 and S4 use for halting cases.

**2. Four state-machine diagram errors**, all found by building the thing:

- `REPAIRING → RECONCILING` **cannot work** — the claim preserves `RECONCILING`, so such a
  run is re-reconciled with no agent ever running. Replaced with `REPAIRING → READY`,
  which S3's report had already identified as the functioning edge.
- `RECONCILING → REPAIRING` missing from the diagram but forced by §4.8's `NO_CHANGE`.
- `BLOCKED` drawn with no outgoing edge yet not terminal — a trap that strands every
  blocked run. Only `→ CANCELLED`/`→ SUPERSEDED` permitted.
- `AWAITING_APPROVAL` has no exit for a **denial**. Added `→ AWAITING_REVIEW`; **S8 must
  revisit** when denial semantics are real.

## ✅ Rust falsification trigger — evaluated, does not fire

§2.2 pre-registered this decision point for S1–S5.

| Slice | median `cargo check` | plumbing |
|---|---:|---:|
| S1 | 0.33 s | 0/1 commit |
| S2 | 0.33 s | 0/2 |
| S2.5 | 1.23 s | 0/3 |
| S3 | 3.60 s | 0/4 |
| S4 | 2.21 s | ~15 % |
| **S5** | **3.09 s** | **~12 %** |

Thresholds are 90 s and 30 %. **29× inside on compile time; under half the plumbing
ceiling.** Check time has been flat since S3 despite the workspace growing ~35 %.

The qualitative half matters more. §2.2 argued the compiler would be "an automated
reviewer that never tires", and it has been, specifically: the exhaustive `match` on
`Precondition` fired the moment S5 added the two git effect kinds and pointed straight at
the file that had forgotten them. S4 built the same mechanism deliberately, so S7 and S11
**cannot compile** without deciding what their evidence means.

**Rust stands. The trigger is retired for v1** — it was pre-registered for S1–S5 and has
been answered.

## Known limitations

1. **Part 9 row 2's final state `COMPLETE` is out of reach at S5** — the automatic retry
   path is `REPAIRING → READY`, which **S6 owns**. The test asserts what S5 owns:
   `NO_CHANGE`, `REPAIRING`, and no effect performed.
2. **No observable duplicate could be produced** for either git effect (above). Reported
   rather than papered over.
3. **Row 16 does not fetch on divergence** — the work stays in the retained clone, and the
   human sees a repository Conductor has not touched. A defensible reading of "no rebase,
   no merge"; flagged in case the branch should be fetched anyway.
4. The `plan_version` placeholder is written `DRAFT`, never `APPROVED`; it exists only to
   satisfy a non-null foreign key until S11.
5. A run routed to `VERIFYING` was not claimable while `recover()` released the lease — a
   stranding hole, fixed by claiming before recovery and keeping the lease.

## Skills used

`superpowers:test-driven-development`, `superpowers:verification-before-completion`.
`systematic-debugging` not formally invoked; the subagent judged the two mid-slice defects
came straight out of reproducible test output, which I accept as an honest call rather
than a skipped step.

## Push status

Slice-scoped commit on `main`, pushed to `origin/main`. Working tree clean.

## Recommendation

**S5 COMPLETE — CONTINUING AUTOMATICALLY TO S6 (bounded repair).**

S6 owns `REPAIRING → READY`, which is what makes Part 9 rows 2 and 7 reach their stated
final states. Its acceptance property — *no configuration of the fake agent can produce
more than `max_attempts` agent invocations* — must be asserted by **counting spawns**, and
given row 22's lesson, S6 should check explicitly whether that counter can actually fail.
