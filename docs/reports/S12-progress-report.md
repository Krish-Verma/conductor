# S12 — Packets and reports — **PROGRESS** report (slice NOT complete)

**Slice:** S12
**Branch:** `s12-packets`
**Base:** `c10ede5` (S11 on `main`)
**Status:** ⚠️ **IN PROGRESS — the stop point has not been reached.**

This is deliberately not a completion report. §7 below states exactly what is
missing and why it is not "nearly done".

**Stop point (Part 8):** *Recovery does not depend on hidden state.*
**Verify line:** *An agent handed **only** a continuation packet completes a task
interrupted mid-way, on a fixture, **with no session resume**.*

The Verify line has **not** been executed. Nothing in this report should be read
as evidence that it passes.

---

## 1. What is done, and verified

| §6.5 / §6.6 requirement | State | Evidence |
|---|---|---|
| Canonical, deterministic packet serialization | DONE | `packet::canonical_bytes`, sorted keys, type-tagged, length-prefixed |
| Its own digest domain (not the plan's) | DONE | `PACKET_CANONICAL_VERSION`, unit test |
| Implementation packet, every §6.5 field | DONE | `packet::implementation` |
| Continuation packet = implementation + observed reality | DONE | `packet::continuation` |
| §6.5's verbatim "previous agent's reasoning is not available" | DONE | `NO_PRIOR_REASONING` constant |
| Context minimization (explicit refs) | DONE | ADR-0016 |
| Evidence linked by path **and** digest, never embedded | DONE | `packet::Evidence::linked` |
| Size budget enforced as a **refusal**, never truncation | DONE | `MAX_PACKET_BYTES`, `emit()` |
| Byte-identical packet from a *separately built* store | DONE | see §3 |
| Continuation regenerable after total process restart | DONE (as determinism) | see §7 for the caveat |
| Repair packet composed onto the implementation packet | **NOT DONE** | §7 |
| Agent report schema as a shipped artifact | **NOT DONE** | §7 |
| Packet reaches a real agent | **NOT DONE** | §7 — this is the big one |
| Verify line executed | **NOT DONE** | §7 |

**11 tests**, all passing, in `crates/conductor-run/tests/packet.rs`, plus 4 unit
tests in `packet::tests`. `cargo fmt --check` clean; `cargo clippy --all-targets
--all-features -- -D warnings` exit 0.

---

## 2. Two architectural findings

### 2.1 §6.5's context minimization had only one implementable half — ADR-0016

§6.5 selects decisions *"by touching the task's scope globs or explicit refs"*.
**Neither existed.** Scope matching requires the *decision* to declare a scope,
and §3.6 fixes decision frontmatter at four fields under `deny_unknown_fields` —
adding `scope:` would contradict §3.6 and change the content-hash preimage of
every decision already written. Explicit refs had no field either, but a plan
document has no `deny_unknown_fields`, so the reference can point from the plan.

Implemented as `plan::model::Task::decisions`. A ref that does not resolve is a
**refusal naming both ids**, not a silent omission — a well-formed packet missing
an argument the plan says the task needs is the failure nobody would see.

### 2.2 The plan ledger and the product's core verb are not connected

Found while looking for where to hand the packet to an agent. **`conductor task
run` does not use the plan ledger S11 built.** It runs off the S5-era
`.conductor/task.yaml` spec and *fabricates* a synthetic plan version:

```rust
// crates/conductor-cli/src/task.rs, ensure_task_and_run
// DRAFT, never APPROVED — the row exists to satisfy `task.plan_version_id`,
// and it says exactly what it is: the task-spec file S11 will replace.
INSERT OR IGNORE INTO plan_version
  (id, project_id, version, content_hash, state, source_path)
VALUES (?1, ?2, 0, ?3, 'DRAFT', '.conductor/task.yaml')
```

`version 0`, `state DRAFT`, `source_path .conductor/task.yaml`. The comment
predicted its own replacement and the replacement never happened.

**Consequences, stated plainly:**

- `packet::implementation::build` cannot run against a spec-created run: it reads
  the plan document at `.conductor/plans/v0/plan.yaml`, which does not exist.
- S11's materialisation, approval gate and row-21 pinning are reachable through
  the library and through `conductor recover`, but **not** through `task run`.
- **S16's dogfood requirement is blocked by this.** The master prompt's rule for
  dogfooding is "use the real integrated path … do not direct-call internal
  functions if the acceptance criterion is meant to prove integrated product
  behaviour." As things stand there is no integrated path from an approved plan
  to a running agent.

This is the single most consequential open item in the project right now, and it
is larger than "wire up a packet". It is properly S12's, because S12 is the slice
that has to hand a plan-derived packet to an agent — but it was not in S12's
scope line, and it should be tracked as its own piece of work.

---

## 3. Non-vacuity ledger

Every mutation was **verified present in the source** before its result was read.

| Mutation | Applied? | Killed | Interpretation |
|---|---|---|---|
| Canonical encoder stops sorting map keys | VERIFIED | `map_key_order_cannot_reach_the_bytes` | Sorting is load-bearing |
| `emit()` stops enforcing the ceiling | VERIFIED | oversized-refusal test | The budget is real |
| `Evidence::linked` embeds the bytes it hashes | VERIFIED | size-budget test | "Linked, never embedded" is load-bearing |
| Carry every decision instead of the referenced ones (attempt 1) | **did not compile** | — | **INVALID EXPERIMENT** |
| Same, in a compiling form | VERIFIED | context-minimization test | Selection is load-bearing |
| Drop the `.conductor/**` union (attempt 1) | VERIFIED | **nothing — SURVIVED** | **The test was vacuous** — see below |
| Same, after adding a fixture that declares nothing | VERIFIED | new governance test | The union is load-bearing |

**The surviving mutant was a real finding about the test, not the product.**
`the_packet_carries_every_field_section_6_5_names` asserted `.conductor/**`
appears in the packet — but the fixture's `project.yaml` already declared it
under `scope_defaults`, so the assertion passed whether or not Conductor added
it. §3.3 requires the path to be forbidden *"regardless of what any plan says,
precisely because the plan is a file the agent can edit"*, and only a fixture
that declares nothing can test that. `the_governance_path_is_forbidden_even_when_no_document_says_so`
is that test, and it fails under the mutation.

**Determinism is tested against a second store, not a second call.** Calling one
function twice in one process proves almost nothing — it is satisfied by any pure
function of values already in memory, including one that captured a timestamp
once. The test builds the same durable state in two independently created stores,
in two directories, **at two different wall clocks** (`1_000` and `9_999_000`),
and demands identical bytes.

---

## 4. Verification

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | exit 0, zero warnings |
| `cargo test -p conductor-run --test packet` | 11 / 11 |
| `cargo test --all --no-fail-fast` | exit 0 — **95 suites, 1139 passed, 0 failed, 3 ignored**, 0 panics |

The gate was run twice. The first attempt was **killed** at 19 suites when its
wrapper process was terminated, and a monitor watching for the process to
disappear reported it as finished. That is the process-attribution error this
project has hit before, and 19 suites / 185 tests was **not** recorded as a
passing gate. The second run was detached with `nohup` so a wrapper kill could
not take it down, and it is the one reported above — confirmed complete by its
tail (`Doc-tests conductor_store`, cargo's last step; a killed run ends
mid-`Running`) and by arithmetic against S11's gate: exactly +1 suite
(`tests/packet.rs`) and +15 tests (11 integration + 4 unit).

---

## 5. Files

```
crates/conductor-run/src/packet/mod.rs             canonical form, budget, evidence
crates/conductor-run/src/packet/implementation.rs  §6.5's implementation packet
crates/conductor-run/src/packet/continuation.rs    §6.5's continuation packet
crates/conductor-run/tests/packet.rs               11 tests
crates/conductor-run/src/plan/model.rs             Task::decisions (ADR-0016)
docs/decisions/ADR-0016-...md                      the context-minimization ruling
```

---

## 6. Master-plan amendment

§6.5's context-minimization paragraph gains a note recording that only "explicit
refs" is implemented, which half is not, and why — so a later reader does not
read the gap as an oversight and "fix" it by adding `scope:` to decisions.

---

## 7. What is NOT done — the honest list

1. **The Verify line has not been run.** No agent has been handed a continuation
   packet and asked to finish an interrupted task. The packet is *built* and
   *proven deterministic*; it has never been *used*. This is the stop point, so
   **S12 is not complete.**
2. **The packet does not reach any agent.** `conductor task run` still passes
   `spec.objective()` — a single string — as the Codex prompt. Blocked on §2.2.
3. **`conductor task run` does not use the plan ledger** (§2.2). This is the
   prerequisite for 1 and 2 and for S16's dogfood.
4. **The repair packet is not composed onto the implementation packet.** §6.5
   says the repair packet *"adds only"* its six fields to the implementation
   packet; `repair::packet` (S6) still emits only the added fields, with a doc
   comment deferring the rest to S12.
5. **`schemas/agent-report.v1.json` does not exist** as a file. The packet
   references it by path (`REPORT_SCHEMA_PATH`) and `conductor_agent::codex`
   carries a `REPORT_SCHEMA_JSON` constant, but the two have not been reconciled
   into one artifact, and §6.5's report shape has not been checked against the
   constant field by field.
6. **"Regenerable after total process restart" is proven only as determinism.**
   Two stores in one process produce identical bytes. A genuinely separate
   process rebuilding the packet — the shape `conductor-cli/tests/recover.rs`
   uses for S11's T9 — has not been written.
7. **No secret-safety test for packets.** The master prompt requires disposable
   canary secrets through packet/report/log surfaces; `verify::secrets` exists
   and is not yet applied to packet emission.

---

## 8. Recommended next step

Take §2.2 as its own piece of work before finishing S12: make `conductor task
run` claim a task materialised from an approved plan version, so the plan ledger,
the packet, the eligibility gate and row-21 pinning are all on one path. Items
1, 2 and 4 above become straightforward once that exists, and S16 cannot be
honest without it.

---

## 9. §2.2 is DONE — ADR-0017

Recorded here rather than by editing §7's list above, so that the honest list
stays the honest list as it stood.

`conductor task run` now claims a task materialised from an **approved** plan
version. `conductor_run::plan::runnable` is the single place that decides whether
a task may run; `conductor plan approve` materialises, because approval is the
event that changes what work exists; `--spec`, `conductor_run::spec` and
`conductor_core::task::{TaskSpec, ValidatedTaskSpec, TaskSpecError}` are deleted.
**11 tests** in `crates/conductor-cli/tests/task_run_plan.rs`, every one through
the shipped binary, with a positive control for every refusal.

Seven mechanisms that were implemented and unreachable from the core verb are
reachable: §4.3's approval gate · both §4.2 gates (`declared_actions` and
`execution_requirements` were `NULL` on every real run) · §5.2's dependency edge ·
§5.2's restart clause · §3.3 control 2 at run start · row 21's supersession.

**Four further defects the fix exposed**, all fixed, all recorded in ADR-0017:

1. Row 30's persisted `BLOCKED` state was invisible at the CLI boundary — every
   `run_task` error mapped to §7.2's `1`, so a script could not tell an ineligible
   execution mode from a crash and `--json` printed nothing. The store now decides
   the exit code.
2. The vertical claimed *"the next run"* rather than this task's run, which a
   materialised plan makes reachable without an adversary.
3. `task run` on a terminal task created a second `READY` run before failing.
4. Deleting the task spec orphaned two of its five refusals; `blank_objective` and
   `non_positive_attempt_budget` are restored as validator rules, and the
   empty-scope one is deliberately not (it already fails closed in
   `conductor_git::Scope`).

**One new finding, not previously recorded anywhere.** §7's item 5 says the report
schema does not exist as a file. The larger half is that §6.5's documented report
shape and the shipped one **disagree**: §6.5 specifies `task_id` / `status`
(`complete|partial|blocked`) / `files_changed` / `commands_run` /
`acceptance_criteria` / `deviations` / `blockers` / `unverified_claims`, while
`conductor_agent::codex::REPORT_SCHEMA_JSON` and `conductor_core::AgentReport`
ship `claim` (`COMPLETE|PARTIAL|FAILED`) / `files_touched` / `summary`. Different
field names **and** a different status vocabulary. Separately,
`packet::implementation::REPORT_SCHEMA_PATH` is the bare relative string
`schemas/agent-report.v1.json`, which in a generated packet resolves against the
*user's* repository, where no such file exists — while the schema actually handed
to the agent is written to `<artifacts>/<run>/agent/report-schema.json`. A packet
field naming a path that does not resolve is the failure nobody sees, which is
exactly ADR-0016's reasoning about unresolvable decision refs.

---

## 10. §7's items 2, 4, 5 and 7 are DONE — ADR-0018

§7's list said the packet does not reach any agent, the repair packet is not
composed, the report schema is not a file, and there is no secret-safety test.
All four are now done, and the reason they were all still open was one finding:

> **The packets were built and never delivered.** `task run` passed
> `spec.objective()`. Worse, `repair::driver` built the repair packet, returned it
> in `Attempted { packet }` for *reporting*, and launched the attempt with
> something else — so §6.5's `do_not_retry` list, *"what stops attempt 2 from being
> attempt 1 again"*, had never been seen by attempt 2. S6's own test asserted the
> packet's contents from that returned value; its name claimed delivery. The name
> was the tell, and it passed for two slices.

* **`StartInput` carries the instruction per-attempt** (`with_prompt` deleted). A
  packet cannot exist before the workspace does, and §4.6 gives attempt 2 a
  different one.
* **All three of §6.5's packets now have consumers**, and which one an attempt gets
  is decided by *how the previous attempt ended* — row 2 (crashed, nothing
  survived) gets the implementation packet, row 3 (crashed, work survived) the
  continuation packet, row 7 (exited, verification failed) the repair packet. The
  verification result cannot be the discriminator: a crash whose surviving work
  fails a check is `ObservationKind::Failed`, indistinguishable from row 7 by kind.
  Row 2's boundary is measured by re-observing the workspace against the stored
  baseline, not read off §4.8's table.
* **Every attempt stores `packet.yaml`** beside its `report.json`, as a plain
  artifact rather than through the side-effect ledger — a packet is a pure function
  of durable state (§6.6), while a baseline is a measurement that cannot be re-taken.
* **Secrets are redacted** at one chokepoint both the YAML and the hashed bytes pass
  through, so the digest names what was delivered.
* **`schemas/agent-report.v1.json` exists**, bound to `REPORT_SCHEMA_JSON` by
  `include_str!`; the packet's `report_schema` is its `$id` rather than a path that
  resolved for nobody.

**Three findings about the tests**, all exposed by the delivery, none a product
defect: run fixtures registered `root_path = '/fixture'`; acceptance rows 13 and 14
were passing because the pinned policy snapshot was *undecodable*, so the gate
failed closed — the right answer by the wrong mechanism; and there were two
verification profiles, the product reading the one nothing wrote.

**Gate:** 96 suites, 1158 passed, 0 failed, 3 ignored, 0 panics (S11: 94 / 1124).
`fmt` clean; `clippy --all-targets --all-features -D warnings` clean.

---

## 11. The Verify line — RUN, and non-vacuous

> An agent handed **only** a continuation packet completes a task interrupted
> mid-way, on a fixture, **with no session resume**.

`s12_verify_line_a_fresh_agent_finishes_from_the_continuation_packet_alone`, in
`crates/conductor-run/tests/repair_loop.rs`. Each clause is *arranged*, not asserted
after the fact:

| clause | how it is made true |
|---|---|
| *interrupted mid-way* | attempt 1 writes `src/scratch.rs` and is `SIGKILL`ed; work survives, no report exists, and the required check still fails so the run stays alive |
| *a continuation packet* | Part 9 row 3's routing, and the worker stores it at `artifacts/<run>/2/packet.yaml` |
| **handed *only*** | attempt 2's scenario is a single `finish_from_packet` step, which takes **no parameters** |
| *no session resume* | `session_resume: false` on the adapter, **and** the assertion reads `attempt.agent_session_id` out of the store for every attempt |

The *"only"* clause is the one that needed a new mechanism. Every other fake-agent
step is scripted — the scenario file says what to write — so a test built from them
would have proved the *scenario* was sufficient and said nothing about the packet;
an empty packet would have passed just as well. That is ADR-0006's vacuity, and it
is why `Step::FinishFromPacket` takes no arguments: it opens
`CONDUCTOR_FAKE_PACKET`, refuses unless the document carries an objective, a scope
and acceptance criteria, and derives the file to write from the packet's own
objective. A packet that stopped being sufficient breaks the test and names which
field it lost.

**Mutation, verified applied.** Disabling the continuation route in
`repair::driver` (`} else if false && unfinished {`, confirmed present in the source
before the run) killed exactly two tests — this one and
`row_3_a_crash_after_edits_gives_the_next_attempt_a_continuation_packet` — and left
the other 13 in the file passing. So the routing is load-bearing for precisely the
claims that name it, and for nothing else. Reverted; 15/15 green.

**What it does not prove:** that a *reasoning* agent can use the packet. It proves
the packet carries enough, and that finishing needs no state the dead process took
with it — which is the stop point, *"recovery does not depend on hidden state"*. The
real-agent half belongs with S15/S16's real-agent suite.

---

## 12. What is still not done

1. **`TARGET_PACKET_BYTES` is advisory and the fixture packets exceed it.** §6.5
   *targets* 4 KB; only `MAX_PACKET_BYTES` is enforced. Enforcing the target would
   be a behaviour change, not a tightening, and it is not decided here.
2. **Separate-process regenerability is proven for the implementation packet only.**
   `the_stored_packet_is_the_one_the_state_produces` rebuilds it from the store in a
   different process than the one that wrote it; the *continuation* packet's
   equivalent is proven as determinism plus `observe_run`'s measurement, not as a
   second process. §7's item 6 is therefore half closed.
3. **No real agent has acted on a packet.** §11's last paragraph: the deterministic
   half is proven, the model half is S15/S16's.
