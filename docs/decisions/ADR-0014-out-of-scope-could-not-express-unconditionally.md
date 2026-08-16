# ADR-0014 — `OUT_OF_SCOPE` could not express "unconditionally"

**Status:** ACCEPTED
**Date:** 2026-08-16
**Slice:** S11 (found while scoping acceptance row 29, before any code was written)

---

## Question

§3.3 control 1 says:

> `.conductor/**` is in the always-forbidden write scope. Any change to it
> arriving on a run branch is **rejected at reconciliation, unconditionally**,
> with a finding. Conductor never fetches such a change.

§3.3 calls `.conductor/**` an *"always-forbidden write scope"*, and §4.8 already
has a verdict for changes outside the declared scope: `OUT_OF_SCOPE`. **Can row
29 be implemented by reusing it?**

## Why the answer matters to Conductor

`.conductor/` holds the plan, the approval marker and the policy. It is inside
the agent's workspace by construction (§3.3's opening sentence), so an agent
*can* write it. The three controls in §3.3 are the only thing standing between
that and an agent approving its own plan. Row 29 is one of the nine rows the
master plan names as most distinguishing the design.

## Evidence

`crates/conductor-git/src/reconcile.rs` derives `Ord` on `Verdict` with the
variants in ascending precedence, matching §4.8's precedence line exactly:

```
NoChange < CleanComplete < CleanNoReport < OutOfScope < PolicySensitive
         < Contradicted < Corrupt
```

`reconcile()` returns **exactly one** verdict, highest wins. Therefore:

| Agent's changes | Verdict under the reuse plan | Route |
|---|---|---|
| `.conductor/policy.yaml` alone | `OUT_OF_SCOPE` | `AWAITING_REVIEW` ✅ |
| `.conductor/policy.yaml` **and** `Cargo.lock` | **`POLICY_SENSITIVE`** | **`AWAITING_APPROVAL`** ❌ |

The second row is the defect. `POLICY_SENSITIVE` outranks `OUT_OF_SCOPE`, so a
governance mutation arriving *alongside* a dependency change classifies as a
policy question. It then routes to a human as an **approval** request about the
dependency — and a human who grants it advances a run that is still carrying the
edit to `.conductor/policy.yaml`. The approval was never about the governance
change, and nothing subsequently asks.

That is not a hypothetical ordering: `POLICY_SENSITIVE` outranks `OUT_OF_SCOPE`
deliberately, because acceptance row 14 forces it (§4.8's own note). The
precedence is correct for the reason it was chosen and wrong for this rule.

So the word "unconditionally" is not decoration. Under the reuse plan the
rejection holds only on the condition that nothing higher-precedence
co-occurs — and an agent that wanted to slip a governance edit through would
simply touch a lockfile in the same attempt.

## Decision

**Add an eighth verdict, `GOVERNANCE_VIOLATION`, ranked directly below
`CORRUPT`.**

`CORRUPT` stays on top because a broken repository makes every other reading
unreliable. Nothing else outranks discovering that the rules themselves were
edited.

This extension was anticipated by the code rather than forced onto it. The
`From<Verdict> for ReconciliationEvidence` impl already said:

> *"Exhaustive on purpose: an eighth verdict added to §4.8 will not compile until
> somebody decides whether it may complete a task, which is the opposite of
> defaulting to 'clean'."*

That guard worked as designed: adding the variant broke exactly one match
(`worker.rs`'s `route_for`), which is the one place that had to decide.

**Precedence is the third layer of the guarantee, not the only one.** §3.3 says
"unconditionally", and a single mechanism is a condition. Three independent
things must all fail before a governance edit reaches the registered repository:

1. the `GOVERNANCE_VIOLATION` verdict, which routes to `AWAITING_REVIEW`;
2. a **`CRITICAL`** finding — §4.5's criterion 4 blocks `COMPLETE` on one, and
   §4.8 says findings never auto-resolve, so this holds even if the verdict were
   somehow wrong;
3. the run never reaching `VERIFYING`, and therefore never reaching the
   commit-and-fetch step at all.

`FindingKind::GovernancePath` is deliberately **not** a member of
`forces_policy_evaluation()`. A governance change is not a thing policy decides
about — routing it through policy evaluation is the exact indirection that lets
an approval carry it through.

## Consequences

- §4.8's verdict table becomes eight rows; Part 9 row 29 is now scorable from
  end-to-end evidence.
- Path matching is segment-aware, not a bare prefix test. S10 found a boundary
  bug of precisely this shape (`/workspace-other` matching `/workspace`), and the
  same mistake here would classify `.conductorized/` as governance and halt runs
  that touched nothing of the kind. `a_path_that_merely_begins_with_the_governance_prefix_is_not_governance`
  is the control.
- A path that *is* `.conductor` (a file where the directory belongs) counts as
  governance.

## Falsification

Removing the detection (`is_governance_path` → `false`) fails three named tests:

```
conductor-git  a_governance_file_written_on_a_run_branch_is_refused_unconditionally  FAILED
conductor-git  a_governance_change_outranks_a_policy_sensitive_one_arriving_with_it  FAILED
conductor-run  row_29_a_conductor_change_on_a_run_branch_is_rejected_and_never_fetched  FAILED
```

The second is the one that would have survived the reuse plan, and it is the
reason this ADR exists.
