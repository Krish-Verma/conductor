# ADR-0019 — §5.2's `AWAITING_REVIEW → COMPLETE` edge was drawn and unreachable

**Status:** ACCEPTED
**Date:** 2026-08-19
**Slice:** S13 (found while deciding what a review `accept` is allowed to do)

---

## Question

§5.2's review machine ends in a decision:

```text
PENDING ─export─► EXPORTED ─import─► DECIDED
          {accept · repair · revise_plan · pause · stop}
```

and §5.2's task machine draws `AWAITING_REVIEW → COMPLETE`. So: **when a human
accepts a review, what authorizes the task to complete?**

## Why the answer matters to Conductor

This is the question S13 exists to answer, and it has a wrong answer that is very
easy to reach. `accept` could simply write `COMPLETE`. That would make the review
bridge work, every acceptance test pass, and §4.5 advisory — because the same door
that lets a human accept a `CONTRADICTED` verdict would let one accept a task whose
tests never passed. §4.5's *"verification is authoritative"* and §4.4's *"policy
wins over green tests"* are both one convenience away from being decoration, and
the convenience is spelled `accept`.

## Evidence

Two measurements, both from the current tree at `ddc5ffe`.

**1. The edge has no writer.** `AWAITING_REVIEW` appears in both state machines,
`ReconciledRoute::AwaitingReview` exists, `requires_human()` includes it, and
`escalate_from_repairing` writes it. Nothing takes it back out:

```
$ grep -n "required: RunState::AwaitingReview" crates/conductor-store/src/lease.rs
(no matches)
```

`lease.rs` exports `advance_to_reconciling`, `route_reconciled` (requires
`Reconciling`), `route_verified` (requires `Verifying`), `reopen_for_repair` and
`escalate_from_repairing` (both require `Repairing`), and `resume_after_grant`
(requires `AwaitingApproval`). `advance_state` — the one function that could take
any other edge — is private to the module. So every route *into* `AWAITING_REVIEW`
was built and no route *out* of it was.

**2. The gate would refuse anyway.** `ReconciledRoute::Complete` carries a
`VerifiedComplete` whose only constructor is `completion::evaluate`, and criterion
6 requires the verdict to be `CLEAN_COMPLETE` or `CLEAN_NO_REPORT`. Every verdict
that routes a run to `AWAITING_REVIEW` in the first place — `CONTRADICTED`,
`OUT_OF_SCOPE`, `GOVERNANCE_VIOLATION`, an unauthorized `POLICY_SENSITIVE` — is a
verdict criterion 6 refuses. So even with a writer, `accept` would move the run to
a state the gate then refuses to certify.

This is the third instance of one shape in this project: **ADR-0013** found
criterion 7 unreachable behind criterion 6, **ADR-0017** found the core verb never
calling the plan ledger, and this is the review edge. A mechanism drawn, agreed,
and never given a caller. The standing check CLAUDE.md records — *for every
mechanism a slice reports complete, name its product call site* — is what found
all three, and here it was applied before the slice claimed anything rather than
after.

## Decision

**A review acceptance is a human authorization that enters the completion gate as
evidence, exactly as a policy grant already does — and it resolves the review
boundary only.**

Three parts.

1. **The authorization is a §4.3 grant, not a new mechanism.** S8 already shipped
   `ApprovalKind::ReviewAcceptance`, `Subject::ReviewPacket { packet_id }`,
   `action_column() == "review.accept"`, and `ExpiryRule::Forbidden` (a review
   acceptance does not expire). All of it was built and none of it had a caller —
   the same finding as everything above, in the subsystem that would have been
   the natural place to invent a second one. `accept` uses it.

2. **It enters the gate as `ReconciliationEvidence::AcceptedAtReview { verdict,
   authorization }`.** This mirrors `AuthorizedPolicySensitive`, added at S9 for
   the same reason, and inherits its construction discipline verbatim: the
   authorizing grant is a *required field*, so "accepted" cannot be claimed by a
   caller that does not hold one. The verdict is carried verbatim and never
   rewritten to a clean one — a reader of the durable record must be able to see
   that a human accepted `CONTRADICTED`, because that is a materially different
   history from a run that was clean.

3. **Acceptance resolves the review boundary and nothing else.** Criterion 6 is
   satisfied; criteria 1–3 still require a `PASS` bound to the current tree,
   criterion 4 still counts findings no human has resolved, criterion 5 still
   wants its acceptance bindings, and criterion 7 still wants its grants. When the
   gate still refuses, `accept` refuses **and names the criterion**. The decisions
   that exist for a failing check are `repair`, `revise_plan` and `stop`.

Findings are resolved by *writing* `finding.resolution` — the column §4.8 reserved
for *"a human decision path (S13)"* and which had no writer until now — so
resolution is a recorded act with an author, not a side effect of acceptance.

## Alternatives rejected

**`accept` writes `COMPLETE` directly, bypassing the gate.** Rejected: it makes
`VerifiedComplete`'s single-constructor guarantee false, and that guarantee is the
reason S3 could say *"no code path, tested or untested, can route a run to
`COMPLETE`"*. One bypass and the sentence stops being true of the system.

**Acceptance satisfies every criterion.** Rejected, and mutation-tested rather
than merely argued: making `evaluate` return `Ok` for any accepted review is
killed by `accepting_a_review_does_not_excuse_a_check_that_did_not_pass` and
`accepting_a_review_does_not_excuse_an_unresolved_finding`. Both were written
before the variant existed and were confirmed to fail against the permissive
version.

**A new `Criterion::ReviewAcceptance` (an eighth criterion).** Rejected: §4.5 has
seven criteria and acceptance is not a new thing to check — it is an answer to
criterion 6's question. Adding an eighth would also mean every existing caller's
exhaustive `match` over `Criterion::ALL` gains an arm that is `Ok` whenever nobody
reviewed anything, which is a criterion that passes by default. Criterion 6's
existing shape — a verdict plus what resolved it — already expresses this.

**Rewriting the verdict to `CLEAN_COMPLETE` on acceptance.** Rejected: it is the
cheapest implementation and it destroys the audit trail §4.8 exists to keep. The
measured fact must survive the decision made about it.

## Consequences

- One variant and one match arm added to `conductor-core::completion`. No new
  criterion, no change to the other six, no new approval kind.
- `lease.rs` gains the missing writer whose `required` state is
  `RunState::AwaitingReview`, modelled on `resume_after_grant`.
- `finding.resolution` gains its first and only writer.
- **The master plan's §4.5 and §5.2 are amended** to record that the
  `AWAITING_REVIEW → COMPLETE` edge is authorized by a `REVIEW_ACCEPTANCE` grant
  carried as reconciliation evidence, and that acceptance does not excuse the
  other six criteria.
- An acceptance-suite consequence: rows 6, 14, 15, 26, 28 and 29 all end at
  `AWAITING_REVIEW`, and before S13 none of them had a route onward. They are
  still scored at `AWAITING_REVIEW` — that is their specified final state — but
  the state is no longer a dead end, and S13's tests exercise the exit.
