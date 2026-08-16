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
| `cargo test --all --no-fail-fast` | see the commit that closes this branch |

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
