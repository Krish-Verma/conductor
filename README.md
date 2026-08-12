# Conductor

A local-first engineering execution control plane for coding agents.

Conductor takes a human-approved plan and executes it as bounded coding-agent tasks —
in isolated repository workspaces, with independent verification against the
repository rather than against the agent's own claims, and with recovery from durable
evidence when an agent or the host dies.

> Conductor never advances on the basis of an agent's self-report.

## What it does

Given an approved, versioned plan, Conductor:

- persists plan versions, decisions and durable task/run state,
- creates an isolated git workspace per run,
- launches and supervises a coding agent as a subprocess,
- reconciles what actually changed against the repository,
- runs verification itself and binds each result to the exact tree it observed,
- attempts bounded, loop-detected repair on failure,
- enforces global and project policy, and stops at approval gates,
- reconstructs state after a crash, timeout or reboot from git, the filesystem and
  its own durable records,
- and produces continuation and human-review packets.

## What it is not

- Not a foundation model, and not a code graph.
- Not a replacement for git, tests, CI, `CLAUDE.md` or `AGENTS.md`.
- Not a general workflow platform, and not a Temporal or Hatchet reimplementation.
- Not multi-machine, multi-tenant, or an agent marketplace.
- Not a code reviewer, credential broker, or transcript archive.
- **Not a repository-intelligence product.** See below.

## Conductor and Nerve are separate products

[Nerve](https://github.com/Krish-Verma/Nerve) is a separate repository-intelligence /
code-graph product. It answers *"what is true about this repository?"*. Conductor
answers *"what is the next authorized action, and what evidence is required before we
advance?"*.

Nerve is **not** a dependency, a prerequisite, or an architectural predecessor of
Conductor. Conductor v1 uses a built-in git + filesystem evidence provider and nothing
else. If a second evidence provider is ever genuinely needed, a generic provider
abstraction may be introduced then — post-v1.

## Status

**Design complete. S0 (measurement) complete. Implementation has not begun.**

No product code exists yet. The repository currently contains the architecture,
the decision records, and the measurement scripts backing them.

| Slice | What | State |
|---|---|---|
| S0 | Measurement and pre-registered falsification | **done** |
| S1 | Store foundation (SQLite schema, migrations) | not started |
| S2 | Workspace isolation | not started |
| S2.5 | Containment probe harness | not started |
| S3 | Fake agent, supervision, crash recovery | not started |
| S4 | Verification runner | not started |
| S5 | First vertical: task → agent → reconcile → verify → commit | not started |
| S6–S16 | Repair, policy, approvals, adapters, plan ledger, packets, review bridge, daemon, dogfooding | not started |

The full slice list, with acceptance criteria per slice, is master plan Part 8.

## Architecture

**[`docs/architecture/CONDUCTOR-MASTER-PLAN.md`](docs/architecture/CONDUCTOR-MASTER-PLAN.md)**
is the authoritative implementation specification. Everything else defers to it.

Some load-bearing decisions, each backed by measurement:

- **Runtime:** custom, single-machine, SQLite-backed. Not Hatchet, not Temporal —
  durable execution is not reconciliation, and neither removes Conductor's actual
  difficulty.
- **Workspaces:** `git clone --no-hardlinks`. A default local clone hardlinks its
  object store, and a write inside it corrupts the source repository ([ADR-0001](docs/decisions/ADR-0001-workspace-isolation.md)).
- **Containment:** measured per (adapter × launcher) on the host, cached by version,
  fail-closed when stale ([ADR-0002](docs/decisions/ADR-0002-execution-containment.md)).
- **Tool hooks:** audit-grade, not gate-grade — 100% fire rate, but confirmed
  bypasses ([ADR-0003](docs/decisions/ADR-0003-claude-hook-reliability.md)).
- **Language:** Rust, with a pre-registered falsification trigger.

## Repository layout

```
docs/
├── architecture/   authoritative design (master plan)
├── decisions/      ADRs — the evidence behind individual claims
├── archive/        superseded design documents, retained for provenance
└── reports/        slice completion reports
scripts/
└── measure/        reproducible measurement scripts + raw results
```

`crates/`, `tests/` and `fixtures/` do not exist yet. They are created by the slice
that first needs them — an empty skeleton would create false architectural
commitments.

## Environment

Developed and measured against:

| | |
|---|---|
| OS | macOS 26.6 (arm64) |
| Rust | 1.97.1 |
| SQLite | 3.51.0 |
| git | 2.50.1 |
| Claude Code | 2.1.228 |
| Codex CLI | 0.142.0 |
| Container runtime | none installed |

Measurements in the decision records are specific to these versions. The containment
probe re-measures on the host rather than trusting them.

## License

Not yet determined.
