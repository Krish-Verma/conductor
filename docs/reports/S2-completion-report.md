# S2 — Completion Report

**Slice:** S2 — Workspace isolation
**Date:** 2026-08-12
**Status:** COMPLETE — stop point reached (isolation proven, and proven non-vacuous)
**Starting commit:** `0716860`
**Ending commit:** see "Push status"

---

## Objective and scope

Create, describe, reconcile and quarantine per-run workspaces. This is the load-bearing
safety mechanism: everything downstream assumes an agent with a shell cannot reach the
user's real repository.

**Out of scope and confirmed absent:** agents, verification, policy, approvals, packets,
daemon. `/tmp` delta scanning and secret scanning are listed in §4.8's reconciled surface
but belong to S9 (the `/tmp` delta is meaningless before a per-run `TMPDIR` exists) —
they are left as **documented seams, deliberately not stubbed**, because a
`scan_secrets()` returning "none found" without looking would read as working.

## Files created

New crate `crates/conductor-git/` — deps `conductor-core`, `serde`, `serde_json`,
`thiserror`; dev `tempfile`. **No new dependency versions; no `libgit2`/`gix`** (§2.2
requires git as a subprocess so Conductor observes what the user observes).

| File | Purpose |
|---|---|
| `src/git.rs` | Subprocess runner. Returns raw bytes so `-z` NUL records survive; non-zero exit is *data*, not `Err`; env inherited unmodified |
| `src/clone.rs` | §4.1 sequence verbatim, submodule refusal, partial-workspace cleanup on failure |
| `src/descriptor.rs` | `.conductor-run.json` read/write + `exclude_descriptor_locally` |
| `src/baseline.rs` | `capture_baseline` (§4.1 surface) and `observe` (§4.8 surface + `RepoHealth`). All I/O lives here |
| `src/reconcile.rs` | **Pure** classifier: seven verdicts, findings, scope and sensitive-pattern matching |
| `src/quarantine.rs` | `find_orphans` / `quarantine`. **No delete path exists in the module, not even behind a flag** |
| `tests/` × 7 files | 78 integration + 10 unit = **88 new tests** |

`conductor-git` does **not** depend on `conductor-store`: `find_orphans` takes the active
`RunId` set as a parameter, which kept the crate store-free and testable without a
database.

## Verification — commands and results

All run by me, not taken from the implementing subagent's report.

| Check | Result |
|---|---|
| `cargo fmt --check` | **PASS** |
| `cargo check --all-targets` | **PASS** |
| `cargo clippy --all-targets --all-features -- -D warnings` | **PASS** (forced re-analysis by touching all sources) |
| `cargo test --all` | **140 passed, 0 failed** |
| S1 regression | 52 pre-existing tests unchanged and passing |
| `docs/`, `CLAUDE.md` untouched by the subagent | confirmed — `git status --porcelain docs CLAUDE.md` empty |
| `../Nerve` | not touched by this work; its 5 modified files are pre-existing user state |

## The acceptance test that matters — and its two independent non-vacuity proofs

**Positive test:** a hostile routine inside the clone sets a remote, deletes a branch,
writes a hook, mutates object files in place, runs `gc`, mutates again. The source
repository is asserted byte-identical across `.git/config`, `git show-ref` output, the
full `cat-file --batch-all-objects --batch-check` **output**, and `git fsck`.

**Proof 1 — negative control.** The same routine against a plain `git clone` damages the
source, measured:

```
report                : mutated_before_gc: 9, mutated_after_gc: 9, shared_inodes_seen: 9
source fsck before    : Some(0)
source fsck after     : Some(128)
object bytes identical: false
fsck stderr           : error: corrupt loose object '499fbb72…'
cat-file HEAD         : Some(128)
```

M2 reproduced exactly. The control runs the *identical* assertion function the positive
test uses, inside `catch_unwind`, and asserts it panics — so if isolation ever stops
being tested, that test fails and names the reason.

**Proof 2 — mutation test, which I reproduced myself.** Removing `--no-hardlinks` from
production code makes the positive test fail, showing the source's own objects becoming
unreadable:

```
assertion `left == right` failed: git cat-file --batch-all-objects --batch-check output changed
  left: "…6c23984f… missing / …bfa65511… missing / …d184003c… missing…"
 right: "…6c23984f… blob 28 / …bfa65511… blob 6 / …d184003c… tree 36…"
```

I performed this mutation independently, observed the failure, and restored the file.

## Two ways the plan's own recipe was vacuous → ADR-0006

1. **Step order.** §4.1 said "runs `gc`, **and** mutates object files in place". In that
   order the mutation proves nothing: `gc` runs `repack -d` + `prune-packed`, which
   **unlink** the shared objects, and unlink only decrements a link count. After `gc`
   there is no shared inode left to mutate. Must mutate **before and after**.
2. **Assertion shape.** `cat-file --batch-all-objects --batch-check` **exits 0 on a
   corrupt object store**, printing `missing`. The assertion is sound only because it
   hashes the *output*. "Tidying" it into an exit-status check would silently make the
   entire isolation test vacuous while leaving it green.

**This is now a pattern, not a coincidence:** S1's crash test and S2's isolation test
each initially passed for the wrong reason. ADR-0006 makes "prove the test can fail" a
standing requirement for every safety-critical test in this project.

## Every §4.8 verdict, and the test that constructs it

| Verdict | Constructed by |
|---|---|
| `NO_CHANGE` | `nothing_happened_is_no_change` |
| `CLEAN_COMPLETE` | `in_scope_changes_with_a_consistent_report_are_clean_complete` |
| `CLEAN_NO_REPORT` | `in_scope_changes_with_no_report_are_clean_no_report` |
| `OUT_OF_SCOPE` | `a_change_outside_the_declared_scope_is_out_of_scope` |
| `POLICY_SENSITIVE` | lockfile, migration, **row 14** (remote added with identical tree), committer identity, branch deletion, hook write |
| `CONTRADICTED` | **row 6** (success claimed over untouched tree); report naming a file git never saw change |
| `CORRUPT` | merge in progress; **row 15** (damaged object store); + 8 failure-injection tests |

Precedence itself is tested: `corrupt_outranks_every_other_verdict`, and **row 24**
(`a_passing_verification_does_not_soften_a_policy_sensitive_verdict`).

## Failure injection

Delete workspace mid-run · corrupt `.git` · leave `index.lock` · move the source repo —
11 tests. Each produces a defined outcome (`CORRUPT`, a finding, or a clean error);
none panics, none silently succeeds.

## Master-plan changes

1. **§4.1** — corrected hostile-test recipe (mutate before *and* after `gc`; assert on
   `batch-check` **output**, not exit status); baseline gains `nested_repos`; descriptor
   must be hidden via `.git/info/exclude`.
2. **§4.8** — `reconcile()` signature gains `scope` and `sensitive_patterns`; verdict
   precedence specified; nested-repo modification defined as finding-without-verdict.
3. **§2.5** — `WorkspaceProvider` revised from Trait to **Concrete for now**.
4. **S2 slice entry** — marked complete.
5. **ADR-0006** written and indexed.

### Two spec defects worth naming separately

- **The descriptor leaked into the user's repository.** Nothing in §4.1 said
  `.conductor-run.json` must be hidden from git. A test caught that `git add -A` commits
  it onto the run branch — the branch Conductor later *fetches into the real repo*. Fixed
  with `.git/info/exclude`; a `.gitignore` would not do, since adding one is itself a
  tracked change in the agent's diff.
- **`reconcile()` could not compute one of its own verdicts.** The four-argument
  signature contains no scope, so `OUT_OF_SCOPE` was unreachable.

### One deliberate disagreement with the plan

§2.5 listed `WorkspaceProvider` as a **Trait**, justified by "tests need a fake". Building
the tests falsified that: all 88 run real `git` against real disposable repositories,
which is strictly stronger evidence for an isolation boundary than a fake could give — a
faked clone cannot demonstrate that a real one does not share inodes. One implementation,
no fake needed, so CLAUDE.md's rule makes a trait here a pure function in a costume. Kept
concrete; promote when a second strategy or a genuine fake need appears.

## Known limitations

1. Hostile step set is a **lower bound** on agent behaviour, not an upper bound.
2. Threat model is a same-user agent with a shell — not a different user, not a
   privileged process.
3. APFS-specific; `nlink` semantics on other filesystems untested. Unix-only
   (`/dev/null`, mode bits).
4. **M4's "2.5× faster" was not re-measured.** The only timing assertion is ADR-0001's
   10 s revisit tripwire, which is not a comparative benchmark.
5. Glob matching is hand-written (`**`, `*`, `?`, literals) to avoid a new dependency;
   documented and unit-tested, fails closed on empty scope. If `globset` semantics are
   wanted for a security-load-bearing matcher, that is a one-line dependency swap.

## Process note

The subagent found an **untracked `crates/conductor-git/` left by the interrupted
session**. Unable to attest it was written test-first, it moved the directory aside
unread and rebuilt from zero under TDD. That is the correct call under the Iron Law, and
I have since deleted the set-aside copy.

## Rust falsification tracking (S1–S5)

| Metric | S2 value |
|---|---|
| Median `cargo check --all-targets` in the edit loop | **0.33 s** (6 samples); worst case 1.29 s when `conductor-core` changes |
| Commits primarily type/serde plumbing | 0 of 2 slice commits so far |

## Skills used

`superpowers:test-driven-development` and `superpowers:verification-before-completion`,
both invoked by the subagent this time (S1's report recorded their absence as a
deviation; the S2 prompt made invocation explicit and it was followed).

## Push status

Slice-scoped commit on `main`, pushed to `origin/main`. Working tree clean; local and
remote identical.

## Recommendation

**S2 COMPLETE — CONTINUING AUTOMATICALLY TO S2.5 (containment probe harness).**

S2.5 must carry the same lesson: its AF_UNIX probe requires a **positive control**, or a
denial cannot be distinguished from a broken test — the exact failure that invalidated
S0's first containment round.
