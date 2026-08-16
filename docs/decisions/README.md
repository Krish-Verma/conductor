# Decision Records

## Convention

```
docs/decisions/ADR-NNNN-short-slug.md
```

`NNNN` is a zero-padded sequence, assigned once and never reused. A record is never
edited to change its conclusion — it is superseded by a new record that names it.

Every record uses this structure:

```
ID · Title · Status · Date
Question
Why the answer matters to Conductor
Experiment / evidence
Observed result
Decision
What this DOES prove
What this DOES NOT prove
Pre-registered falsification / revisit trigger
Impacted master-plan sections
```

`Status` is one of `ACCEPTED`, `SUPERSEDED by ADR-NNNN`, or `OPEN`.

The two sections that are not optional are **"What this DOES NOT prove"** and
**"Pre-registered falsification"**. A measurement without a stated limit invites
over-reading, and a decision without a stated reversal condition cannot be
falsified — it can only be defended.

## Relationship to the master plan

`docs/architecture/CONDUCTOR-MASTER-PLAN.md` is the authoritative implementation
specification. Decision records are the durable evidence behind individual claims
in it.

**They must not duplicate each other.** Where a measurement is already recorded in
master plan Part 0 (the `M1`–`M27` table), the ADR states the decision and its
revisit trigger and *cites* the Part 0 row rather than restating the numbers. Two
copies of a number are two things that can disagree.

## Index

| ID | Title | Status |
|---|---|---|
| [ADR-0001](ADR-0001-workspace-isolation.md) | Run workspaces are `git clone --no-hardlinks`, not worktrees | ACCEPTED |
| [ADR-0002](ADR-0002-execution-containment.md) | Execution containment is measured per (adapter × launcher), never declared | ACCEPTED |
| [ADR-0003](ADR-0003-claude-hook-reliability.md) | Claude Code `PreToolUse` hooks are audit-grade, not gate-grade | ACCEPTED |
| [ADR-0004](ADR-0004-sqlite-claim-safety.md) | `BEGIN IMMEDIATE` single-statement claim is the run-claim mechanism | ACCEPTED |
| [ADR-0005](ADR-0005-claim-latency-is-load-dependent.md) | The claim holds under `rusqlite`; its latency trigger was measuring offered load | ACCEPTED |
| [ADR-0006](ADR-0006-the-isolation-test-recipe-was-vacuous.md) | The master plan's own hostile-workspace test recipe was vacuous in two ways | ACCEPTED |
| [ADR-0007](ADR-0007-blake3-is-the-only-digest.md) | BLAKE3 is the only content digest; the `sha256` column was incidental wording | ACCEPTED |
| [ADR-0008](ADR-0008-progressed-called-a-regression-progress.md) | `progressed()` called a regression progress | ACCEPTED |
| [ADR-0009](ADR-0009-the-repair-bound-must-be-durable.md) | The repair bound must be durable, not in-memory | ACCEPTED |
| [ADR-0010](ADR-0010-the-deny-cap-is-scoped-to-a-rules-own-facts.md) | The deny cap is scoped to a rule's own `when:` facts | ACCEPTED |
| [ADR-0011](ADR-0011-the-credential-boundary-had-two-holes-the-environment-cannot-close.md) | The credential boundary had two holes the environment cannot close | ACCEPTED |
| [ADR-0012](ADR-0012-the-state-machine-could-not-express-two-refusals-it-was-required-to-make.md) | The state machine could not express two refusals it was required to make | ACCEPTED |
| [ADR-0013](ADR-0013-completion-criterion-7-was-unreachable-behind-criterion-6.md) | Completion criterion 7 was unreachable behind criterion 6 | ACCEPTED |
| [ADR-0014](ADR-0014-out-of-scope-could-not-express-unconditionally.md) | `OUT_OF_SCOPE` could not express "unconditionally" | ACCEPTED |
| [ADR-0015](ADR-0015-approval-survives-the-store-as-a-receipt.md) | Approval survives the store as a receipt, not as a re-decision | ACCEPTED |

*(ADR-0008 through ADR-0014 were absent from this index until S11 — the records
existed, the table stopped at 0007. Recorded rather than silently fixed: an index
nobody maintains is how a reader concludes a decision was never written down.)*
