# ADR-0008 — §4.6's `progressed()` counted a pure regression as progress

**Status:** ACCEPTED
**Date:** 2026-08-14
**Slice:** S6 (found by the slice's own test, against the slice's own implementation)

---

## Question

§4.6 defines the repair loop's progress predicate as, verbatim:

```
progressed(prev, next) :=
      next.failing_checks ⊂ prev.failing_checks              # strictly fewer
   OR (next.fingerprint ≠ prev.fingerprint AND tree changed) # different problem, real edit
```

S6 implemented it literally and, separately, wrote a test asserting that a repair
which fixes nothing and breaks something new is not progress. The two disagreed.
Concretely, for `prev.failing_checks = {alpha}` and `next.failing_checks = {alpha,
beta}` at a changed tree:

- first disjunct: `{alpha, beta} ⊄ {alpha}` → false;
- second disjunct: the failing-check set is hashed into the fingerprint (§4.6's
  `sorted(failing_check_ids)`), so adding `beta` changes it; the agent edited
  files, so the tree changed → **true**.

`progressed()` therefore returns **true**. Every check that failed before still
fails, one more fails now, and the loop is told to keep spending.

So: is the formula right and the test wrong, or the other way round?

## Why this needed a ruling rather than a choice

`progressed()` is not decoration. `breaker::decide` consults it directly and
stops with `NoProgress` when it returns false, and it is the only stop that fires
when `stop_on_identical_fingerprint` is switched off. A predicate that says "yes"
to a regression removes a stopping condition.

It also fails in the direction S6's own sibling module documents as the dangerous
one. `repair/fingerprint.rs` states the asymmetry explicitly: over-stripping
stops the loop early, which is "loud, and safe"; under-stripping lets the loop run
to the budget, which is "silent". Calling a regression progress is the silent
failure applied to the predicate instead of to the normalizer — nothing announces
it, the run simply burns its budget on an agent that is making things worse.

## Decision

**A proper superset of the previous failing-check set is not progress, and
short-circuits `progressed()` to `false` before either disjunct is evaluated.**

```rust
let regressed = prev.failing_checks.is_subset(&next.failing_checks)
    && next.failing_checks != prev.failing_checks;
if regressed { return false; }
```

Reasons, in order of weight:

1. **The formula's second disjunct is a heuristic for "the agent did something
   real"; it was never intended to license "the agent made it worse."** Its own
   inline comment reads `# different problem, real edit`. `{alpha} → {alpha,
   beta}` is not a different problem — it is the same problem plus another one.
2. **The predicate must fail safe.** Both readings stop the run eventually; only
   one of them stops it for a reason a human can act on, and only one of them
   refuses to spend an agent attempt on a strictly worse tree.
3. **The guard is the narrowest possible.** It refuses only unambiguous
   regression. `{alpha} → {beta}` — alpha fixed, beta revealed — is *not* a
   superset and remains progress through the second disjunct, which is the
   ordinary shape of real repair and the case the disjunct exists for.
4. **It cost nothing to state and cannot be reached by accident.** Proper
   containment in both directions is now explicit in the code, where §4.6 had
   `⊂` in one direction and silence in the other.

## What this DOES prove

- `crates/conductor-run/tests/repair.rs` pins all eight cases of the predicate,
  including the two that distinguish `⊂` from `⊆` in each direction.
- The eight cases pass against the amended implementation and
  `more_failing_checks_is_not_progress` failed against the literal one — the
  disagreement was observed, not reasoned about.

## What this DOES NOT prove

- It says nothing about whether a *partial* regression — some checks fixed, others
  newly broken, neither set containing the other — is progress. That case still
  resolves through the second disjunct as "progress", and the budget is what
  bounds it. This is deliberate: no evidence yet says which way it should go, and
  inventing a rule for it would be exactly the speculative tightening this ADR
  declines to do.
- It does not make `progressed()` sufficient on its own. It never was; §4.6's
  three loop-breakers and S6's hard invocation ceiling are what bound the loop.

## Pre-registered falsification / revisit trigger

1. A real repair run is refused with `NoProgress` on a superset that a human
   judges to have been genuine forward motion — most plausibly when a fix
   unblocks previously-skipped checks that then fail for the first time. That
   would show the guard is too broad and the rule needs the "previously not run"
   distinction the current check-id sets cannot express.
2. The partial-regression case above showing up in practice as a budget sink.

## Master-plan deltas

**(1) §4.6** — `progressed()` gains the regression guard as a leading clause, with
the note that `⊂` is proper containment in both directions.

## Impacted master-plan sections

§4.6 (bounded repair) · slice S6 · acceptance rows 7 and 9.
