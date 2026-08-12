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
master plan Part 0 (the `M1`–`M17` table), the ADR states the decision and its
revisit trigger and *cites* the Part 0 row rather than restating the numbers. Two
copies of a number are two things that can disagree.

## Index

| ID | Title | Status |
|---|---|---|
| [ADR-0001](ADR-0001-workspace-isolation.md) | Run workspaces are `git clone --no-hardlinks`, not worktrees | ACCEPTED |
| [ADR-0002](ADR-0002-execution-containment.md) | Execution containment is measured per (adapter × launcher), never declared | ACCEPTED |
| [ADR-0003](ADR-0003-claude-hook-reliability.md) | Claude Code `PreToolUse` hooks are audit-grade, not gate-grade | ACCEPTED |
| [ADR-0004](ADR-0004-sqlite-claim-safety.md) | `BEGIN IMMEDIATE` single-statement claim is the run-claim mechanism | ACCEPTED |
