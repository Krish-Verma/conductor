# S1 — Completion Report

**Slice:** S1 — Store foundation
**Date:** 2026-08-12
**Status:** COMPLETE — stop point reached (`conductor doctor` green)
**Starting commit:** `864ef1e` (S0 end state, verified against `origin/main`)
**Ending commit:** see "Push status" below

---

## Objective and scope

Schema v1, forward-only migrations, transaction discipline, the atomic claim, and
`conductor doctor` — plus the re-measurement ADR-0004 pre-registered as mandatory for
S1, because S0 measured SQLite-via-Python rather than the shipping stack.

**Out of scope and confirmed absent:** policy, verification, approvals, packets, agents,
workspaces, daemon, socket server. `doctor` *reports on* the socket directory; it does
not implement one. No `conductor-git`, `conductor-agent` or `conductor-run` crate was
created — CLAUDE.md forbids skeleton directories for slices that have not begun.

## Files created

| Path | Purpose |
|---|---|
| `Cargo.toml` | Workspace, 3 members, edition 2024 |
| `crates/conductor-core/src/ids.rs` | 7 ID newtypes; blank rejected; deserialize goes through the constructor |
| `crates/conductor-core/src/state.rs` | `TaskState` (12), `RunState`, `PlanVersionState` (5), `AttemptOutcome` (5) |
| `crates/conductor-core/src/event.rs` | `EventKind::RunClaimed` + payload — the only event S1 writes |
| `crates/conductor-store/src/schema.rs` | Part 5.1 DDL + pragma set/expected tables; `apply_pragmas` fails closed |
| `crates/conductor-store/src/migrate.rs` | Forward-only migrations, one `BEGIN IMMEDIATE` each, `integrity_check` after each |
| `crates/conductor-store/src/tx.rs` | `with_immediate`: commit on `Ok`, rollback on `Err`, rollback failure never hides the original |
| `crates/conductor-store/src/claim.rs` | §4.7 claim verbatim + `RUN_CLAIMED` insert in the same transaction |
| `crates/conductor-store/src/lib.rs` | Concrete `Store` (no trait, per §2.5) |
| `crates/conductor-store/src/bin/conductor-kill-victim.rs` | SIGKILL failure-injection helper |
| `crates/conductor-store/src/bin/conductor-claim-bench.rs` | Concurrency instrument + measurement harness + self-test |
| `crates/conductor-cli/src/{main,doctor}.rs` | `conductor doctor`, §7.2 exit codes |
| `scripts/measure/s1_rusqlite_claim_latency.sh` | Reproducible measurement driver (5 configurations) |
| `docs/decisions/ADR-0005-*.md` | The re-measurement and the trigger re-scoping |

4,582 lines total, of which 1,146 is the measurement instrument and ~1,400 is tests.

## Verification — commands and results

All run by me, not taken from the implementing subagent's report.

| Check | Command | Result |
|---|---|---|
| Format | `cargo fmt --check` | **PASS** |
| Type check | `cargo check --all-targets` | **PASS** |
| Lint | `cargo clippy --all-targets --all-features -- -D warnings` | **PASS**, forced re-analysis by touching all sources (a cached "Finished" is not evidence) |
| Tests | `cargo test --all` | **52 passed, 0 failed** |
| Stop point | `conductor doctor` | **green, exit 0**, all six pragmas confirmed in effect |
| Exit codes | `conductor doctor` with no store | exit **2** per §7.2 |
| DDL fidelity | mechanical diff of `SCHEMA_V1` against master plan Part 5.1 | **22/22 statements exact match**, zero divergence |
| Secret scan | `grep -rInE '(sk-…\|gho_\|ghp_\|AKIA…\|BEGIN .*PRIVATE KEY\|xox[baprs]-)'` | clean (only hit is S0's report quoting the pattern) |
| Architecture audit | `WorkflowRuntime\|Temporal\|Hatchet\|libgit2\|gix\|worktree\|hardlink\|RepositoryEvidenceProvider\|deny_unknown_fields\|Nerve` over `crates/` | **0 matches each** |
| Dependencies | vs master plan §2.2 authorized list | exact subset; **`tokio` deliberately not added** (supervision is S3) |

**Independent-verification note.** The DDL match, the clippy re-analysis, the full test
run, the doctor stop point and the entire re-measurement were executed by me directly.
The subagent's claims were treated as claims.

## Failure injection

**`SIGKILL` mid-transaction × 100 cycles** — passes, and the test is non-vacuous by
construction:

- the victim signals `READY` only once its transaction holds the write lock and has
  written rows that must not survive;
- the parent asserts the WAL exceeded 64 KiB **before** killing, so uncommitted pages
  genuinely reached disk and there is something to roll back;
- the parent asserts death by signal 9;
- after each kill: `integrity_check` = ok, 0 FK violations, table counts identical to
  baseline, no event row bearing the cycle's marker;
- after 100 kills the store is still *usable*, not merely intact — a fresh run is seeded
  and claimed.

This mattered: the first implementation passed in 1.05 s because SQLite held every dirty
page in memory, so the kill had nothing on disk to roll back. **A crash test that passes
because nothing was at risk is not a crash test.** Strengthened (`cache_size=-16`,
`cache_spill=1`, 4000 rows) it runs in ~8 s and exercises real WAL rollback.

**Honesty on the slice's "survives simulated power loss" line.** It is **not satisfied**.
`SIGKILL` destroys a process but does not drop the OS page cache or the drive write
cache — precisely what `fullfsync=1` exists to defeat. What is proven is **crash
atomicity**. Genuine power-loss testing needs hardware or VM-level fault injection not
available on this host. Recorded in ADR-0005 and in the master plan's S1 entry rather
than quietly satisfied.

## Mandatory re-measurement (ADR-0004 trigger 1) → ADR-0005

`rusqlite` 0.40.2 / **SQLite 3.53.2** bundled (S0: 3.53.3 via Python; system CLI 3.51.0).
Every claim goes through the **production** `Store::claim_next_run` and
`Store::open_existing` with production pragmas — no reimplementation, which was the
specific mistake ADR-0004 flagged. Separate writer **processes**, matching S0.

**39,400 claims across 5 configurations and 1/4/16 writers: zero duplicate ownership,
zero invariant failures, `claims == rows` exactly.**

`fullfsync=1` (ships), 4 writers, as offered load varies:

| inter-claim gap | median | p99 | max | busy | dup |
|---|---:|---:|---:|---:|---:|
| ~1 ms (saturation) | 3.44 | **265.3** | 875.8 | 0 | **0** |
| 25 ms | 3.54 | **24.6** | 67.0 | 0 | **0** |
| 250 ms | 4.44 | **48.7** | 85.4 | 0 | **0** |

**The finding: p99 tracks offered load, not the mechanism.** Median stays flat at
3.4–4.4 ms while p99 moves 265 → 24.6 ms. That is queueing at ~100% utilisation of a
deliberately serialised writer.

**Trigger disposition:**

- **Trigger 1 — does not fire on the shipping configuration.** No duplicate claim
  anywhere; medians within 1.06–1.24× of S0. Partially fires on the *non-shipping*
  `fullfsync=0` comparison (5.03× at 1w, 2.57× at 4w), attributed — as a hypothesis, not
  a decomposition — to the S1 transaction doing strictly more work than S0's harness.
- **Trigger 2 — fires as literally worded; the wording was defective and is amended.**
  It omitted offered load, so any saturating benchmark satisfies it, and it would then
  recommend Temporal — whose durable write is a Postgres round-trip plus a network hop,
  making the measured quantity *worse*. A trigger whose firing recommends an action that
  worsens what it measures is measuring the wrong thing. At the load it is actually
  about: **24.6 ms**.
- **Trigger 3 — 6 `SQLITE_BUSY` observed, but at 16 writers under saturation**, which is
  not "normal operation". Zero at ≤4 writers in every configuration.

**The number that must not be lost:** worst-case claim latency moved 1093 ms → **5147 ms**.
ADR-0004 explicitly coupled lease duration to the worst-case starvation window, so the
60 s lease margin narrows from ~55× to **~12×**. Still safe; moved by 5× under a stack
change. S3 owns leases and must not shrink the lease without re-deriving this.

I tuned nothing to avoid a fire. The instrument carries the self-test ADR-0004 made
binding, and the S1 self-test is stronger than S0's — I4 now also detects a deliberately
corrupted database file, not merely `ok` on a sound one. I ran it and watched all six
checkers fire.

## Master-plan changes

1. **Part 0** — added measured facts **M23–M27**.
2. **§2.6 revisit trigger 3** — re-scoped to name the arrival rate; saturation numbers
   retained in ADR-0005 as the ceiling.
3. **Part 5.1** — `ix_event_run` is now **UNIQUE**.
4. **§4.7** — recorded the `RECOVERING` contradiction (below) and the measured
   lease/starvation coupling.
5. **§5.2** — "Attempt (7 states)" corrected to 8; recorded the missing `attempt.state`
   column.
6. **S1 slice entry** — marked complete, with what was *not* proven stated inline.

### Schema deviation, deliberate and recorded

`event` gained `UNIQUE(run_id, seq)`. S0's harness carried this constraint and used it as
a hard double-claim tripwire; the DDL that reached the master plan had lost it. Restored
by TDD (test written first, observed failing with the duplicate insert returning `1` row
modified) while the schema has **zero deployed instances**, so no migration is owed. A
duplicate claim is now an `INSERT`-time error rather than something only an offline
checker would notice.

### Two contradictions found in the spec, routed to S3 rather than guessed

1. **`RECOVERING` is not a state.** §4.7's claim selects
   `state IN ('READY','RECOVERING')`, but §5.2's task machine has no `RECOVERING`, and
   the run state "mirrors its task" — so half the claim's own predicate can never match.
   §5.2's restart rule forces crashed runs to `RECONCILING`, which is almost certainly
   the intended predicate. **S1 shipped the statement verbatim rather than guessing**,
   because the reclaim path is S3's design surface and the measurement had to measure the
   statement as specified. Safe today only because nothing writes `RECOVERING`.
2. **`attempt` has no `state` column** — only `outcome` (5 values). `CREATED`,
   `STARTING` and `ACTIVE` are unpersistable, so a supervisor cannot record that an
   attempt is in flight, which is exactly what startup recovery must read. S3 owns the
   forward migration.

Both are recorded in the master plan at the point of contradiction, not only here.

## Rust falsification tracking (S1–S5)

| Metric | S1 value |
|---|---|
| Median `cargo check --all-targets` in the edit loop | **0.33 s** (9 samples; min 0.27, max 0.55) — threshold is 90 s, so ~270× inside |
| Cold full-workspace check | 7.83 s |
| Cold `rusqlite` bundled build (one-off C compile) | ~46 s |
| Commits primarily type/serde plumbing | 0 of 1 S1 commit — needs S1–S5 history to be meaningful |

The check-time half of the trigger is nowhere near firing. The plumbing-ratio half is not
yet evaluable from one slice.

## Skills used

- `superpowers:test-driven-development` — invoked before implementation; red-green-refactor
  was followed for the schema change I made directly (duplicate-`seq` test observed failing
  first).
- `superpowers:executing-plans` — invoked; the master plan is the plan under execution.
  Its worktree step was deliberately not taken: the authorization directs slice-scoped
  commits and pushes on `main`, which is the established S0 workflow.

**Deviation to record honestly:** the implementing subagent did **not** invoke the skills;
it followed the equivalent discipline by hand because the slice prompt and CLAUDE.md
mandate it. CLAUDE.md asks for the skill to be used rather than approximated. Subagent
prompts for S2 onward will name the skills explicitly.

## Subagent work and review

One implementation subagent (opus) carried Deliverables 1–8. I reviewed by: mechanically
diffing the DDL against Part 5.1; reading `claim.rs`, `tx.rs`, `schema.rs` and
`kill_injection.rs` in full; forcing clippy re-analysis; running the whole suite; running
the self-test; and re-running every measurement myself after my schema change so the
evidence matches shipping code rather than a superseded schema.

Two subagent proposals I accepted (`event` UNIQUE, `doctor --init-store`); one I declined
for now (changing the claim predicate — S3's call, see above).

`doctor --init-store` is **new CLI surface not in §7.1**. It exists because doctor must be
able to *report* on a store without creating one, while the S1 stop point requires a green
doctor. Revisit at S11 when `conductor init` lands, where store creation belongs more
naturally.

## Known limitations

1. Power-loss durability unproven (crash atomicity only) — see above.
2. Trigger-1 attribution on the non-shipping `fullfsync=0` config is a hypothesis.
3. 16-writer configurations remain non-representative in ADR-0004's sense: only
   12.5–13.0 of 16 writers ever claimed before the queue drained.
4. `conductor-store` ships two `src/bin` targets (the instruments); they would be
   installed by `cargo install`. Move behind a feature or into a test-support crate if
   that becomes a distribution concern.
5. Nothing here exercises lease expiry, fencing conflict or recovery — S3.

## Push status

Slice-scoped commit on `main`, pushed to `origin/main`. Working tree clean; local and
remote identical.

## Recommendation

**S1 COMPLETE — CONTINUING AUTOMATICALLY TO S2 (workspace isolation).**

S2's acceptance test must be demonstrably non-vacuous in the same way the kill test had
to be made non-vacuous: the hostile-workspace test must be shown to **fail** against a
default hardlinked clone (M2), not merely pass against `--no-hardlinks`.
