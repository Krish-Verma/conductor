# ADR-0018 · The packets were built and never delivered · ACCEPTED · 2026-08-17

## Question

§6.5 opens with *"Every packet is generated from durable state, content-hashed,
and stored as an artifact."* Two questions hide behind the word *generated*: is
the packet **stored**, and is it what the agent is actually **told**?

## Why the answer matters to Conductor

The packet is the whole of what an agent knows. Everything Conductor does to
constrain an agent — the scope, the acceptance criteria, the policy boundaries,
the decisions whose argument the task depends on — reaches it through §6.5 or not
at all. And §6.5's repair packet exists for one named purpose: *"an explicit
`do_not_retry` list of approaches already tried. That last field is what stops
attempt 2 from being attempt 1 again."*

If the packet is not delivered, none of that is true of the running system. The
agent is working from whatever string somebody passed, the constraints are
enforced only after the fact by reconciliation and policy, and a repair attempt
repeats the first attempt — which is the specific failure §4.6's whole budget
exists to bound.

## Experiment / evidence

**1. The implementation packet reached nobody.** `conductor task run` built its
adapter with `CodexAgent::with_prompt(spec.objective())` — one line of prose from
the task spec. `packet::implementation::build` had exactly two callers, both
tests. Nothing wrote a packet artifact anywhere: `find` over a completed run's
artifact tree showed `baseline.json` and `report.json` and nothing else.

**2. The repair packet reached nobody either, and this one is worse** because the
code that built it sat three lines from the code that launched the attempt:

```rust
// crates/conductor-run/src/repair/driver.rs, before this ADR
let packet = packet::build(run_id.as_str(), next_ordinal, &observations, …);
// …
let result = run_task_with_session(store, adapter, vertical, session.as_deref(), observer)?;
// …
Ok(Step::Attempted(Box::new(Attempted { ordinal: next_ordinal, packet, … })))
```

The packet was built from durable state, returned in `Attempted { packet }` for
**reporting**, and never given to the agent. S6's own test
`attempt_two_is_given_a_packet_that_names_what_attempt_one_already_tried` asserts
the packet's *contents* — from that returned value. Its name claims delivery; it
tested composition. The name is the tell, and it passed for two slices.

**3. The report schema was two copies, one of which did not exist.**
`packet::implementation::REPORT_SCHEMA_PATH` was the string
`"schemas/agent-report.v1.json"`; no such file existed in the repository, and the
shape lived as a literal in `conductor_agent::codex`.

## Observed result

All three of §6.5's packets were built, unit-tested, reported complete in slice
reports — and unreachable from the product path. Exactly ADR-0017's shape, one
layer down, found by asking ADR-0017's question a second time.

## Decision

**1. The instruction is per-attempt input.** `StartInput` gains `instructions:
String` and `instructions_path: PathBuf`. `CodexAgent::with_prompt` is deleted.
Two reasons, and the second is the one that makes construction-time impossible:
a packet cannot exist before the workspace does (§6.5 carries
`repository.workspace`, and the clone happens inside the attempt), and §4.6 makes
the instruction **differ between attempts of one run**.

**2. The worker derives it, or is told it.** `WorkerConfig::instructions` is
`None` for an ordinary attempt — derive §6.5's implementation packet from durable
state — and `Some` when the caller has already decided, which only
`repair::driver` can know. Not a speculative seam: it is exactly §4.6's two
cases, and the run's rows cannot distinguish them because both are true of the
same rows.

**3. The repair packet is composed, not sent alone.** §6.5 says it *"adds only"*
six fields, so `packet::repair` puts them on top of the implementation packet. A
repairing agent still needs the objective and what proves the task done. A
composition failure ends the repair rather than falling back to the plain
implementation packet — that fallback is "attempt 2 is attempt 1 again", silently.

**4. Every packet is stored, as a plain artifact.** `packet.yaml` in the
attempt's directory, so `artifacts/<run>/<ordinal>/packet.yaml` is always exactly
what that attempt was told. **Not** through the side-effect ledger, and the
difference from `baseline.json` is principled: §4.7's ledger answers *"did this
happen, and can I tell?"* for effects Conductor cannot re-derive. A baseline is a
measurement at a point in time and cannot be re-taken once the workspace changed;
a packet is a pure function of durable state (§6.6), so re-deriving it **is** the
check. A restart that finds identical bytes accepts them; different bytes are
refused rather than overwritten.

**5. Secrets are redacted on the way out, not refused.** A packet is the one
artifact Conductor **re-publishes to a different agent** — the continuation
packet carries the previous attempt's partial report, the repair packet carries a
failing check's output and the previous diff, and none of that text was vetted.
Redaction happens at one chokepoint (`packet::render`) that both the YAML and the
hashed bytes pass through, so the digest names what was delivered rather than an
unredacted original that would then be the only surviving copy. It is a pure
function of the text, so §6.6 is untouched. The marker names the kind, so
`github-token` and `aws-access-key-id` do not collapse into one blackout — the
difference between rotating one credential and rotating two.

**6. `report_schema` is an identifier, and the schema is one file.**
`schemas/agent-report.v1.json` now exists and `REPORT_SCHEMA_JSON` `include_str!`s
it, so the crate does not build without it. The packet field carries the schema's
`$id`, because as a path it could not resolve for anybody: a packet is generated
for the *user's* project.

## Three findings about the *tests*, which the delivery exposed

Making the packet real gave the run path three new reads of durable state — the
plan document, the task's `verification_profile`, and the run's pinned policy — and
each one found a fixture that had been describing something that was not there.
None of these is a product defect; all three were tests passing for the wrong
reason, which is the class ADR-0006 exists for.

**1. The registered tree was a fiction.** Every run fixture seeded `project`
with `root_path = '/fixture'`, a path that does not exist, and a `plan_version`
pointing at a `plan.yaml` nobody wrote. Harmless while the rows existed only to
satisfy §5.1's foreign keys. Now a project row is the anchor §6.5 reads the plan
through, so a fixture with no document is a run that cannot be told what to do —
and after ADR-0017 a run with no approved plan behind it is not a state the product
can reach at all. The fixtures now write a real `.conductor/` into their source
repository and register *that*, which also means they exercise §3.3's control 2
instead of bypassing it.

**2. Two acceptance rows were passing because the policy snapshot was
undecodable.** The fixture pinned `canonical_blob = '{}'`. Nothing read it back, so
nothing noticed. Acceptance rows 13 and 14 — `AWAITING_APPROVAL` for a dependency
change and for a remote change — then asserted the right outcome, reached by the
wrong mechanism: the policy gate cannot decide against a policy it cannot read, so
it failed closed to a human. Giving the fixture a *decodable* empty policy dropped
both to `VERIFYING`, which is correct — `Action::floor()` allows a **known**
taxonomy action that no rule names and denies an unknown one, S7's documented
reading of §4.4. The fixture now declares the rules those rows are about, so the
tests assert the mechanism rather than its absence.

**3. There were two verification profiles, and the product read the one nothing
wrote.** The runner was pointed at `<tempdir>/verification.yaml`; the task row's
`verification_profile` named something else entirely, and nothing resolved it.
§4.5's clarification 3 is now settled as a path relative to the repository root
(ADR-0017), so the packet resolves it — against a file that was not there. The
fixture now keeps exactly one profile, at §3.1's location inside the registered
tree, and `with_profile` overwrites the file both the runner and the packet read.

A fourth, smaller one: the fixtures left `.conductor/` **untracked**, which made the
operator's repository permanently dirty and would have failed
`the_user_repository_keeps_its_own_branch_and_checkout` for a reason unrelated to
the run. It is committed now, which is what §3.2 makes it anyway — *"a plan is a
file you write"*, tracked, so project truth travels with the repository.

## One behaviour change, stated rather than absorbed

`a_materialized_task_is_refused_when_the_runs_policy_cannot_be_read_and_a_never_materialized_one_is_not`
asserted that a task with `declared_actions = NULL` **completes** under an
undecodable policy: §4.3's rule does not apply, so nothing reads the policy, so the
corruption is harmless. That is still true of the rule and is no longer true of the
run — §6.5's packet reads the pinned policy to fill in the `boundaries` an agent is
told, and a run that cannot state what requires approval and what is forbidden must
not launch an agent.

This is a strengthening in the fail-closed direction, and it is the same reading
`enforce::launch` already applies: *"we cannot tell what the rules are"* must never
become *"there are no rules"* on the path that exists to enforce them. The
`NULL`-versus-`'[]'` distinction is §4.3's, not the packet's, so it moved down to
the gate — `a_task_that_was_never_materialized_is_not_subject_to_the_binding_rule`
— where it can still be seen.

## What this DOES prove

* The agent's instruction is §6.5's packet, asserted at two levels: the Codex
  adapter puts `StartInput::instructions` in argv as the positional prompt
  (`the_prompt_is_the_final_argument_and_an_empty_one_is_refused`), and a real run
  through the shipped binary stores a packet artifact carrying the plan version,
  the criterion and its check, the scope, `.conductor/**` and the report schema
  (`the_agent_is_given_the_packet_and_it_is_stored_as_an_artifact`).
* The stored packet equals what the durable state produces
  (`the_stored_packet_is_the_one_the_state_produces`), so a reviewer can
  re-derive what the agent was told without trusting the file.
* Attempt 2 is told what attempt 1 tried, with attempt 1's packet as the positive
  control (`the_repair_packet_is_what_attempt_two_was_actually_told`).
* A leaked credential in a partial report does not reach the next agent, and a
  clean packet is not marked redacted.

## What this DOES NOT prove

* **That the continuation packet has a consumer.** It does not. §6.5 specifies
  three packets and the product now delivers two: `resume_task` deliberately runs
  no new agent, so a crashed-then-incomplete run routes to repair and gets the
  *repair* packet. Acceptance row 3 says *"crash after edits → verify current
  tree; continuation packet"*, which implies repair-after-crash should compose the
  continuation packet while repair-after-verification-failure composes the repair
  one. **That question is open and is S12's Verify line.** Recorded here so it is
  not mistaken for done.
* **That an agent can finish from a packet alone.** That is the Verify line, and
  it has not been run. Note that proving it needs an agent that *acts on* the
  packet: a scripted fake that ignores its instruction would make the test
  vacuous, in exactly the way ADR-0006 describes.
* **That redaction catches every secret.**
  `verify::secrets::NOT_DETECTED` is the list, it is part of this guarantee, and
  it includes "anything requiring entropy analysis — not implemented, and not
  scheduled".
* **That the packet is small.** §6.5 *targets* 4 KB; `TARGET_PACKET_BYTES` is
  advisory and reported, and the fixture packets exceed it. Only
  `MAX_PACKET_BYTES` is enforced.

## Pre-registered falsification / revisit trigger

* **Falsified if** an attempt runs with no `packet.yaml` beside its
  `report.json`. The artifact and the instruction come from one call, so a missing
  file means one of them was bypassed.
* **Falsified if** attempt 2's stored packet lacks `do_not_retry` while attempt
  1's has it, or both are identical — the first means delivery regressed, the
  second means the composition collapsed.
* **Falsified if** `redact` is moved after hashing, or applied to only one of the
  YAML and the canonical bytes: the digest would then name a document nobody has.
* **Falsified if** a run fixture reappears whose `project.root_path` is not a
  directory that exists. That was how all three findings above hid, and the cheapest
  detector for the next one of their kind.
* **Revisit if** an adapter appears whose CLI takes an instruction **file**
  rather than an argument. `instructions_path` exists for it, and the day it has a
  second reader is the day to check that both agree.
* **Revisit if** `TARGET_PACKET_BYTES` starts being enforced. The fixture packets
  are already over it, so enforcing it is a behaviour change, not a tightening.

## Impacted master-plan sections

* **§6.1** — the interface block gains `StartInput`'s fields, and the reason the
  instruction is per-attempt rather than adapter state.
* **§6.5** — the report block records the shipped-versus-specified divergence,
  the schema's single artifact, and `report_schema` as an identifier.
* **§4.6** — the session-policy clause pairs `new_session_on_attempt: 2` with the
  repair packet; that pairing is now real rather than described.
* **Part 9 row 3** — its "continuation packet" clause is the open question above.
