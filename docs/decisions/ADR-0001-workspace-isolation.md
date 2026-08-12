# ADR-0001 — Run workspaces are `git clone --no-hardlinks`, not worktrees

**Status:** ACCEPTED
**Date:** 2026-08-12
**Slice:** pre-S0 (measured during architecture convergence, before this repository existed)

---

## Question

What is the isolation boundary between an agent's run workspace and the operator's
real repository?

## Why the answer matters to Conductor

This is the boundary that decides whether a misbehaving agent damages a scratch
directory or the user's actual work. Everything else in the security model assumes
it holds.

## Experiment / evidence

`scripts/measure/git_isolation_experiment.sh`. Builds a disposable source repository
with both loose objects and a pack, clones it three ways, compares inodes and link
counts, then performs an adversarial in-place byte mutation from inside the clone and
runs `git fsck` on the **source**.

Full measurements: master plan **Part 0, rows M1–M5**. Not restated here.

## Observed result

Summarized (numbers live in Part 0):

- A default local clone **hardlinks both loose objects and pack files** (same inode,
  `nlink=2`). Object files are mode `-r--r--r--`, but the same user owns them, so
  `chmod u+w` succeeds trivially.
- A 16-byte in-place write from inside such a clone **corrupted the source repository**:
  `git fsck` went from exit 0 to exit 128, and `git cat-file -p HEAD` failed.
- `--no-hardlinks` produces distinct inodes; the same mutation left the source at
  `fsck` exit 0 with all objects readable.
- `--no-hardlinks` is also **faster** than the hardlinking default at realistic repo
  size, which was the opposite of the expected trade-off.
- Deleting an object inside a hardlinked clone is safe — `unlink` only decrements the
  link count. The hazard is exclusively in-place mutation of a shared inode.

Also rejected, on documented git behaviour rather than measurement: **worktrees**,
which share `.git/config`, refs, hooks and the object store. A `pre-commit` hook
written by an agent inside a worktree would later fire under the operator's own hands.

## Decision

Run workspaces are created with:

```bash
git clone --no-hardlinks --no-checkout "$SOURCE" "$WORKSPACE"
git -C "$WORKSPACE" checkout -b "conductor/$TASK_ID/$RUN_ID" "$BASE_COMMIT"
git -C "$WORKSPACE" remote remove origin
git -C "$WORKSPACE" config core.hooksPath /dev/null
```

`--no-hardlinks` is **mandatory and there is no opt-out "fast mode"**, because the
measurement showed the safe option is also the fast one. Shipping a
faster-but-unsafe mode that is in fact slower would be indefensible.

## What this DOES prove

- The default local clone is not an isolation boundary against a same-user process.
- `--no-hardlinks` isolates the object store for the mutation class tested.
- The safety/performance trade-off assumed in earlier drafts does not exist at the
  tested scale.

## What this DOES NOT prove

- It does not prove `--no-hardlinks` isolates against *every* attack, only against
  in-place object mutation. Filesystem-level attacks outside `.git` are a different
  layer (ADR-0002).
- Timing was measured on APFS on one machine at one repository size. It is not a
  claim about other filesystems, sizes, or platforms.
- Worktree config/ref/hook sharing was **not** independently measured here. It is
  documented git behaviour and is no longer decision-relevant, since clones won on
  both safety and speed. Recorded as an unverified claim rather than kept as fact.

## Pre-registered falsification / revisit trigger

- A registered repository whose clone exceeds **10 s**. Revisit toward `--reference`
  with `--dissociate`, or a cached base clone refreshed by fetch. **Never** toward
  hardlinks.
- Disk pressure from concurrent runs becoming an operational problem.
- Evidence that `--no-hardlinks` does not isolate on a filesystem we care about.

## Impacted master-plan sections

Part 0 (M1–M5) · §4.1 · §11.2 · acceptance-suite rows 14, 15, 17.
