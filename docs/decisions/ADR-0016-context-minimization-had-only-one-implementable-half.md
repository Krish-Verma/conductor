# ADR-0016 — §6.5's context minimization had only one implementable half

**Status:** ACCEPTED
**Date:** 2026-08-16
**Slice:** S12 (found building the implementation packet, before any code was written)

---

## Question

§6.5 states the rule that keeps a packet from growing without bound as a project
ages:

> **Context minimization:** decisions selected by touching the task's scope globs
> or explicit refs — never "all accepted decisions."

Two mechanisms, joined by "or". **Can either be implemented against the schemas
S11 shipped?**

## Why the answer matters to Conductor

This is not a cost optimization. A packet that carries every accepted decision
grows monotonically with the project, and the first thing it breaks is the size
budget the same section imposes — after which the pressure is to start dropping
fields, and §6.5 is explicit that the field which must never be dropped is the
reason for a constraint. Context minimization is what keeps the budget
satisfiable without lying.

It is also the difference between an agent reading the three arguments that bear
on its task and an agent reading a decision log. The second is not more
information; it is the same information with the relevant part hidden.

## Evidence

**Mechanism 1 — "touching the task's scope globs" — requires the decision to
declare a scope. It cannot.**

`crates/conductor-run/src/decision/model.rs` puts `#[serde(deny_unknown_fields)]`
on the frontmatter and models exactly four fields — `id`, `status`, `date`,
`supersedes`. That is not an oversight; it is §3.6:

> A **decision is an argument.** Small fixed metadata (`id`, `status`,
> `supersedes`, `date`) in schema-validated frontmatter; the value is prose a
> human reads and a packet quotes.

and the module's own docs give the reason `deny_unknown_fields` is there while
the plan deliberately lacks it: *"an unknown key here is not tomorrow's
feature."* A decision with a `scope:` would also change the content-hash
preimage of **every decision already written**, because the hash covers the
canonical frontmatter.

There is no other source for a decision's scope. Nothing in `.conductor/` maps
decisions to paths.

**Mechanism 2 — "explicit refs" — has no field either, but the field it needs is
free to add.**

`crates/conductor-run/src/plan/model.rs`'s `Task` had `id`, `objective`,
`rationale`, `depends_on`, `scope`, `verification_profile`, `attempt_budget`,
`acceptance_criteria`, `actions`, `execution_requirements` — and nothing naming a
decision. But unlike the decision document, a plan document has **no**
`deny_unknown_fields`, on §3.2's reasoning that a plan written for a later
Conductor must still load on this one.

So one direction is closed by a rule with a stated purpose, and the other is
open by a rule with a stated purpose.

## Decision

**Implement explicit refs. Do not implement scope matching.**

`plan::model::Task` gains `decisions: Vec<String>` — decision ids whose argument
the task needs. The reference points **from the plan**, which is the document a
human writes and approves, rather than from the decision, which is a record of
something already argued.

That direction is better on its own merits, not merely cheaper. A decision does
not know which future task will need it; the person writing the task does. A
scope glob on a decision would be a guess about relevance made at the moment the
argument was recorded, and it would be wrong in both directions — matching tasks
that happen to touch a path the decision mentions, and missing tasks whose
relevance is conceptual rather than positional.

**A reference that does not resolve is a refusal, not an omission.**
`PacketError::UnknownDecision` names both the task and the id. Silently dropping
it would be context minimization failing in the direction that loses
information, and the failure would be invisible: the agent would receive a
well-formed packet missing an argument the plan says it needs.

## Consequences

- §6.5's sentence is now half-implemented **by design**, and says so. The master
  plan is amended to record which half and why, so a later reader does not
  read the gap as an oversight and "fix" it by adding `scope:` to decisions.
- Adding an optional field to `Task` does not change the content hash of any
  existing plan: the canonical encoding is over the parsed document, and a field
  no document declares contributes nothing.
- `plan validate` is **not** extended to check decision refs. §3.7 is an explicit
  list of refusals and CLAUDE.md forbids speculative additions; the packet
  builder is where both halves — the plan and the decision set — are in hand, and
  it fails closed there.
- **Not done:** a task cannot reference a decision by anything other than its id.
  If a project later wants "every decision about the policy engine", that is a
  query, and a query needs an index that does not exist. It is not needed for v1.

## Impacted sections

- **§6.5** — the context-minimization sentence gains a note recording that only
  explicit refs is implementable, and why.
- **§3.6** — unchanged, and deliberately: this ADR exists partly to record that
  the four-field frontmatter was *considered and kept*.

## Evidence index

- `crates/conductor-run/src/decision/model.rs` — `deny_unknown_fields`, four fields.
- `crates/conductor-run/src/plan/model.rs::Task::decisions` — the field, and its reasoning.
- `crates/conductor-run/src/packet/implementation.rs` — selection, and the refusal.
- `crates/conductor-run/tests/packet.rs::only_decisions_the_task_explicitly_references_travel`
  — asserts both directions: a referenced decision travels, an unreferenced one does not.
- Mutation: replacing the reference list with every available decision kills that
  test. (A first attempt at the same mutation did not compile and is recorded as
  an invalid experiment, not a surviving mutant.)
