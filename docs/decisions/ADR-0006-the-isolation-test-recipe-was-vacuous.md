# ADR-0006 — The master plan's own hostile-workspace test recipe was vacuous in two ways

**Status:** ACCEPTED
**Date:** 2026-08-12
**Slice:** S2
**Relates to:** ADR-0001 (workspace isolation), master plan §4.1, Part 9 rows 14–15

---

## Question

Master plan §4.1 and slice S2 prescribe the acceptance test that the whole product's
safety story rests on:

> an adversarial script inside the clone that sets a remote, deletes a branch, writes a
> hook, runs `gc`, **and mutates object files in place** leaves the source repository
> byte-identical — asserted by hashing `.git/config`, `git show-ref` output, and
> `git cat-file --batch-all-objects --batch-check`.

Does that recipe, executed literally, actually detect a broken isolation boundary?

## Why the answer matters to Conductor

This is decision 2 of the Ten That Matter Most, and Part 9 rows 14 and 15 both depend on
it. A test that passes because it never put the source at risk is worse than no test: it
converts an unverified assumption into a documented guarantee. S1 had already hit this
exact failure mode once — its `SIGKILL` crash test initially passed in 1.05 s because
SQLite held every dirty page in memory, so the kill had nothing to roll back.

## Experiment / evidence

Implemented in `crates/conductor-git/tests/isolation.rs`, with two independent
non-vacuity mechanisms: a **negative control** (same hostile routine against a plain
`git clone`, asserting the source *is* damaged) and a **mutation test** (delete
`--no-hardlinks` from production code, confirm the positive test fails).

### Finding 1 — the prescribed step *order* destroys the attack

The recipe says "runs `gc`, **and** mutates object files in place". Executed in that
order the mutation proves nothing: `git gc` runs `repack -d` and `prune-packed`, both of
which **unlink** the loose object files. Unlink only decrements a link count — ADR-0001's
own note says the danger is exclusively *in-place mutation of a shared inode*. After
`gc`, there is no shared inode left to mutate.

The routine must mutate **before and after** `gc`. Measured with the corrected order:
`shared_inodes_seen: 9`, `mutated_before_gc: 9`.

### Finding 2 — the prescribed assertion passes through corruption if read as an exit code

Measured against a deliberately corrupted source:

```
source fsck before    : Some(0)
source fsck after     : Some(128)
batch-check exit      : Some(0) -> Some(0)      <-- unchanged
object bytes identical: false
fsck stderr           : error: corrupt loose object '499fbb72…'
                        error: unable to unpack contents of .git/objects/49/9fbb…
cat-file HEAD         : Some(128)
```

`git cat-file --batch-all-objects --batch-check` **exits 0 even when objects are
corrupt** — it prints `missing` for them. The assertion is only sound because it hashes
the command's *output*. Anyone "tidying" it into an exit-status check would silently make
the whole isolation test vacuous while leaving it green.

### The mutation test, independently reproduced

With `--no-hardlinks` removed from `create_workspace`, the positive test fails and names
the reason — the source repository's own objects have become unreadable:

```
assertion `left == right` failed: git cat-file --batch-all-objects --batch-check output changed
  left: "…6c23984f… missing / …bfa65511… missing / …d184003c… missing…"
 right: "…6c23984f… blob 28 / …bfa65511… blob 6 / …d184003c… tree 36…"
```

and separately: `has 2 links; --no-hardlinks was not applied (left: 2, right: 1)`.

This reproduces M2 exactly: source `fsck` 0 → 128, `cat-file HEAD` fails.

## Decision

1. **Amend §4.1's recipe** to require mutation **before and after** `gc`, and to state
   that the `batch-check` assertion is on the command's **output**, never its exit
   status.
2. **Keep both non-vacuity mechanisms permanently.** The negative control asserts the
   hostile routine damages a default clone, running the *identical* assertion function
   the positive test uses inside `catch_unwind` and asserting it panics. If isolation
   ever silently stops being tested, that test fails and names the reason.
3. **Treat "prove the test can fail" as standing practice for every safety-critical
   test in this project**, not a one-off. Two slices, two tests that initially passed
   for the wrong reason, is a pattern rather than a coincidence.

## What this DOES prove

- The hostile routine, executed in the corrected order, genuinely damages a source
  repository reached through a hardlinked clone — on this git version, this filesystem,
  today.
- With `--no-hardlinks`, the same routine leaves the source byte-identical across
  `.git/config`, `show-ref`, the full object listing, and `fsck`.
- The test detects the removal of the isolation flag from production code.

## What this DOES NOT prove

- It does not prove isolation against attacks the routine does not perform. The hostile
  set is a lower bound on agent behaviour, not an upper bound — the same caveat S0
  recorded for the hook bypass set.
- It says nothing about isolation from a *different user* or a privileged process; the
  threat model is a same-user agent with a shell.
- M4's "`--no-hardlinks` is 2.5× faster" was **not** re-measured at S2. The only timing
  assertion is ADR-0001's 10 s revisit tripwire, which is not a comparative benchmark.
- APFS-specific. `nlink` semantics on another filesystem were not tested.

## Pre-registered falsification / revisit trigger

1. The negative control ceases to damage the source — meaning either git changed its
   hardlinking behaviour or the routine stopped being hostile. Either way the positive
   test's value is void until re-established.
2. A hostile step is added that the source-unchanged assertion cannot see. Any new step
   must be accompanied by evidence the assertion set can detect it.
3. `git clone` stops hardlinking by default, which would retire the negative control and
   require a different vacuity guard.

## Impacted master-plan sections

§4.1 (workspace isolation recipe and baseline surface) · §4.8 (`reconcile()` signature,
verdict precedence) · §2.5 (`WorkspaceProvider`) · Part 9 rows 14–15 · slice S2.
