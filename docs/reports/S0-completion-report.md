# S0 — Completion Report

**Slice:** S0 — Measurement and pre-registered falsification
**Date:** 2026-08-12
**Status:** COMPLETE — stop point reached, S1 not started
**Authorized scope:** repository bootstrap + S0 only

---

## What was authorized, and what was done

Repository bootstrap (organization, operating contract, README) plus S0's two remaining
empirical questions. No product code. No crates. No schema. No S1 work.

## Repository state

**Start:** `/Users/krishverma/Documents/Conductor` was **not a git repository** — a loose
directory holding three architecture documents. The remote was at `6b830311`, one commit,
a single 11-byte `README.md`, no source, no schema, no migrations, no CI, no `.conductor/`.
Re-verified against `gh api` rather than taken from prior context.

The local directory was attached to the existing remote with `git init` + `git remote add`
+ `git fetch` + `git reset --mixed origin/main`, preserving the remote's history rather
than force-pushing a new root commit.

**End:** three commits on `main`, pushed, local and remote identical.

## Q1 — Claude Code hook reliability → ADR-0003

**Method.** Split deliberately so the expensive half stayed small: a free deterministic
corpus of 31 command strings against two classifiers, plus **two** `claude -p`
invocations in a disposable git repo with **no remote**, so an escaping `git push` fails
on its own.

**Result.**

| | |
|---|---|
| Fire rate | **10 / 10** Bash tool calls invoked the hook |
| Visibility | raw command string delivered intact, incl. `sh -c '…'` and `cd . && …` |
| Enforcement | 6 denials, **0** executions |
| Bypass | **2 / 2 tested indirection shapes passed the hook and reached `git`** |
| Audit | `system/hook_started` + `hook_response` events present with `--include-hook-events` |
| Hermeticity | `--settings` alone is **not** hermetic; `--setting-sources` is required |

Classifier corpus: naive substring matching caught 18/25 with **3 false positives**
(it blocks `grep -r 'git push' docs/`). A normalized classifier caught the same 18 with
**0 false positives**. The 7 shapes defeating both are exactly the class requiring shell
evaluation — substitution, backticks, variable indirection, pipe-to-shell,
write-then-execute, alias, encoded.

**Decision.** `tool_interception` stays `Restricted` and **non-gating** — now measured
rather than assumed. Hooks are adopted as an audit channel and accident speed bump, with
`matcher: "Bash"` and no `if` pre-filter.

## Q2 — SQLite claim safety → ADR-0004

**Method.** Disposable temp database, separate writer **processes**, explicit
`BEGIN IMMEDIATE`, four configurations, four invariants checked after every run, plus a
self-test that deliberately corrupts state to prove the checkers fire.

**Result — zero duplicate ownership in every configuration at every writer count.**

| config | writers | median | p95 | p99 | max | busy | **dup** |
|---|---:|---:|---:|---:|---:|---:|---:|
| baseline | 1 / 4 / 16 | 0.065 / 0.064 / 0.066 | 0.126 / 0.150 / 0.144 | 0.218 / 0.317 / 0.295 | 2.1 / 2.5 / 118.5 | 0 | **0** |
| contended | 1 / 4 / 16 | 0.079 / 0.139 / 1.333 | 0.154 / 3.84 / 10.02 | 0.338 / 9.93 / 91.30 | 6.8 / 115.7 / 677.3 | 0 | **0** |
| `fullfsync` | 1 / 4 / 16 | 2.733 / 2.780 / 3.626 | 3.18 / 42.7 / 151.4 | 3.82 / 116.8 / 778.1 | 7.5 / 777.6 / 1093 | 0 | **0** |

`--self-test`: all four invariant checkers correctly fire on deliberately corrupted state.

**Decision.** Adopt as specified. `UPDATE … RETURNING` with a subquery worked; no
fallback needed. Latency is not a constraint at Conductor's real load.

## Master-plan delta — four amendments, all forced by ADR-0004 and ADR-0003

This is the part that matters most, because S0 falsified two things the plan asserted.

1. **Part 5.1 pragmas — `synchronous=FULL` is not media-durable on macOS.** The plan
   justified `FULL` as "this database is the recovery record"; on macOS `FULL` issues
   `fsync()`, which does not flush the drive write cache. Added `fullfsync=1` and
   `checkpoint_fullfsync=1`. Measured cost: median 0.065 ms → 2.733 ms (~40×), which is
   irrelevant at Conductor's claim rate and is what the plan's own justification requires.

2. **§2.6 revisit trigger 3 was unscoped and already firing.** "p99 claim latency
   >100 ms" is exceeded under `fullfsync=1` at 4 writers (116.8 ms) and 16 (778 ms) — so
   as written it argues for adopting Temporal today, at a concurrency Conductor will
   never reach. Scoped to "≤4 concurrent writers with `fullfsync=1`, measured under
   `rusqlite`", with S1 required to re-measure.

3. **§6.3 hook integration** rewritten with the measured requirements: `matcher: "Bash"`
   with no `if`, mandatory `--setting-sources`, `--include-hook-events`, and the two
   confirmed bypasses named.

4. **Part 0** gained M18–M22.

Also corrected: `tool_interception` in §4.2 is now labelled measured rather than assumed.

## Verification

| Check | Command | Result |
|---|---|---|
| Classifier corpus | `python3 scripts/measure/s0_hook_classifier_corpus.py` | PASS — 31 cases, results written |
| Hook live probe (round 1) | `bash scripts/measure/s0_hook_live_probe.sh` | truncated at budget cap — see below |
| Hook live probe (round 2) | `ROUND=2 BUDGET=1.00 HERMETIC=1 bash …` | exit 0, 4/4 calls hooked |
| SQLite invariant self-test | `python3 …/s0_sqlite_claim_latency.py --self-test` | PASS — checkers have teeth |
| SQLite canonical run | `… --writers 1,4,16 --rows 2000 --repeat 3` | PASS — 0 dupes, all invariants |
| Independent reproduction | re-ran the subagent's script myself | reproduced; invariants PASS |
| Secret scan | `grep -rInE '(sk-\|gho_\|ghp_\|BEGIN .*PRIVATE KEY\|AKIA…)'` | clean |
| Nerve integrity | `git -C ../Nerve fsck` | exit 0, working tree clean, untouched |

## Things that went wrong, recorded rather than tidied away

- **Round 1 of the hook probe silently truncated.** `--max-budget-usd 0.50` terminated
  the run after 6 of 10 commands with `result: error_max_budget_usd`. The summary output
  looked complete; only reading the result event revealed it. The probe list was split
  into two rounds rather than raising the cap. A budget cap shortens an experiment
  invisibly.
- **The subagent's primary result file was clobbered** by my own concurrent re-run with
  different parameters. Regenerated at canonical fidelity and metadata verified. Shared
  output directories need provenance checks when more than one agent writes to them.
- **The negative control was uninformative.** `BEGIN DEFERRED` with the guard removed
  produced 1264 busy errors and still zero duplicates — WAL blocks that race
  structurally. The `--self-test`, not the negative control, is what proves the detector
  works. Kept in the record because it is a methodology lesson.
- **`busy_errors = 0` does not mean "no contention."** `busy_timeout` absorbs waiting
  into latency; max latency hit 1093 ms while the busy counter stayed at zero.

## What S0 does NOT prove

- Q2 measured **SQLite via Python 3.14.6 (SQLite 3.53.3)**, not `rusqlite` (which links
  a third build; the system CLI is 3.51.0). **S1 must re-measure.**
- No crash or power-loss injection — commit *cost* was measured, not crash survival.
- The lease-expiry / fencing reclaim path was never exercised (that is S3).
- The hook bypass set is a **lower bound**, not an upper bound: 2 of 7 predicted shapes
  were confirmed live, and the probe agent was cooperative, not adversarial.
- Baseline SQLite numbers are not representative at 16 writers — the queue drained
  before writers contended. Only the contended and fullfsync runs describe real contention.

## Open items carried into S1

1. Re-measure the claim transaction under `rusqlite` (ADR-0004 trigger 1).
2. Confirm `fullfsync=1` is actually applied by `rusqlite`, not silently dropped.
3. The five master-plan open items (Part 10.2) are unchanged; none blocked S0.

## Stop point

**S0 is complete. S1 is not started.** No crates, no `Cargo.toml`, no schema, no
workspace provider, no fake agent, no adapter, no daemon. Awaiting review before S1.
