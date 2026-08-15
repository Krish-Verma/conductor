# ADR-0012 — §5.2 could not express two outcomes the acceptance suite requires

**Status:** ACCEPTED
**Date:** 2026-08-15
**Slice:** S9 (found by wiring two decided-but-unreachable gates to real call sites)

---

## Question

S7 and S8 each built a decision as a pure function and left the call site to S9,
and the master plan said so explicitly for both. Wiring them asked a question
neither slice had to answer: **where does the run go when the answer is no?**

Two edges turned out not to exist.

## Why the answer matters to Conductor

The legality table in `conductor-core/src/task.rs` is the authority — §5.2's
diagram is "kept as the shape". A transition the table does not draw is refused
at the store layer with `IllegalTaskTransition`. So a gate whose refusal has no
legal destination cannot refuse: it can only fail with an internal error, or be
quietly dropped. Both were reachable states of this codebase before S9.

## Experiment / evidence

### (1) `READY → BLOCKED` — the eligibility refusal had nowhere to go

Acceptance row 30:

| # | Scenario | Expected persisted state | Final |
|---|---|---|---|
| 30 | Ineligible execution mode | attempt never starts | `BLOCKED` |

§5.2 draws the edge as `READY ──claim+eligibility──► RUNNING` — one arrow,
**two** gates named on it, and only the both-pass outcome drawn. The table read:

```rust
Ready => &[Running, Cancelled, Superseded],
```

`Blocked` is not reachable from `Ready`. Nor is it reachable from `Running`
(`Running => &[Reconciling, Cancelled]`), so there was no path from a claimable
run to row 30's stated final state at all.

### (2) `AWAITING_APPROVAL → ?` — "resumes on grant" resumed into the wrong state

Acceptance row 12 says a granted run "resumes on grant". The diagram's
`(granted)` arrow points back at the `READY` column, and the table agreed:

```rust
AwaitingApproval => &[Ready, AwaitingReview, Cancelled],
```

`READY` means "an agent may be claimed and launched". Following it produces this
sequence, all of which is already documented behaviour of existing code:

1. `ensure_workspace` finds `run.workspace_path` set, and calls
   `capture_baseline(&path)` — capturing **the workspace as it now stands**.
2. The approved change is therefore part of the *new baseline*.
3. The attempt reconciles against that baseline and returns `NO_CHANGE`.
4. The approved work is invisible, and the approval authorized nothing.

This is not a hypothesis: `vertical::resume_task`'s own doc comment describes
exactly this mechanism as the reason recovery does **not** run a second attempt.
The two facts had simply never been put next to each other, because no test had
ever granted an approval and asked what happened next.

## Observed result

Both gates were unreachable in the same way and for the same reason: the state
machine was drawn from the success path, and the refusal branch of a labelled
edge was never given a destination.

## Decision

Two edges added to the legality table, each with the narrowest form that works.

**1. `READY → BLOCKED`.** The eligibility gate runs *before* the claim. The claim
statement moves the run to `RUNNING` atomically, so a gate that ran after it
would have to walk a launched run back — and §4.8's "every exit from `RUNNING`
passes through reconciliation" would then need an exception for runs that never
ran anything. Refusing first keeps that invariant literally true and gives row 30
what it asks for: no workspace cloned, no attempt row, no process.

`RUNNING → BLOCKED` was considered and **rejected** for that reason.

**2. `AWAITING_APPROVAL → RECONCILING`.** Not `READY`, for the reason above.
`RECONCILING` is not a new mechanism: the claim predicate already accepts
`RECONCILING` and deliberately preserves it, and §4.7's recovery path reconciles
against the **stored baseline artifact** rather than re-capturing one. A granted
run therefore rejoins exactly the path a crashed-then-resumed run takes, with no
second agent invocation, and the work the human approved is the work that gets
verified.

`READY` is **kept** as a successor: a denial or a plan revision may legitimately
want a fresh attempt, and S13's review outcomes will need it.

Both writes are unfenced and state-guarded (`WHERE state = 'READY'`,
`WHERE state = 'AWAITING_APPROVAL'`), because a run in either state holds no
lease — there is no fencing token to be stale against, and requiring one would
mean claiming the run, which is the thing being avoided. The `WHERE` clause is
the same concurrency mechanism the claim itself uses.

## What this DOES prove

- Row 30 is reachable from a real launch: `enforce_eligibility.rs` drives
  `vertical::run_task` and asserts `BLOCKED`, zero attempt rows, no workspace on
  disk, and a `CRITICAL` finding naming the dimension.
- Row 12's grant path completes: `enforce_approval.rs` asserts the run reaches
  `COMPLETE`, the attempt count is **unchanged** across the resume, the grant is
  `CONSUMED`, and `Cargo.toml` — the approved change — is in the integrated
  commit read back from git.
- Both are mutation-tested. Making the gate permit everything fails six refusal
  tests; making it refuse everything fails the positive controls.

## What this DOES NOT prove

- Nothing here says the *diagram* in §5.2 is now complete. These are the two
  edges S9 needed; S13's review outcomes and S14's daemon may find more, exactly
  as S5 found four and S6 found one.
- `AWAITING_APPROVAL → RECONCILING` is proven for a policy approval raised by a
  run. §4.3's other three approval kinds (plan, policy-exception, review
  acceptance) do not travel this path yet, and this record makes no claim about
  them.
- The unfenced writes are safe because those two states are unclaimed **by
  definition of the claim predicate**. If a future slice makes either state
  claimable, both functions become unsound and must be revisited.

## Pre-registered falsification / revisit trigger

- Any change to `CLAIM_SQL`'s state set. If `READY` or `AWAITING_APPROVAL`
  becomes claimable, `refuse_ineligible_launch` and `resume_after_grant` are
  writing to a run somebody may own, and both must take a fence.
- A second launch path that does not go through `vertical::run_task`. The gate is
  reached from there; a new entry point inherits nothing.
- An approval kind other than `Policy` reaching `AWAITING_APPROVAL` on a run.

## Impacted master-plan sections

- **§5.2, "corrections to the diagram"** — corrections 5 and 6 are added, in the
  same form as the four S5 recorded.
- **Part 9, rows 12, 13, 25, 30** — the notes deferring these to S9 are
  discharged; the rows are scored from end-to-end evidence.
- **§4.2** — "before launching an attempt" is now a call site, and the refusal
  has a persisted state.
