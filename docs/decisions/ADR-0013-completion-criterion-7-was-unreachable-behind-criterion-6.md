# ADR-0013 — §4.5's criterion 7 was unreachable behind criterion 6

**Status:** ACCEPTED
**Date:** 2026-08-15
**Slice:** S9 (found by running acceptance row 12 to completion for the first time)

---

## Question

§4.5's completion gate has seven criteria. Two of them are:

> 6. Reconciliation verdict ∈ {`CLEAN_COMPLETE`, `CLEAN_NO_REPORT`}.
> 7. Every policy-sensitive action has a matching, unexpired, correctly scoped
>    grant.

Read literally and applied in order, **can criterion 7 ever be the deciding
criterion?**

## Why the answer matters to Conductor

Criterion 7 is the entire point of the approval subsystem inside the completion
gate. It is the sentence that says a human's grant is what permits a
policy-sensitive change to complete. If it is structurally unreachable then §4.3,
§4.4 and S8's whole apparatus terminate in a gate that never consults them, and
acceptance rows 12 and 13 are satisfied by a run that waits for a human, receives
an answer, and then refuses to complete regardless of what the answer was.

## Experiment / evidence

S9 wired the policy gate into the run path and ran acceptance row 12 end to end
for the first time: a fixture adds a dependency, policy resolves
`require_approval`, a request is raised, a human grants it, the run resumes.

The run reached the completion gate and was refused:

```
Stopped { state: AwaitingReview,
          reason: "ReconciliationVerdict: the reconciliation verdict is
                   POLICY_SENSITIVE, not CLEAN_COMPLETE or CLEAN_NO_REPORT" }
```

The grant had been authorized. The binding matched. The grant was consumed. And
the gate refused on **criterion 6**, before criterion 7 was ever consulted.

The reason is structural, not a bug in the evidence: a policy-sensitive action is
policy-sensitive because a sensitive path changed, and *it is still changed after
the human approves it*. Approval does not un-modify `Cargo.toml`. So the verdict
is `POLICY_SENSITIVE` on the resumed reconciliation too, and criterion 6 refuses
every time.

Therefore: **every run that criterion 7 could speak about is refused by criterion
6 first.** Criterion 7 could only ever be reached by a run with no
policy-sensitive action — a run about which it has nothing to say.

## Observed result

Criterion 7 was unreachable by construction, and had been since S4 shipped the
gate. It was invisible because `PolicyEvidence` had exactly one variant,
`NotEvaluated`, which routed to `deferred` — so the criterion never refused, never
passed, and never ran. The single-variant enum was deliberate (S4's comment says
adding a variant is what makes `evaluate` stop compiling until the owning slice
wires it in) and it worked exactly as intended: the compiler brought S9 here.

## Decision

Criterion 6 is read as excluding the verdicts **nobody has resolved**, and an
authorized `POLICY_SENSITIVE` is resolved — by criterion 7, which exists for
precisely this case.

Concretely, two variants are added to `conductor-core`:

```rust
ReconciliationEvidence::AuthorizedPolicySensitive { verdict, authorization }
PolicyEvidence::{ NoSensitiveActions, AllGrantsPresent { detail } }
```

`AuthorizedPolicySensitive` **cannot be constructed from a verdict alone** — the
authorizing evidence is a required field, so a caller can only claim
"authorized" if it has something to name. `CORRUPT`, `CONTRADICTED`,
`OUT_OF_SCOPE` and `NO_CHANGE` continue to refuse through `NotClean`, unchanged.

`NoSensitiveActions` and `NotEvaluated` are kept distinct on purpose: "we looked
and there was nothing to authorize" and "nobody looked" must never be conflated,
because only one of them is evidence.

The run-side value is **derived from the run's own state**, not passed in as a
flag: a run that reached `VERIFYING` with a policy-sensitive verdict got there
through `policy_gate::route_reconciliation`, which only permits it after the
action was allowed by a rule or authorized by a consumed grant. The grant id is
read back from the database so the completion evidence names *what* authorized
it rather than merely asserting that something did.

## What this DOES prove

- Acceptance row 12 completes: the granted run reaches `COMPLETE`, and
  `Cargo.toml` is present in the integrated commit read back from git.
- Criterion 7 now has three distinguishable outcomes rather than one, and the
  match in `evaluate` is exhaustive over them.
- The negative direction still holds: a run whose policy *denies* the action
  never reaches the gate at all (it routes to `AWAITING_REVIEW` from the policy
  gate), and a run whose grant was revoked or expired never leaves
  `AWAITING_APPROVAL`. Both are tested.

## What this DOES NOT prove

- It does not prove the grant covers *every* sensitive change in the diff. The
  evidence names the action that gated the run; a diff touching two sensitive
  paths where policy gates one and silently permits the other would satisfy this.
  The join over observed actions takes the strongest effect, which makes that
  case route to approval — but the *evidence string* names one action, and a
  reviewer reading it should not infer exhaustiveness.
- It says nothing about criterion 5 (`AcceptanceBindings`), which remains
  deferred to S11 and still routes to `deferred` exactly as before.
- It does not revisit whether §4.5's seven criteria are the right seven.

## Pre-registered falsification / revisit trigger

- A run reaching `COMPLETE` with `AuthorizedPolicySensitive` and **no** consumed
  grant and **no** allowing rule. The authorization string would read as the
  fallback text; if that is ever observed in a completion, the derivation in
  `vertical::policy_position` is wrong and this record is revisited.
- S11 wiring criterion 5, which will make `evaluate` stop compiling in the same
  way and is the natural moment to re-read all seven together.

## Impacted master-plan sections

- **§4.5** — criterion 6's statement is qualified: the excluded set is the
  *unresolved* non-clean verdicts, and an authorized `POLICY_SENSITIVE` is
  admitted by criterion 7.
- **Part 9, rows 12, 13** — reachable and scored.
- **Part 5.1 / `conductor-core::completion`** — two evidence enums gain variants.
