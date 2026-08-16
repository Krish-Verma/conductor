# S11 — Plan ledger and decisions — completion report

**Slice:** S11
**Branch:** `s11-plan-ledger`
**Starting `main`:** `03b786f` (S10)
**Status:** COMPLETE — stop point reached.

**Stop point (master plan Part 8):** *Project truth outlives execution state.*
**Verify line:** *Delete `conductor.db`, rebuild: no plan, decision, policy or
verification definition is lost.*

Both are met, and §7 below states exactly what "met" was proven with.

---

## 1. Task ledger

| # | Task | Commit(s) | What it settled |
|---|---|---|---|
| — | plan model, semantic content hash, §3.7 validation | `b93ce76` | The document, and §3.6's hash over parsed semantics rather than bytes |
| T1 | `Task.actions` + per-task `execution_requirements` | `603e995` | §4.2's block is one dialect at project and task level |
| T2 | schema v8, ledger store API, plan-state legality | `0f3d2c5`, `ded31ff` | §5.2's machine is the only thing that decides plan transitions |
| T3 | `project.yaml`, plan ledger, §3.3 controls 2 and 3 | `bda3a4b`, `4a6ef94` | Approval read only from the registered tree; store and file disagreeing halts |
| T4 | task materialization, acceptance row 21 | `d788fb6`, `6a1ab7b` | A run in flight finishes under the plan version it started on |
| T5 | append-only decisions | `99970a1` | Superseding a decision does not erase it |
| T6 | §4.3 action-binding gate | `84888a5` | The rule decided at S9 finally has a caller |
| T7 | `.conductor/**` unconditional rejection, row 29 | `f9bcdb7` | ADR-0014's eighth verdict |
| T8 | acceptance criteria, dependency gating, plan trailer | `b79afc6` | Three criteria that named S11 as owner |
| **T9** | **§3.5 database-loss reconstruction** | **`4f25a33`** | **The stop point. ADR-0015.** |
| T10 | `CODEX_HOME` on the real launch path | `d6636e9` | The credential boundary only the tests had built |
| T11 | `plan validate` / `plan approve` / `init` | `c6af236` | `plan approve` has one door |
| T12 | PID-identity fail-closed | `7153e11` | A start time that could not be read is not a start time |
| — | `Project.adapter` wired; adapter precedence | `808f34e` | A knob that did nothing now decides which agent runs |
| — | master-plan amendment audit applied | *(this commit)* | Five missing amendments, one stale status line, one absent ADR index |

18 commits ahead of `03b786f`. 90 files, +17,575 / −225.

---

## 2. T9 — the stop point, in detail

### 2.1 What was actually missing

Not a test. A **mechanism**.

Every step of §3.5's recovery path already had a function — `register_project`,
`register_plan_version`, `register_decisions`, `materialize`. What did not exist
was the **order**, and one step that exists nowhere else: **restoring approval**.

A §4.3 grant is a row in `conductor.db`. Once the store is gone, no witness
exists anywhere on the machine, so `ledger::approve` — which re-derives a
witness and refuses without one — cannot be replayed. Read literally, two
sections of the master plan then contradict each other:

- §5.2: *"`APPROVED` only via a human at the control socket."*
- §3.5: *"**Not lost:** every approved plan."*

### 2.2 The resolution — ADR-0015

The `APPROVED` sidecar is not an input to the approval decision; it is an
**output** of one. `ledger::approve` writes it *last*, after a real grant has
been re-derived and consumed. It is therefore a **receipt for a decision that
already happened at the socket** — §3.4's argument for commit trailers, applied
to approval.

`ledger::adopt_approval` re-reads a receipt. It does not mint authority, which
is why it takes no `Authorization` and spends no grant: there is no second
decision to authorize. It walks §5.2's machine (`VALIDATED → AWAITING_APPROVAL →
APPROVED`) rather than writing the state directly.

What stops that from being a second door onto `APPROVED`:

1. §3.3's control 1 — an agent's `.conductor/` writes are rejected at
   reconciliation and never fetched, so an agent cannot put a sidecar in the
   **registered** tree, which is the only tree `adopt_approval` reads.
2. Every field of the receipt is checked against something the receipt does not
   control: the version against its directory, the content hash against a
   **fresh** hash of the document, the timestamp against RFC 3339 (never
   defaulted to `0`). Every refusal happens *before* the state moves.

Residual risk is stated in ADR-0015 rather than mitigated: the receipt is exactly
as trustworthy as the registered working tree, and a party who can write that
tree could already run `plan approve`. The boundary is unchanged, not widened.

### 2.3 `conductor recover`

§7.1's thirteenth command, and the operator-facing door. `--scan` is **absent**
rather than accepted-and-ignored — the descriptor scan §3.5 also names is a
judgement about live execution state and **S14 owns it**. A flag that parses and
does nothing is the failure mode this slice spent a commit removing elsewhere
(`Project.adapter`); shipping one here would have been incoherent.

### 2.4 Evidence

**8 tests.** 5 in `crates/conductor-run/tests/plan_reconstruct.rs`, 3 in
`crates/conductor-cli/tests/recover.rs`.

Both halves of §3.5 are asserted, not just the flattering one:

| §3.5 list | Asserted |
|---|---|
| **Not lost** | project identity, the approved plan at the same content hash, the task list, decisions, policy hash, verification catalogue |
| **Lost** | runs, attempts, pending approvals, the event journal, and that the rebuilt task points at no run |

A rebuild that resurrected a `RUNNING` run with an expired lease would be
asserting a process that does not exist — worse than losing the row — so the
second list is a test, not a comment.

**The destructive step is total.** `conductor.db` **plus `-wal` and `-shm`**.
§5.1 runs SQLite in WAL mode, so deleting only the main file can leave committed
rows in the sidecar for SQLite to replay — which would have made the whole test
vacuous while looking green. Absence of all three is asserted, and the headline
test additionally asserts the fresh store knows nothing about the project before
reconstruction runs.

**Fresh process.** The CLI test drops its `Store`, destroys the files, and then
runs the real `conductor` binary as a separate OS process whose address space
never held any of it. What it does *not* claim is stated in the file's own
header: the setup half runs in the test's process, so this is "a fresh process
reconstructs", not "two independent processes hand state through the repository
alone".

---

## 3. Mutation / non-vacuity ledger

Every mutation was **verified present in the source** before its result was read
(the standing rule: a mutation that did not apply is an invalid experiment, not
a surviving mutant).

| Mutation | Applied? | Killed | Interpretation |
|---|---|---|---|
| Reconstruction never adopts a receipt | VERIFIED | 4 / 5 | Adoption is load-bearing. The survivor asserts only *absence* of execution state, which is still true — correct, not a gap. |
| `adopt_approval` drops the receipt↔document hash check | VERIFIED | 1 / 5 | Precisely the edited-plan test. The binding is what stops a rebuild laundering a changed document. |
| Reconstruction never syncs decisions | VERIFIED | 1 / 5 | Decision sync is load-bearing. |
| **`tasks` reassigned, `materialize` call left running** | **lexically applied, semantically not** | **0** | **INVALID EXPERIMENT.** The call still executed as a side effect on the store. Recorded as such. |
| `materialize` call actually removed | VERIFIED | 1 / 5 | Task rebuild is load-bearing. |

The fourth row is the reason this table exists. It survived, and reporting it as
"a surviving mutant" would have been a false finding about the product; the
defect was in the experiment.

### Carried forward: the `unwrap_or(0)` mutant (T12)

Reintroducing `unwrap_or(0)` in `supervise.rs` still survives all targeted tests,
and **no token assertion was added to kill it**. Classified as *test seam absent
for external I/O*: no unit test can deterministically drive a child into the
zombie state the branch exists for. The evidence retained is host-level and
measured — 300/300 start-time probes fail with `ESRCH` for an immediately-exiting
child; 0/300 for a 50 ms control. Option B of the two available (measured
host-level proof, no artificial seam). See §5.

---

## 4. Master-plan amendment audit

Standing rule: *a report saying the master plan changed ≠ the master plan
actually changed.* Every claim in every report (S0–S10) and all 14 prior ADRs was
checked against the current document.

**Result: 60 CONFIRMED · 5 MISSING · 4 PARTIAL · 1 AMBIGUOUS.**

Applied in this slice:

| Claimed by | Section | Was | Now |
|---|---|---|---|
| S10 | §6.1 | `parse_event → Option<AgentEvent>` — **contradicted all three shipped implementors** | `Vec<AgentEvent>`, with the reason (a Codex `file_change` carries an array; §4.8 reconciles against git, so no test could catch the dropped paths) |
| S4 | §4.5 | `conditional:` `commands:` shorthand had no `id`, yet `check_id` is a cache-key component | Both spellings specified; argv-list `command:` form specified |
| ADR-0003 | §4.9 | layer-4 row cited only M17 | cites the two confirmed bypasses (M19, §6.3) |
| ADR-0012 | §4.2 | eligibility block never said the refusal is a persisted state | says it, names `enforce/launch.rs` and §5.2 correction 5 |
| ADR-0009 | Part 5.1 | named "Part 5.1 (schema v5)" as impacted, on the assumption this block tracks the live schema | Part 5.1 now states once that it is the **v1** schema, that the store ships at **v8**, and where each delta is recorded |

**Closed without action:** S10's claimed §4.9 credential-directory amendment was
genuinely missing at S10 and was applied at S11 by `d6636e9` before this audit
ran. It is now present.

**Also corrected (nobody claimed these; they are honesty gaps found by the same
sweep):**

- The master plan's own header read **"design complete; no implementation has
  begun"** after eleven slices had shipped.
- Part 8 had no completion marker for **S9** or **S10** despite both shipping.
- `docs/decisions/README.md`'s index stopped at **ADR-0007**; 0008–0014 existed
  as files and were absent from the table.
- `CLAUDE.md` read "S0–S9 done, S10 not started".

**Deliberately not changed:** §7.1 lists `conductor approve` / `conductor deny`
while S8 shipped `conductor approval {…}`. S8 flagged this divergence explicitly
rather than reconciling it silently; it stays flagged and is **carried to the v1
acceptance sweep**, not resolved here.

---

## 5. Findings that outlived their task

Three defects found during S11 that were not in its scope line:

1. **`plan approve` authorized nothing.** A merely `VALIDATED` plan could
   materialize tasks. Fixed at T4 (`6a1ab7b`); `materialize` now gates on
   `PlanVersionState::Approved`, and the test asserts **both** directions —
   approved succeeds, validated-but-unapproved refuses — so an
   implementation that rejected everything cannot pass.
2. **`OUT_OF_SCOPE` could not express "unconditionally"** (ADR-0014). An agent
   editing `.conductor/policy.yaml` *and* a lockfile produced
   `POLICY_SENSITIVE`, which outranks `OUT_OF_SCOPE`, so a human approving the
   dependency would have let the governance edit advance too. Eighth verdict
   added.
3. **PID identity failed open.** A child exiting before the start-time probe is
   a zombie, and `proc_pidinfo` fails with `ESRCH` — deterministically, not as a
   race (300/300 vs 0/300 control). The old `unwrap_or(0)` fallback was read
   downstream as "wildcard, skip identity validation", so a startup-crashing
   adapter routinely entered a fail-open path. Fixed at T12.

And one carried from the slice's own audit:

4. **`Project.adapter` was a knob that did nothing** — parsed, refused when
   blank, folded into `config_hash`, never read, while `--adapter` defaulted to
   `fake`. Fixed (`808f34e`). Before: a repository with no `project.yaml` and no
   `--adapter` exited **0** having silently run the fake agent. Now exit **2**.

---

## 6. Verification

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | exit 0, zero warnings |
| `cargo test --all --no-fail-fast` (pre-T9 baseline) | 91 suites, **1108 passed, 4 failed, 3 ignored** |
| `cargo test -p conductor-run --test plan_reconstruct` | 5 / 5 |
| `cargo test -p conductor-cli --test recover` | 3 / 3 |
| `cargo test -p conductor-cli --test adapter_precedence` | 4 / 4 |
| `cargo test -p conductor-cli` (full) | 91 / 91 |
| `cargo test --all --no-fail-fast` (final) | exit 0 — **94 suites, 1124 passed, 0 failed, 3 ignored**, 0 panics |

### 6.1 The 4 failures, root-caused rather than re-run

All four were in `crates/conductor-cli/tests/containment.rs`, from **one** cause:

```
probe cannot be trusted: … conductor-probe-action cannot run unlaunched on this
host (Broken { reason: "timed out after 60s; a case that never finished says
nothing about containment" })
```

The host was saturated — a concurrent unrelated `cargo test` in another
repository plus this full workspace suite — and the containment probe's 60 s
timeout fired.

**Re-run alone on a quieter host: 6 / 6 pass in 15.9 s.**

Classification: **test-environment defect, not a product defect.** The product
behaved correctly under the condition — it **failed closed**, reporting
`ok: false`, exit 2, and "nothing measured through it would mean anything",
which is exactly the architectural invariant *"fail closed when stale"*.

What is genuinely wrong is the test: it requires an idle host and does not say
so. This is the same family as ADR-0005 (*claim latency is load-dependent*).
**Carried to the v1 acceptance sweep as a known load-sensitive suite**, not
closed here.

### 6.2 Final full-suite result

Recorded in the commit that closes this slice; see §8.

---

## 7. What this slice did **not** do

- **`conductor recover --scan`** — the workspace-descriptor half of §3.5's
  recovery path. **Bound to S14** (startup reconciliation, multi-project).
  The flag does not exist rather than existing inertly.
- **§7.1's `conductor approve` / `conductor deny` naming divergence** — flagged
  at S8, still flagged, carried to the v1 sweep.
- **Cryptographic binding of the `APPROVED` receipt to its grant** — would need
  a key with somewhere to live that an agent cannot read, which is §4.3's
  unsolved problem. Recorded in ADR-0015 as a stated residual risk.
- **Killing the `unwrap_or(0)` mutant with a unit test** — deliberately not
  done; see §3.

---

## 8. Skills and subagents

**Skills used:** `superpowers:executing-plans` (slice execution),
`superpowers:test-driven-development` (T9 and the adapter fix — RED verified for
the right reason in both cases before any implementation).

**Subagents used:** one general-purpose agent for the master-plan amendment audit
(§4). Its output was verified against the document rather than accepted: its
central finding — §6.1's `parse_event` signature contradicting all three shipped
implementors — was confirmed by reading the trait and the three `impl`s before
the amendment was applied.

**Host/process notes, distinguished from product defects:** the concurrent
unrelated build in a sibling repository was identified by parent-process lineage
and deliberately left running; it was not this session's to kill. The in-flight
suite inherited from the previous context was confirmed to belong to this session
by walking its PPID chain to the same `claude` process, rather than assumed.

---

## 9. Stop point

> **Project truth outlives execution state.**

Reached. `conductor.db`, `conductor.db-wal` and `conductor.db-shm` are deleted; a
separate process, given only the repository, rebuilds the project, every plan
version, the approval that a human granted at the socket, the decisions and the
task list — and does **not** rebuild the runs, attempts, approvals-in-flight or
event journal that §3.5 says should die with the store.

**S11 is complete. S12 (packets and reports) is unlocked.**
