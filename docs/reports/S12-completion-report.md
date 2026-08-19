# S12 — Packets and reports — **COMPLETION** report

**Slice:** S12
**Branch:** `s12-packets`
**Base:** `c10ede5` (S11 on `main`)
**Status:** ✅ **COMPLETE — the stop point is reached.**

**Stop point (Part 8):** *Recovery does not depend on hidden state.*
**Verify line:** *An agent handed **only** a continuation packet completes a task
interrupted mid-way, on a fixture, **with no session resume**.*

The Verify line has been **run** and is green, with the mechanism it depends on
mutated out to show the test is not vacuous. §11 is the evidence.

This report supersedes `S12-progress-report.md`, which is kept because it records
what was open at each point and why — including the two findings that turned out to
be the substance of the slice.

---

## 1. What S12 turned out to be about

The scope line said *"generate every packet from durable state"*. Generation was the
easy half and was largely done by the progress report. The slice's real content was
two findings of the same shape, the second found by asking the first's question a
second time:

> **ADR-0017 — the core verb never used the plan ledger.** `conductor task run` read
> the S5-era `.conductor/task.yaml` and fabricated a `plan_version` at `version 0`,
> `state DRAFT`. So on the product path §4.3's approval gate never ran, and
> `task.declared_actions` / `task.depends_on` / `task.acceptance_criteria` /
> `task.execution_requirements` — the four columns S11's materializer writes and
> §4.2/§4.3's gates read — were `NULL` on **every real run**, so both gates compared
> nothing and proceeded. `materialize` had no call site outside tests and
> `conductor recover`.

> **ADR-0018 — the packets were built and never delivered.** `task run` passed
> `spec.objective()`, one line of prose. Worse, `repair::driver` built the repair
> packet, returned it in `Attempted { packet }` for *reporting*, and launched the
> attempt with something else — so §6.5's `do_not_retry` list, *"what stops attempt 2
> from being attempt 1 again"*, had never been seen by attempt 2.

Both were built, unit-tested, and described as complete in earlier slice reports.
S6's own test `attempt_two_is_given_a_packet_that_names_what_attempt_one_already_tried`
asserted the packet's *contents* from the value the driver returned; its **name**
claimed delivery. The name was the tell, and it passed for two slices.

---

## 2. §6.5 / §6.6 requirements

| Requirement | State | Evidence |
|---|---|---|
| Canonical, deterministic packet serialization | DONE | `packet::canonical_bytes`, sorted keys, type-tagged, length-prefixed |
| Its own digest domain | DONE | `PACKET_CANONICAL_VERSION` |
| Implementation packet, every §6.5 field | DONE | `the_packet_carries_every_field_section_6_5_names` |
| Repair packet **composed onto** the implementation packet | DONE | `packet::repair`; `the_repair_packet_is_what_attempt_two_was_actually_told` |
| Continuation packet = implementation + observed reality | DONE | `packet::continuation`; measured by `observe_run` |
| §6.5's verbatim "previous agent's reasoning is not available" | DONE | `NO_PRIOR_REASONING` |
| Context minimization (explicit refs) | DONE | ADR-0016; `only_decisions_the_task_explicitly_references_travel` |
| Evidence linked by path **and** digest, never embedded | DONE | `a_continuation_packet_links_a_large_diff_rather_than_carrying_it` |
| Size budget enforced as a **refusal** | DONE | `an_oversized_packet_is_refused_rather_than_silently_truncated` |
| Byte-identical packet from a separately built store | DONE | two stores, two directories, two wall clocks |
| Continuation packet regenerable after process restart | DONE (see §12.2) | `a_continuation_packet_is_regenerable_after_the_process_that_made_it_is_gone` |
| **Every packet is stored as an artifact** | DONE | `artifacts/<run>/<ordinal>/packet.yaml`, per attempt |
| **Each packet actually reaches the agent** | DONE | argv assertion + stored artifact + row-3 and row-7 delivery tests |
| Agent report schema as one shipped artifact | DONE | `schemas/agent-report.v1.json`, `include_str!`-bound |
| Secret safety on packet surfaces | DONE | `packet::render`; three canary tests + positive control |
| `task run` claims a task from an approved plan | DONE | ADR-0017; 13 tests through the shipped binary |
| **Verify line** | DONE | §11 |

---

## 3. Which of §6.5's three packets an attempt gets

§6.5 defines three packets and never says who receives which. **Part 9 does** — and
I first recorded that the master plan left this open, which was wrong. Rows 2, 3 and
7 specify it between them:

| row | previous attempt | next attempt is told |
|---|---|---|
| 2 | crashed, nothing survived | *"new attempt, **same packet**"* — the implementation packet |
| 3 | crashed, work survived | *"verify current tree; **continuation packet**"* |
| 7 | **exited**, verification failed | the repair packet |

The verification result cannot be the discriminator, and missing that is what made
the question look open: `repair::observation::observe` gives a verification failure
precedence over the attempt's terminal state, so a crash whose surviving work then
fails a check is `ObservationKind::Failed` — indistinguishable from row 7 by kind
alone. What separates them is whether an agent ever **finished a turn**. One that
exited made choices that can be wrong, and `do_not_retry` is about not repeating
them; one that died made no choices worth avoiding, and its successor needs to know
what is already in the tree. `attempt.outcome` is durable, so the distinction
survives the restart §4.7 exists for.

Row 2's boundary is **measured**, not read off §4.8's table:
`continuation::observe_run` re-observes the workspace against the baseline the dead
attempt stored and classifies it — the same measurement §4.7's recovery makes — and
an empty observed half falls back to the implementation packet. Writing
`CLEAN_NO_REPORT` without looking would have been wrong for the crash that also
touched `.conductor/**` or moved a remote, which is the one case where the field a
reader trusts most would have been a guess.

---

## 4. Six defects fixed, beyond the two findings

1. **Row 30's persisted state was invisible at the CLI boundary.**
   `enforce::launch::gate` wrote `BLOCKED` plus a `CRITICAL` finding and *then*
   returned `Err`; `do_run` mapped every error to §7.2's `1`, so a wrapper script
   could not tell an ineligible execution mode from a crash and `--json` printed
   nothing. The store now decides the exit code — §4.8's doctrine one level up.
2. **The vertical claimed "the next run", not this task's.** `claim_next_run`
   selects `ORDER BY priority, created_at LIMIT 1` over every run and never compared
   `claimed.task_id`. Safe by accident while one store held one run; a materialized
   plan makes it reachable with no adversary. Fixed by `claim_run`, which
   `conductor_store` already ships for `conductor recover` on the same reasoning.
3. **`task run` on a terminal task wrote a second run row** before failing at the
   transition. A refusal that has already written a row is not a refusal.
4. **Deleting the S5 task spec orphaned two of its five refusals.**
   `blank_objective` and `non_positive_attempt_budget` are restored as validator
   rules; the empty-scope one is deliberately **not** — `Scope::contains` already
   fails closed, and a second gate on a settled question is the drift ADR-0010 warns
   about. The absence is asserted so restoring it later is deliberate.
5. **§4.5's clarification 3 was unresolved and `conductor init` was wrong about it.**
   `verification_profile` is a path relative to the repository root. The scaffold
   wrote `default`, which resolved to `<repo>/default` and would have failed the
   first real run of every scaffolded project.
6. **`report_schema` named a file that resolved for nobody.** A packet is generated
   for the *user's* project, so `schemas/agent-report.v1.json` resolved against their
   tree. It is now the schema's `$id`; handing the agent the file is the adapter
   mechanism's job.

---

## 5. Three findings about the tests

None is a product defect. All three are tests that were passing for the wrong
reason, exposed because the packet gave the run path three new reads of durable
state — the plan document, the task's `verification_profile`, and the run's pinned
policy.

1. **The registered tree was a fiction.** Every run fixture seeded `project` with
   `root_path = '/fixture'`, a path that does not exist, and a `plan_version`
   pointing at a `plan.yaml` nobody wrote.
2. **Acceptance rows 13 and 14 were passing because the policy snapshot was
   undecodable.** `canonical_blob = '{}'`. The gate cannot decide against a policy it
   cannot read, so it failed closed to a human: the right outcome by a mechanism that
   had nothing to do with the rules. A *decodable empty* policy dropped both to
   `VERIFYING`, which is correct — `Action::floor()` allows a known action no rule
   names — so the fixture now declares the rules those rows are about.
3. **There were two verification profiles and the product read the one nothing
   wrote.** One profile now, at §3.1's location in the registered tree.

A fourth, smaller: the fixtures left `.conductor/` untracked, making the operator's
repository permanently dirty. It is committed now, which is what §3.2 makes it
anyway.

**A fifth, found during this report's own verification: an identified flake, with a
measured root cause.** The re-gate of §7.1 came back `1160 passed / 1 failed`, and the
harness named the failure instead of losing it:
`a_successful_agent_is_spawned_streamed_and_reaped`, at
`assert!(matches!(agent.liveness(), Liveness::Alive(_)))`.

* **Not reproducible in isolation** — 0/40 runs of the test alone, and 0/40 under
  20 CPU burners on a 10-core host. CPU saturation is not the trigger.
* **Reproducible at 3/30** running the whole `supervise` binary, whose other fourteen
  tests provide the real load. The instrumented assertion named the variant every
  time: `Liveness::Dead`.
* **Root cause is a measured host fact this project already owns.** The catalogued
  `success` scenario exits in single-digit milliseconds, so the child can finish
  before the parent reaches the next line. A child that exited and has not been
  reaped is a **zombie**, and S11 measured that `proc_pidinfo` fails with `ESRCH`
  for a zombie on this host — 300/300 against a 0/300 control. `start_time_us`
  therefore answers `None` and `probe` answers `Dead`, **correctly**: the process
  really is gone.

So this is the harness asserting a transient state the product never promised — the
same shape as the flake already recorded above `completing()`, and the second distinct
flake in this one test. **The product was right both times.** The fix gives the child a
lifetime long enough to be observed (the `success` steps plus a 250 ms sleep placed
*after* the first emit, so the supervisor's startup timer still sees output at once)
and makes the assertion name the variant it saw, because `Dead`, `Recycled` and
`Unidentified` are three different defects that `matches!` could not distinguish.

Verified: **0/40** whole-binary runs after the fix, against 3/30 before. Stated as
what it is — a 10 % flake that a clean 40-run sweep leaves at roughly 98 % confidence
of having dropped, plus a mechanism that is now structurally absent rather than
re-rolled. Whether this is the same event as the project's historical unidentified
`765 passed / 1 failed` cannot be established: that run did not record which test
failed, which is why the harness records it now.

**One near-miss worth recording.** The first secret canary used a 7-character
password and read as a scanner gap. It was the fixture: `is_placeholder` suppresses
values under 8 characters deliberately. The canary was fixed, not the scanner —
which is the direction ADR-0006 exists to enforce.

---

## 6. One behaviour change, stated rather than absorbed

`a_materialized_task_is_refused_when_the_runs_policy_cannot_be_read_and_a_never_materialized_one_is_not`
asserted that a task with `declared_actions = NULL` **completes** under an
undecodable policy: §4.3's rule does not apply, nothing reads the policy, the
corruption is harmless. Still true of the rule; no longer true of the run. §6.5's
packet reads the pinned policy to fill in the `boundaries` an agent is told, and a
run that cannot state what requires approval and what is forbidden must not launch
an agent.

A strengthening in the fail-closed direction, and the same reading `enforce::launch`
already applies: *"we cannot tell what the rules are"* must never become *"there are
no rules"*. The `NULL`-versus-`'[]'` distinction is §4.3's, not the packet's, so it
moved to the gate —
`a_task_that_was_never_materialized_is_not_subject_to_the_binding_rule`.

---

## 7. Non-vacuity ledger

Every mutation was **verified present in the source** before its result was read.

| Mutation | Applied? | Killed | Interpretation |
|---|---|---|---|
| Canonical encoder stops sorting map keys | VERIFIED | `map_key_order_cannot_reach_the_bytes` | Sorting is load-bearing |
| `emit()` stops enforcing the ceiling | VERIFIED | oversized-refusal test | The budget is real |
| `Evidence::linked` embeds the bytes it hashes | VERIFIED | size-budget test | "Linked, never embedded" is load-bearing |
| Carry every decision instead of the referenced ones (attempt 1) | **did not compile** | — | **INVALID EXPERIMENT** |
| Same, in a compiling form | VERIFIED | context-minimization test | Selection is load-bearing |
| Drop the `.conductor/**` union (attempt 1) | VERIFIED | **nothing — SURVIVED** | **The test was vacuous** — fixed, see the progress report |
| Same, after adding a fixture that declares nothing | VERIFIED | governance test | The union is load-bearing |
| **Disable the continuation route** (`} else if false && unfinished {`) | VERIFIED | the Verify line **and** the row-3 delivery test; the other 13 in the file stayed green | The routing is load-bearing for exactly the claims that name it |
| **Remove §4.3's plan-state gate** (`_state => None`) | VERIFIED | **nothing — SURVIVED**, then killed after a test was added | **The gate had no test.** See §7.1 |
| Dependency gate always reports satisfied | VERIFIED | `a_task_whose_dependency_is_not_complete_does_not_launch_an_agent`, `running_one_task_never_claims_another_tasks_run` | The dependency edge is load-bearing |
| §4.3's binding rule derives no requirement from any declaration | VERIFIED | 5 tests in `enforce_eligibility` | The binding rule is load-bearing |

### 7.1 A survivor, found while verifying this report rather than writing it

The three rows above were run **after** this report first claimed completion, as an
independent check of the integration ADR-0017 describes. The first of them survived:
with §4.3's plan-state gate replaced by `_state => None`, **all fifteen tests in
`task_run_plan.rs` still passed** — and the fixture task ran to completion and
*passed verification* under a plan version no human had approved. That is the exact
defect ADR-0017 exists to have fixed, reachable again by deleting one match arm, with
nothing to stop it.

Two tests looked like they covered the gate and neither did:

* `a_task_with_no_approved_plan_does_not_run` asserts §7.2's `2`, which is the
  *unregistered project* refusal. The gate is never reached.
* `a_plan_that_is_only_validated_does_not_run_its_tasks` asserted
  `said(&out).contains("VALIDATED")`. At `VALIDATED` nothing is materialized, so the
  real refusal is `NoSuchTask` — whose message appends the version listing
  `v1 VALIDATED` for the operator. **The assertion was passing on a substring emitted
  by a different error.** This is ADR-0006's vacuity in a new place, and the same tell
  as S6's: the test's *name* claimed one mechanism while its *assertion* held another.

Both are fixed:

* `a_materialized_task_under_a_non_authoritative_plan_version_is_refused_by_state` is
  new, and holds the gate — verified to fail under `_state => None` while the other
  fifteen stay green.
* the `VALIDATED` assertion now asserts the two halves separately (no task row was
  materialized; the refusal names the un-materialized task) so it can no longer pass
  on an unrelated diagnostic.

**Reachability, stated rather than implied.** Every path that creates a `task` row
requires `APPROVED` first — `plan approve` materializes, and `conductor recover`
rebuilds a task list only for an approved version. So the gate's refusal arm is not
reachable from a current product path, which is *why* nothing held it. It is a
fail-closed invariant guard on the invariant §4.3 rests on — a task row executes only
while its plan version is authoritative — and §3.1 makes the store the disposable half
that can come to disagree with `.conductor/`. The new test says so in its own comment
rather than implying a product path it does not have.

The lesson is the standing check CLAUDE.md already records, applied one level deeper:
naming a mechanism's product call site is necessary and not sufficient. **The call site
existed and was correct; what was missing was any test that failed when it was
removed.**

---

## 8. Files

```
crates/conductor-run/src/plan/runnable.rs           which plan version authorizes a run (ADR-0017)
crates/conductor-run/src/packet/repair.rs           §6.5's repair packet, composed
crates/conductor-run/src/packet/mod.rs              render(): redaction chokepoint
crates/conductor-run/src/packet/continuation.rs     observe_run(): observed reality, measured
crates/conductor-run/src/worker.rs                  store_packet(); WorkerConfig::instructions
crates/conductor-run/src/repair/driver.rs           which of the three packets an attempt gets
crates/conductor-agent/src/lib.rs                   StartInput::{instructions, instructions_path}
crates/conductor-agent/src/codex.rs                 with_prompt deleted; schema include_str!
crates/conductor-agent/src/scenario.rs              Step::FinishFromPacket
crates/conductor-cli/src/task.rs                    the core verb, rewritten
crates/conductor-cli/src/approval.rs                plan approve materializes
schemas/agent-report.v1.json                        the one report-shape artifact
docs/decisions/ADR-0017-*.md, ADR-0018-*.md
```

Deleted: `crates/conductor-run/src/spec.rs`,
`conductor_core::task::{TaskSpec, ValidatedTaskSpec, TaskSpecError}`, `task run --spec`,
`CodexAgent::with_prompt`.

---

## 9. Master-plan amendments applied

| Section | Change |
|---|---|
| §3.7 | the four rules the validator adds that §3.7's closed list does not name, and why the empty-scope refusal is not among them |
| §4.5 clarification 3 | settled: `verification_profile` is a path relative to the repository root |
| §6.1 | `StartInput`'s fields; why the instruction is per-attempt rather than adapter state; why the packet is a plain artifact and the baseline is not |
| §6.5 | which packet an attempt gets (rows 2/3/7); the shipped-versus-specified report divergence; the schema as one artifact; `report_schema` as an identifier; `Observed::changed_paths` |
| Part 8 S5 | the task-spec file was deleted at S12, five slices after the slice meant to replace it |
| Part 8 S11 | what S11 did **not** do, so "S11 COMPLETE" is not read as "the product path used it" |
| Part 8 S12 | scope gains the core-verb integration; Verify line marked run |
| Part 9 | rows 21 and 30 were scored from library and unit evidence until S12, and why |

---

## 10. Verification

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | exit 0, zero warnings |
| `cargo test --all --no-fail-fast` | see §13 |

---

## 11. The Verify line

`s12_verify_line_a_fresh_agent_finishes_from_the_continuation_packet_alone`, in
`crates/conductor-run/tests/repair_loop.rs`. Every clause is *arranged*, not asserted
after the fact:

| clause | how it is made true |
|---|---|
| *interrupted mid-way* | attempt 1 writes `src/scratch.rs` and is `SIGKILL`ed; work survives, no report exists, the required check still fails so the run stays alive |
| *a continuation packet* | row 3's routing; the worker stores it at `artifacts/<run>/2/packet.yaml` |
| **handed *only*** | attempt 2's scenario is a single `finish_from_packet` step, which takes **no parameters** |
| *no session resume* | `session_resume: false` on the adapter, **and** the assertion reads `attempt.agent_session_id` out of the store for every attempt |

The *"only"* clause needed a new mechanism. Every other fake-agent step is scripted
— the scenario file says what to write — so a test built from them would have proved
the *scenario* was sufficient and said nothing about the packet: an empty packet
would have passed just as well. That is ADR-0006's vacuity. `Step::FinishFromPacket`
therefore takes no arguments: it opens `CONDUCTOR_FAKE_PACKET`, refuses unless the
document carries an objective, a scope and acceptance criteria, and derives the file
to write from the packet's own objective. A packet that stopped being sufficient
breaks the test and names the field it lost.

**What it proves:** the packet carries enough, and finishing needs no state the dead
process took with it — the stop point exactly.
**What it does not prove:** that a *reasoning* agent can use it. That is the model
half, and it belongs with S15/S16's real-agent suite.

---

## 12. Known limitations

1. **`TARGET_PACKET_BYTES` is advisory and the fixture packets exceed it.** §6.5
   *targets* 4 KB; only `MAX_PACKET_BYTES` is enforced. Enforcing the target would be
   a behaviour change, not a tightening, and is not decided here.
2. **"Regenerable after total process restart" is proven from durable state, and for
   the *implementation* packet also across a genuine process boundary** — the
   `conductor` binary writes `packet.yaml`, and `the_stored_packet_is_the_one_the_state_produces`
   rebuilds it in the test process and compares bytes. The continuation packet's
   equivalent is proven as determinism across two independently built stores plus
   `observe_run` reading every input from disk; a literal second OS process rebuilding
   *it* is not written. Recorded rather than claimed.
3. **No real agent has acted on a packet.** §11's last paragraph.
4. **`Project::default_branch` is stored and never read at run time.** `task run`
   records the run's `target_branch` from the branch the operator has checked out,
   deliberately (§4.1). What was fixed is the `init.rs` doc comment claiming the field
   "decides where work lands", which was a false statement about behaviour. Its
   disposition belongs to the v1 configuration audit.
5. **`review_cadence` is parsed and never read.** S13 owns it.

---

## 13. Gate

`cargo test --all --no-fail-fast`, run to completion (confirmed by cargo's last
step, `Doc-tests conductor_store` — a killed run ends mid-`Running`):

```
suites   96      ok, 0 failed
passed   1160
failed   0
ignored  3       (real-Codex, ignored by default)
panics   0
```

**Re-gated after §7.1's finding**, with the added test and the repaired assertion:

```
suites   96      ok, 0 failed
passed   1161
failed   0
ignored  3
panics   0
```

Against the baselines: S11 was 94 suites / 1124 passed; S12's progress report was
95 / 1139. This slice is **+2 suites** (`task_run_plan`, and the schema artifact
brought no new suite — the second is `plan/runnable`'s coverage landing inside
existing suites) and **+21 tests**, net of the seven deleted with the task spec.

`fmt --all --check` clean · `clippy --all-targets --all-features -- -D warnings`
exit 0, zero warnings.

**Two gates were killed rather than green, and neither is reported above.** Gate 3
ran on a tree whose fixtures were mid-migration; gate 5 was stopped at 92/0 because
the fake-agent binary it was exercising predated `Step::FinishFromPacket`. Recorded
because this project has previously mistaken a killed run for a passing one: a
process disappearing is not a result.

---

## 14. Recommended next step

S13 — the review bridge. Note two things S12 leaves on its doorstep: `review_cadence`
is parsed and unread (limitation 5), and §6.5's **review packet** is the one packet
of the four this slice did not build, because its consumer is S13's `review export`.
