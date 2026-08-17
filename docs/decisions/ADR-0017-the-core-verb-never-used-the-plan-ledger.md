# ADR-0017 · The core verb never used the plan ledger · ACCEPTED · 2026-08-17

## Question

`conductor task run <task-id>` is §7.1's *"core verb"*. Which document decides what
a task **is** when that verb runs — the S5-era `.conductor/task.yaml`, or the
approved plan version S11 built the whole ledger for?

## Why the answer matters to Conductor

Almost everything the design calls load-bearing reads a fact that only a plan can
supply:

| Reader | Fact it needs | Where the fact comes from |
|---|---|---|
| §4.3's binding rule (`enforce::launch::binding_rule_for`) | `task.declared_actions` | `plan.yaml`'s `actions:` |
| §4.2's gate (`enforce::launch::requirements_for`) | `task.execution_requirements` | `plan.yaml`'s per-task override |
| §5.2's `PENDING ──deps met──► READY` edge | `task.depends_on` | `plan.yaml`'s `depends_on:` |
| §4.5's completion criteria | `task.acceptance_criteria` | `plan.yaml`'s `acceptance_criteria:` |
| §3.4's `Conductor-Plan` trailer | `plan_version.version` + `.content_hash` | the approved version |
| §6.5's packet | the plan document itself | `.conductor/plans/vN/plan.yaml` |
| Acceptance row 21 | `run.plan_version` pinning | supersession at approval |

All seven were implemented. None of them was **reachable from the product path**,
because `task run` created its own task row from a task-spec file and its own
`plan_version` row to hang it off:

```rust
// crates/conductor-cli/src/task.rs, ensure_task_and_run — deleted at S12
INSERT OR IGNORE INTO plan_version
  (id, project_id, version, content_hash, state, source_path)
VALUES (?1, ?2, 0, ?3, 'DRAFT', '.conductor/task.yaml')
```

The consequences were not cosmetic:

1. **§4.3's approval gate never ran on the product path.** A task could execute
   with no human having approved any plan. `materialize` refuses a version that is
   not `APPROVED` — and nothing on the product path called `materialize`.
2. **Both §4.2 gates compared nothing.** `declared_actions` and
   `execution_requirements` were `NULL` on every real run, and both readers treat
   `NULL` as "nothing was declared, proceed". That is the correct reading of `NULL`
   and the wrong outcome, because the declaration existed — in a document nobody
   read.
3. **Acceptance row 21 had no product path at all.** Nothing outside tests and
   `conductor recover` ever created a versioned plan, so "approve v4 during a v3
   run" was not a scenario the shipped binary could reach.
4. **§5.2's restart clause described a check nothing performed.** *"Re-hash on
   load; a mismatch on an `APPROVED` plan is a hard error"* — no product path
   loaded an approved plan.

## Experiment / evidence

Two greps and one reading, all against the tree at `ea1c38d`.

**1. `materialize` had no product call site.**

```
$ grep -rn 'materialize::materialize\|materialize(' --include='*.rs' crates | grep -v 'fn materialize'
crates/conductor-cli/tests/recover.rs:178
crates/conductor-run/tests/plan_reconstruct.rs:233, 489
crates/conductor-run/tests/plan_materialize.rs:  … 18 hits …
crates/conductor-run/tests/packet.rs:221
crates/conductor-run/src/plan/reconstruct.rs:236
```

Tests, and one library function that only `conductor recover` reaches. No CLI verb.

**2. `plan approve` stopped at the approval.** `plan_approve` in
`crates/conductor-cli/src/approval.rs` ends at `ledger::approve` and returns the
sidecar's fields. It never materialized, so after a human approved a plan
`conductor task list` was empty.

**3. The stopgap's own comment predicted its replacement.**
`crates/conductor-run/src/spec.rs` opens with *"**S11 deletes this.** The plan
ledger replaces the whole file… Nothing in this module should grow."* S11 shipped
the ledger and did not delete the module, and the `task run` call site remained.

**Positive control.** The defect is asserted as a test rather than only reasoned
about: `crates/conductor-cli/tests/task_run_plan.rs::a_task_with_no_approved_plan_does_not_run`
fails against the old code — the run succeeds — and passes against the new.

## Observed result

`task run` executed tasks that no approved plan declared, under a `plan_version`
row that said `DRAFT` and `version 0`, with every plan-derived gate comparing
`NULL`.

## Decision

**One authority, and it is the approved plan version.**

1. **`plan approve` materializes.** Approval is the event that changes what work
   exists, so it is where §5.2's task list is written. It is also the only place
   supersession can happen, which is what makes row 21 reachable. Decisions are
   registered first, because §6.5's packet resolves a task's `decisions:` refs
   against registered rows.
2. **`task run` claims and invents nothing.** A new module,
   `conductor_run::plan::runnable`, is the single place that decides whether a
   task may run. It requires a registered project whose `root_path` equals the
   offered tree (§3.3 control 2), a task row, and a `plan_version` that is
   `APPROVED` — verified against its `APPROVED` receipt through
   `ledger::verify_approval`, which is what makes §5.2's restart clause a real
   check.
3. **Row 21 is the one exception, and it is conditioned on the run, not the
   state.** A task pinned to a `SUPERSEDED` version may still run **iff** it
   holds a non-terminal run. That is literally what the row asks for ("finish
   under v3"), and `materialize` already carries such a task rather than retiring
   it. No receipt is checked for it: a superseded version's sidecar may
   legitimately be gone, and the run was pinned when the version *was*
   authoritative.
4. **The legacy path is deleted, not deprecated.** `--spec` is gone,
   `conductor_run::spec` is gone, and `conductor_core::task::{TaskSpec,
   ValidatedTaskSpec, TaskSpecError}` are gone. A second reachable authority for
   "what is a task" is precisely the ambiguity this record exists to remove, and a
   compatibility mode nobody asked for would have kept it reachable.
5. **`verification_profile` is a path.** §4.5's clarification 3 explicitly
   deferred the reading — *"§5.1 makes `verification_profile` a per-task path
   while this section names a single `verification.yaml`; until S11's persistence
   step settles that…"*. Something finally had to resolve it, and this is that
   something. It is a path relative to the repository root, because §3.1's layout
   holds **one** `verification.yaml` containing **one** profile and there is no
   second profile for a name to select between. Named profiles would be a feature
   no acceptance row needs. `conductor init`'s scaffold said `default`, which
   resolved to `<repo>/default` and would have failed the first real run of any
   scaffolded project; it now writes the path.

### Three more defects the fix exposed

**1. The vertical claimed "the next run", not this task's run.**
`vertical::run_task_with_session` called `claim_next_run`, whose predicate is
`state IN ('READY','RECONCILING') ORDER BY priority, created_at LIMIT 1` over
**every** run in the store, and never compared `claimed.task_id` against the task
it was asked to run. That was safe by accident while the S5-era path put one task
and one run in each store: "the next run" and "this task's run" could not
disagree. A plan materializes many tasks into one store, and the ordinary route to
the bug needs no adversary — `task run` on a task with an unmet dependency creates
its run row *before* the dependency check refuses, leaving it `READY`; running the
dependency next then claims the refused task's run, drives an agent in its branch
and workspace, and writes the task state against a different row. Fixed by
claiming by id through `claim_run`, which `conductor_store` already ships for
`conductor recover` on exactly this reasoning: *"startup recovery needs the run it
is recovering, not 'the next one'"*. The predicates are held together by
`the_targeted_claim_shares_the_general_claims_predicate`, so this narrows which
run is eligible and changes nothing about whether one is.

**2. `task run` created a second run for a terminal task.** §5.2 makes `COMPLETE`,
`CANCELLED` and `SUPERSEDED` terminal, and nothing checked before the run row was
written. `active_run_for_task` returns `None` for a terminal run, so a second
`task run` on a finished task created a fresh `READY` run and only then failed at
the transition — leaving the row behind. A refusal that has already written a row
is not a refusal. Fixed in `runnable::resolve`, ahead of the approval gate so that
a retired task says "this task was retired" rather than "approve the plan first".

**3. Deleting the task spec orphaned two of its five refusals.** The spec refused a
blank objective, an empty scope and a zero attempt budget; §3.7 refuses none of the
three, and `plan::model::Task` gives all three a `serde` default. So a plan could
declare a task with no objective and validate — and S10 measured that `codex exec`
with no prompt argument **blocks forever reading stdin**, meaning the launch refuses
it only after approval, materialisation and a workspace clone. `blank_objective` and
`non_positive_attempt_budget` are restored as validator rules (§3.7's table of four
non-§3.7 rules). The empty-scope refusal is deliberately **not** restored:
`conductor_git`'s `Scope::contains` already fails closed on it, and a second gate on
a question answered safely is the drift ADR-0010 warns about. The absence is
asserted, so restoring it later is a deliberate reversal.

### A fourth defect the fix exposed

Row 30's persisted state was **unreachable from the CLI**. `enforce::launch::gate`
refuses, writes `BLOCKED` plus a `CRITICAL` finding, and *then* `run_task` returns
`Err`. `do_run` mapped every error to §7.2's `1`, so a wrapper script could not
tell row 30 from a crash, and `--json` printed nothing — the finding the refusal
exists to leave behind was invisible at the boundary that produced it. `task.rs`'s
own module docs already said `BLOCKED` belongs to code `3`.

Fixed by making the **store** decide: on a `run_task` error the persisted task
state is read and, when it is one of §5.2's three human states, the exit code is
`3` and a report is emitted. This is §4.8's doctrine applied one level up — what is
in the store is what happened; a return value is a claim about it.

## What this DOES prove

* The shipped binary now refuses to run a task that no approved plan version
  declares — proven through the process boundary, with a positive control that the
  same fixture succeeds once v1 is approved at the control socket
  (`tests/task_run_plan.rs`, 11 tests).
* Seven previously unreachable mechanisms are reachable: the approval gate, both
  §4.2 gates, §5.2's dependency edge, §5.2's restart clause, §3.3's control 2 at
  run start, and acceptance row 21's supersession.
* Row 30 now surfaces as §7.2's `3` with its finding in `--json`.

## What this DOES NOT prove

* **That the packet reaches the agent.** `task run` still passes the plan task's
  `objective` string as the agent prompt. §6.5's packet is built and proven
  deterministic; carrying it to the agent needs `StartInput` to be able to hold
  it, because the adapter's prompt is fixed at construction time — before the
  workspace exists — and a packet with a null `workspace` would be a wrong
  artifact. That is the next piece of S12 and it is **not** done here.
* **That approval is a human-identity boundary.** It is not; ADR-0002 and §4.3's
  tier table are unchanged. This record moves *which document* is authoritative,
  not *how strongly* the socket authenticates.
* **That `plan approve` is now atomic.** It approves, then registers decisions,
  then materializes. A crash between them leaves an approved version with no task
  rows. Re-running `plan approve` completes it, because all three are idempotent —
  but that is convergence, not atomicity, and it is deliberate: a materialisation
  failure must not un-approve a decision a human already made at the socket.
* **That every configuration knob is now effective.** `Project::default_branch`
  is still not consulted when a run picks its integration target — `task run` uses
  the branch the operator has checked out, deliberately, for §4.1's reason. What
  *was* fixed is the doc comment in `init.rs` that claimed the field "decides
  where work lands", which was a false statement about behaviour. The field's
  disposition is audited in S12's completion report, not resolved by this record.

## Pre-registered falsification / revisit trigger

* **Falsified if** a product path is found that creates a `task` row without an
  `APPROVED` plan version behind it. `plan_version` rows at `version 0` are the
  signature; a query returning one is the alarm.
* **Falsified if** a run is ever claimed by a command that was asked to run a
  different task. `running_one_task_never_claims_another_tasks_run` is the test;
  it asserts the claimed run id and that the other task's run is still `READY`
  with zero attempts.
* **Falsified if** `tests/task_run_plan.rs::the_approved_plan_is_the_authority_for_what_runs`
  and `::a_task_with_no_approved_plan_does_not_run` ever both pass with
  `runnable::resolve`'s state gate removed. The gate is the mechanism; if deleting
  it kills nothing, the tests are decoration.
* **Revisit if** a second verification profile per project becomes a real need.
  The path reading is then a constraint rather than a simplification, and §4.5's
  clarification 3 reopens — with a note that changing it alters the meaning of
  every `verification_profile` value already written.
* **Revisit if** a legitimate use for running a task outside any plan appears
  (a scratch task, a one-off probe). The answer is a plan version with one task,
  not the return of a second authority — but if that proves unusable in practice,
  this record is the thing to argue with.

## Impacted master-plan sections

* **§3.7** — gains the table of four rules the validator adds that §3.7's closed
  list does not name, and the reason the empty-scope refusal is not among them.
* **§4.5's clarification 3** — the deferred `verification_profile` question is
  settled as a path; the clause gains the resolution.
* **Part 8's S11 outcome note** — records that S11's ledger was not connected to
  the core verb, so a reader does not conclude from "S11 COMPLETE" that the
  product path used it.
* **Part 8's S12** — the scope line gains this integration, which was not in it.
* **§7.1** — `task run <task-id>`'s description is unchanged; `--spec` was never
  in the 13-command surface, which is part of why its survival went unnoticed.
* **Part 9 row 21 and row 30** — both gain a product-path evidence pointer.
