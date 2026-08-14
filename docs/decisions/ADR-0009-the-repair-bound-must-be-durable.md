# ADR-0009 — §4.6's invocation bound was neither achievable as stated nor durable

**Status:** ACCEPTED
**Date:** 2026-08-14
**Slice:** S6

---

## Question

§4.6 closes with the property the whole slice exists to deliver:

> **Acceptance property:** no configuration of the fake agent can produce more
> than `max_attempts` agent invocations. Asserted by counting spawns.

Implementing it surfaced two separate problems with it.

**First, the number cannot be right.** `max_attempts` counts *repairs* — §4.6's
own budget test fixes this: one initial attempt plus two repairs is three
attempts against `max_attempts: 2`. So the initial attempt was never inside the
bound. And §4.7 exempts infrastructure retries from the budget entirely
("backoff, does **not** consume budget"), so they were not inside it either.
Taken literally, the property was unsatisfiable by its own definitions.

**Second, and worse, nothing said the count had to survive a restart.** The
natural reading of §4.6 is an in-memory `RepairHistory` threaded through a loop.
But acceptance rows 10 and 11 restart runs by design — daemon crash, reboot with
live workspaces — and a count held in a process resets when that process dies.
A crash-restart cycle would then produce unbounded agent invocations while every
loop-breaker still read as correct in isolation.

The window is real and narrow: §4.7's supervisor commits the `attempt` row and
`STARTING` **before** `spawn()`, while the verification result that feeds a
fingerprint is written **after**. A process killed in between leaves an
invocation on the record and no memory of what it produced. §4.6's history
under-counts, `decide()` sees a run that has barely started, and it says
`Repair` — forever.

## Why this needed a ruling rather than a choice

This is the failure mode this project has hit at every slice since S2: a
mechanism that is correct in the state it can see, and blind to the state it
cannot. S2's isolation test could not fail; S5's row 22 assertion passed because
git was idempotent, not because Conductor's guard worked. Here, every one of
§4.6's three loop-breakers can be individually correct and the system still spend
without limit, because the breakers reason over a history that a crash erases.

A bound that holds only when nothing goes wrong is not a bound. It is a bound
for the case that was never the risk.

## Decision

**Two changes, and the second is the load-bearing one.**

**(1) The bound is stated as a function of configuration only:**

```
ceiling = 1 + max_attempts + max_infra_retries      # defaults: 1 + 2 + 1 = 4
```

One initial attempt, plus §4.6's repairs, plus acceptance row 8's bounded
infrastructure retries. `max_infra_retries` is added to §4.6's `repair:` block
because §4.7 bounds infrastructure retries in prose without a number and row 8
supplies it; without a term of its own, the one count that matters — spawns —
would be unbounded.

**(2) The ceiling is enforced from durable state, immediately before every spawn,
in addition to the loop-breakers — never instead of them.**

The count is `attempt` rows for the run, which §4.7's supervisor already commits
before `spawn()`. Repair observations (fingerprint inputs, tree hash, failing
checks) get their own durable table in schema v5, so `RepairHistory` is rebuilt
from the database on every pass rather than carried in a process.

The two mechanisms answer different questions and are deliberately redundant:

- the **breakers** stop the loop early and tell a person *why* — a run that is
  looping should be reported as looping, not as merely out of money;
- the **ceiling** holds when the history the breakers read has been lost, and is
  the only thing that does.

Within one process the ceiling is unreachable, because `ceiling ≥ decide()`'s
bound by construction. That is the intended relationship, not redundancy to be
optimised away.

## What this DOES prove

Verified by mutation, independently reproduced by the orchestrator rather than
taken from the implementing agent's report:

- **Ceiling disabled** (`if false && spawned >= allowed`) →
  `the_ceiling_holds_when_every_crash_loses_the_observation` fails, having
  reached **ordinal 12** where 4 are allowed. Restored, the suite is green.
- **Breaker 1 disabled** → 2 tests fail, including one asserting that a crash
  between two identical failures does not hide the loop.
- **The durable observation write made a no-op** → 7 tests fail, which is the
  direct evidence that the durability half of this ADR was necessary.

Spawns are counted three independent ways that must agree: the adapter's
`command()` calls, the durable `attempt` rows, and an on-disk marker the spawned
child writes itself. For every hostile configuration — always-identical,
oscillating, no-change, broken-toolchain, novel-failure-each-time — all three
agree and all are ≤ ceiling.

## What this DOES NOT prove

- **The ceiling's non-vacuity rests on a single test.** Because `ceiling ≥
  decide()`'s bound always holds in-process, removing the ceiling leaves the
  hostile-configuration tests green; only the crash-window test kills it. This is
  by construction rather than by oversight, but it means that one test is the
  entire evidential basis for the backstop, and deleting it would silently make
  the ceiling untested.
- The crash window is injected by deleting observation rows, not by a real
  `SIGKILL`. That is deliberate — the state is the thing under test, and
  `crash_matrix.rs` already exercises the supervisor at thirteen kill points —
  but it is a simulation of the window, not the window itself.
- It says nothing about bounding spend *across runs* of the same task. The
  ceiling is per-run.

## Pre-registered falsification / revisit trigger

1. Any future path that spawns an agent without passing the ceiling check —
   which would make the backstop bypassable exactly where it matters.
2. The crash-window test being deleted, weakened, or made conditional.
3. A real adapter (S10/S15) whose spawn is not preceded by a committed `attempt`
   row, which would break the assumption the durable count rests on.

## Master-plan deltas

**(1) §4.6** — the acceptance property is restated as the `ceiling` formula, with
the durability requirement and its rationale.
**(2) §4.6** — `max_infra_retries: 1` added to the `repair:` block.

## Impacted master-plan sections

§4.6 (bounded repair) · §4.7 (retry kinds, supervisor ordering) · Part 5.1
(schema v5) · acceptance rows 8, 9, 10, 11 · slice S6.
