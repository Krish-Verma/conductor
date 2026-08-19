# Conductor — Operating Contract

Read this before doing anything in this repository. It is short on purpose.

## Project identity

Conductor is a **local-first engineering execution/orchestration control plane for
coding agents**. It takes a human-approved plan and runs it as bounded agent tasks:
durable state, workspace isolation, agent supervision, crash recovery, repository
reconciliation, deterministic verification, bounded repair, policy, approval gates,
continuation and review packets.

Conductor does **not** decide product strategy. Strategic planning and high-level
review stay human-mediated.

## Product boundary — Nerve is a different product

Nerve is a separate repository-intelligence / code-graph product (CodeGraph /
GitNexus category). It has its own repository, architecture and roadmap.

- Nerve is **not** Conductor's runtime, state store, workflow engine, policy system,
  or architectural predecessor.
- **No Nerve dependency in v1.** No Nerve invocation. No Nerve-specific paths, exit
  codes, or CLI assumptions in Conductor code or config.
- v1's only repository-evidence mechanism is the built-in git + filesystem provider.
- **Do not create a `RepositoryEvidenceProvider` trait yet.** A one-implementation
  trait is a pure function wearing a costume. It gets written when a second provider
  genuinely exists.
- `docs/archive/` may mention Nerve because it records prior debate. That is history,
  not architecture.

## Authoritative design

```
docs/architecture/CONDUCTOR-MASTER-PLAN.md
```

That file is the implementation specification. `docs/archive/` is superseded and has
no authority. Decision records in `docs/decisions/` hold the evidence behind
individual claims and must not duplicate master-plan numbers — they cite them.

## Implementation discipline

- **One approved slice at a time.** The slice list is master plan Part 8.
- **Never skip a slice stop point.** Stopping is part of the architecture, not a
  courtesy. Do not flow into the next slice because the current one passed.
- **Inspect available skills before each substantial slice** and use the relevant ones
  rather than approximating their workflow by hand. Report which were used.
- **Repository, git and test evidence beat agent claims** — including your own.
- **Do not change architecture silently.** If a load-bearing assumption turns out to
  be false: stop, document the evidence, identify the impacted master-plan sections,
  propose the smallest correction, and report before continuing.
- **No new dependency** without justification recorded in the owning slice or an ADR.
- **No speculative abstractions.** A trait needs two implementations or fakeable I/O.
- **No generic workflow-platform features** — no DAG engine, scheduler, cron, DSL,
  replay engine, or distributed locks. Every runtime feature must trace to a row in
  the master plan's acceptance suite.

## Architectural invariants

Do not violate these without an ADR that first shows a current acceptance-suite need.

- Runtime: custom, single-machine, SQLite-backed. Not Hatchet. Not Temporal.
- Workspaces: `git clone --no-hardlinks --no-checkout`. Not worktrees. Not default
  hardlinked clones. (ADR-0001)
- Verification: bound to the exact tree hash; outcomes are `PASS` / `FAIL` /
  `INCONCLUSIVE` / `VOID`.
- Recovery: the agent report is evidence only. There is never `RUNNING → COMPLETE`
  without reconciliation.
- Policy: global and project rules are first-class. Locked global rules form a
  ceiling. Unknown action fails closed.
- Approval: a `0600` socket alone is **not** a human-identity boundary. Approval
  integrity depends on measured execution containment. (ADR-0002)
- Containment: measured per (adapter × launcher) on the host, cached by version, and
  **fail closed when stale**. Never hardcoded.

## Verification

- Run tests and acceptance criteria **independently**. A report is not proof.
- Prefer failure injection over happy-path assertions for anything touching security,
  recovery, or concurrency.
- Real TDD only: the test encodes the intended invariant and fails first. A test
  written after the code to confirm what the code already does is not a test.
- Every slice produces a completion report.
- Never summarize a failed check as passed.

## Git discipline

- Commits are slice-scoped. Never mix repository organization, measurement evidence,
  and product implementation in one opaque commit.
- Review `git diff` and untracked files before committing.
- Push only verified work. Never rewrite remote history.
- Never commit secrets, credentials, agent transcripts, or large logs.

## Current status

Design complete. **S0–S12 done. S13 (review bridge) not started.**
S12 is on `s12-packets`, not yet merged to `main`.

S9 was the hard gate: no real coding agent runs through Conductor before it
passes. It has. S10 shipped the Codex adapter; S11 shipped the plan ledger,
decisions, and the §3.5 recovery path that makes "project truth outlives
execution state" executable rather than aspirational.

S12 shipped the packets — and found that the two things it depended on had been
built, unit-tested and reported complete while being **unreachable from the product
path**: `conductor task run` never used S11's plan ledger (ADR-0017), and none of
§6.5's packets was ever delivered to an agent (ADR-0018). The repair packet was the
sharpest case: the driver built it, returned it for *reporting*, and launched the
attempt with something else, so the `do_not_retry` list that exists to stop attempt 2
from repeating attempt 1 had never been seen by attempt 2. S6's test asserted the
packet's contents from that returned value; its **name** claimed delivery.

The lesson is now a standing check, not a war story: **for every mechanism a slice
reports complete, name its product call site.** Both defects are invisible to any
test that starts below the CLI, and both survived a slice that declared them done.

Keep this line current. It was stale from S1 through S11 in the master plan's
own header and from S9 through S11 here, which is how a status note stops being
information.

`SECURITY.md` is the measured honesty table — no row may claim `PREVENTED`
without a named passing test and a positive control.

Create implementation directories only when their owning slice begins — an empty
skeleton creates false architectural commitments.
