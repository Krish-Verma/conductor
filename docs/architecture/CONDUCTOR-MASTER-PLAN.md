# Conductor — Master Architecture and Implementation Plan

**Version:** 1.0 (authoritative)
**Date:** 2026-08-12
**Supersedes:** `CONDUCTOR-ARCHITECTURE-REVIEW.md`, `CONDUCTOR-CONVERGENCE-PASS-01.md`
**Status:** design complete; no implementation has begun.

This document is self-contained. Someone with only this file should be able to build Conductor.

---

# Part 0 — Ground Truth

## 0.1 Repository state (verified 2026-08-12)

`github.com/Krish-Verma/conductor` — HEAD `6b830311`, one commit ("Initial commit"), one blob (`README.md`, 11 bytes, `# conductor`), `refs/heads/main` only, repo size 0. No source, no schema, no migrations, no CI, no `.conductor/`. **Greenfield.**

Any assertion that Conductor has existing state — a table, a migration, an applied schema — is false unless re-verified against the repository. This rule applies to every source including this document.

## 0.2 Environment (measured on the target machine)

macOS 26.6 arm64 · Rust 1.97.1 · Node v24.15.0 · SQLite 3.51.0 · git 2.50.1 · Claude Code 2.1.228 · codex-cli 0.142.0 · **no container runtime** (`docker`, `podman`, `colima` all absent) · `/usr/bin/sandbox-exec` present.

## 0.3 Measured facts that determine the architecture

These are experimental results from this machine, not assumptions. Everything in Parts 1–4 that depends on them cites them.

| # | Finding | Measurement |
|---|---|---|
| **M1** | Default `git clone` hardlinks loose objects **and** packs | source & clone `inode=209190344`, `nlink=2` |
| **M2** | A same-user write inside such a clone **corrupts the source repository** | source `fsck` 0 → 128; `fatal: loose object … is corrupt`; `cat-file HEAD` FAILED |
| **M3** | `--no-hardlinks` isolates completely | distinct inodes; after corrupting the clone, source `fsck` exit 0, all objects readable |
| **M4** | `--no-hardlinks` is **faster**, not slower | 22 MB `.git`: 0.59 s vs 1.47 s default; +checkout 0.76 s; `--no-local` 0.94 s |
| **M5** | Disk cost of `--no-hardlinks` | +23.3 MB per run at 22 MB `.git` |
| **M6** | Codex `workspace-write` denies writes outside the workspace | `$HOME`, `$HOME/Documents`, sibling dirs, `/Users/Shared`, `~/.ssh`, `~/.codex`, `~/.claude` → `Operation not permitted` |
| **M7** | …with two exceptions | `/tmp` and `$TMPDIR` are writable |
| **M8** | Restriction is inherited by child processes | nested `sh -c` write outside → denied |
| **M9** | Codex denies network egress | TCP `PermissionError [Errno 1]`; DNS fails; `curl` cannot resolve |
| **M10** | **Codex denies AF_UNIX connect by default** | denied at `/tmp/*.sock`, `$HOME/*.sock`, `$HOME/.conductor-*.sock` — path was known in every case |
| **M11** | M10 is real, not a broken test | positive control: same connect **succeeds** with `--allow-unix-socket`, `CONNECTED: b'ALLOWED_VIA_FLAG'` |
| **M12** | Codex does **not** restrict reads | read planted secret, `~/.codex/auth.json` (`{"auth_mode":"chatgpt","OPENAI_AP…`), `~/.ssh/known_hosts` |
| **M13** | Codex sandbox is a general-purpose wrapper | `codex sandbox -- claude --version` → `2.1.228 (Claude Code)`, exit 0 |
| **M14** | Codex default sandbox is read-only | write to own cwd denied without `-c sandbox_mode=workspace-write` |
| **M15** | Codex propagates exit codes | `exit 42` → 42 |
| **M16** | Claude Code 2.1.228 has **no** `--permission-prompt-tool` | absent from `--help` |
| **M17** | Claude Code `PreToolUse` hooks can deny a tool call | observed directly: a hook blocked `WebFetch` and returned a redirect message |
| **M18** | `PreToolUse` fire rate for Bash is 100%, and the hook sees the raw command | 10/10 Bash tool calls invoked the hook, incl. `sh -c '…'` and `cd . && …` (ADR-0003) |
| **M19** | Hook bypass confirmed end-to-end, not theorized | `$(echo git) push` and `g=git; $g push` passed the hook and reached git (ADR-0003) |
| **M20** | `--settings` alone is **not** hermetic; `--setting-sources` is required | ambient `SessionStart` hooks fired until `--setting-sources project` was added (ADR-0003) |
| **M21** | `BEGIN IMMEDIATE` claim: zero duplicate ownership | 38,800+ claims across 1/4/16 writer processes, 4 configurations, 0 dupes (ADR-0004) |
| **M22** | `synchronous=FULL` is **not** media-durable on macOS | `fullfsync=1` needed for `F_FULLSYNC`; median 0.065 ms → 2.733 ms (ADR-0004) |
| **M23** | Claim correctness survives the stack change to `rusqlite` | 39,400 claims, 1/4/16 writers, 5 configurations, **0 duplicates**, 0 invariant failures (ADR-0005) |
| **M24** | **p99 claim latency is a function of offered load, not of the mechanism** | 4 writers, `fullfsync=1`: 265 ms saturated → **24.6 ms** at a 25 ms inter-claim gap; median flat at 3.4 ms (ADR-0005) |
| **M25** | `fullfsync=1` is genuinely applied by `rusqlite`, not silently dropped | reads back `1`; ~9× median cost vs `fullfsync=0` (3.44 vs 0.36 ms at 4w) corroborates `F_FULLSYNC` (ADR-0005) |
| **M26** | Worst-case claim latency moved 1093 ms → **5147 ms** under `rusqlite` | 16 writers saturated; narrows the 60 s lease margin from ~55× to **~12×** (ADR-0005) |
| **M28** | Claude Code under `codex sandbox` measures **identical to Codex** on all four gating dimensions | Restricted/Hard/Hard/None, exceptions `/tmp`+`$TMPDIR`. Confirms containment is a launcher property (M13). Does **not** show Claude *functions* there — the same sandbox denies the network it needs (S2.5) |
| **M29** | First execution of a freshly built binary costs **~228 ms cold vs ~3.9 ms warm** — *corrected at S4* | **The original S2.5 figure (21.7 s / 3.3 s) was wrong**: it timed a whole `codex sandbox` probe run and attributed all of it to the OS binary scan. Re-measured at S4 over 3 cold/warm pairs and independently reproduced (234/228/224 ms cold vs 3.3/3.6/4.8 ms warm). The scan lands **after** `spawn()` returns (spawn is ~200–350 µs either way), so "start the clock after spawn" does **not** absorb it — a startup grace period until *first output* does. Still real, still worth absorbing; two orders of magnitude smaller than recorded |
| **M27** | Three of Part 5.1's six pragmas are already the dependency's defaults | `libsqlite3-sys` compiles `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`; `rusqlite` calls `sqlite3_busy_timeout(db,5000)` on open; `synchronous` sits at SQLite's compile default. Readback alone is weak evidence; `fullfsync` is the discriminating pragma (ADR-0005) |

**Two prior claims were refuted by these measurements** and the architecture below reflects the corrected position: (a) that a default local clone is an isolation boundary — it is not (M2); (b) that a `0600` unix socket plus environment scrubbing constitutes a human-only approval channel — it does not, except under a sandboxed launcher (M10/M11).

---

# Part 1 — Product

## 1.1 Definition

> Conductor is a local-first execution supervisor for coding agents. It takes a human-approved, versioned plan; runs it as bounded agent tasks in isolated repository workspaces; verifies results against the repository rather than against the agent's claims; reconstructs state from durable evidence when an agent or the host dies; refuses to advance past policy and approval boundaries; and produces the packets a human needs to review it. **It never advances on the basis of an agent's self-report.**

**Target user (v1):** one engineer, one machine, one or two repositories, currently running this loop by hand.

**Job to be done:** *"I approved a plan. Execute the next slice without me watching, don't let the agent lie to me about whether it worked, don't let it do anything I didn't authorize, and if it dies, know exactly where we are without asking me."*

## 1.2 Responsibility split

**Human owns:** vision, product decisions, strategic architecture, plan approval, architecture pivots, high-risk approvals, changing locked policy, review decisions.

**Conductor owns:** plan/decision durability and versioning, task materialization and sequencing, policy evaluation, workspace isolation, agent process lifecycle, evidence capture, verification, reconciliation, bounded repair, approval gating, Conductor-owned side effects, packet generation, recovery.

**Neither owns:** whether the code is correct. That belongs to tests. Conductor's job is to run them honestly.

**Why the boundary sits there:** a plan is the only input Conductor cannot verify. Everything else has a deterministic oracle — git state, exit codes, file hashes. Automating the production of the only oracle-free input, then executing it unattended, is how a system confidently does the wrong thing for six hours.

## 1.3 Non-goals

Not a foundation model · not a code graph · not a replacement for git, tests, CI, `CLAUDE.md`, or `AGENTS.md` · not a general workflow platform · not a Temporal/Hatchet reimplementation · not multi-machine · not multi-tenant · not an agent marketplace · not a CI platform · not a code reviewer · not a credential broker · not a transcript archive · **not a repository-intelligence product.**

## 1.4 Conductor and repository intelligence are different products

Conductor answers *"what is the next authorized action, who performs it, what state are we in, what evidence is required to advance?"*

A repository-intelligence system (Nerve, CodeGraph, GitNexus — separate products with their own roadmaps) answers *"what is true about this repository?"*

Conductor v1 depends on exactly one evidence provider and knows of no others:

```
        ┌─────────────────────────────────────┐
        │          Conductor Core             │
        │  (no knowledge of any vendor)       │
        └──────────────────┬──────────────────┘
                           │
        ┌──────────────────▼──────────────────┐
        │      GitFilesystemProvider          │  ← the ONLY provider in v1.
        │      git + fs. Always present.      │    Concrete module, not a trait.
        └─────────────────────────────────────┘

  ─── post-v1, does not exist yet ───────────────────────────
        trait RepositoryEvidenceProvider  ← written only when a
        └── third-party providers            second provider is built
```

**Binding rules.** Core contains no vendor name, no third-party exit codes, no third-party paths, no assumption about another product's language or layout. Conductor v1 is built, tested, dogfooded and completed with no third-party provider installed; an acceptance test asserts the full suite passes with any such binary removed from `PATH`. A one-implementation trait is a pure function wearing a costume, so the trait is not written until it has two implementations.

---

# Part 2 — Architecture

## 2.1 System shape

```
┌──────────────────────────────────────────────────────────────────────┐
│ OPERATOR SURFACE — trusted                                           │
│   conductor CLI → unix socket at $HOME/.conductor/conductor.sock     │
│   mode 0600 · approval verbs live ONLY here                          │
│   (enforcement of "only here" is a property of the launcher: §4.3)   │
└────────────────────────────────┬─────────────────────────────────────┘
                                 │ line-delimited JSON-RPC
┌────────────────────────────────▼─────────────────────────────────────┐
│ CONDUCTOR CORE — one process (foreground S1–S13, daemon from S14)    │
│                                                                       │
│  ┌── pure domain — no I/O, exhaustively tested without a runtime ──┐ │
│  │  next_action(state, evidence)        -> Decision                │ │
│  │  reconcile(baseline, observed)       -> Reconciliation          │ │
│  │  evaluate(snapshot, action, facts)   -> (Effect, Explanation)   │ │
│  │  classify(attempt_evidence)          -> AttemptOutcome          │ │
│  │  progressed(prev_failure, next)      -> bool                    │ │
│  │  eligible(requirements, measured)    -> Eligibility             │ │
│  └─────────────────────────────────────────────────────────────────┘ │
│                                                                       │
│  ┌── I/O seams — two traits, one concrete store ───────────────────┐ │
│  │  trait WorkspaceProvider   trait AgentAdapter   Store (SQLite)  │ │
│  │  GitFilesystemProvider · Verifier · Packets · Policy loader     │ │
│  └─────────────────────────────────────────────────────────────────┘ │
└──────┬───────────────────────┬──────────────────────┬────────────────┘
       │                       │                      │
┌──────▼──────┐   ┌────────────▼─────────────┐  ┌─────▼──────────────┐
│  SQLite     │   │  RUN WORKSPACE           │  │  ARTIFACTS         │
│  WAL        │   │  per-run clone           │  │  ~/.local/share/   │
│  sync=FULL  │   │  --no-hardlinks   (M3)   │  │  packets, reports, │
│  execution  │   │  origin REMOVED          │  │  jsonl, logs,      │
│  state only │   │  hooksPath=/dev/null     │  │  diffs             │
└─────────────┘   │  scrubbed env, own HOME  │  │  content-addressed │
                  │  own TMPDIR              │  └────────────────────┘
                  │                          │
                  │  ┌────────────────────┐  │
                  │  │  AGENT SUBPROCESS  │  │  ← untrusted
                  │  │  launched under a  │  │  ← may run arbitrary shell
                  │  │  sandbox launcher  │  │
                  │  └────────────────────┘  │
                  └───────────┬──────────────┘
                              │ Conductor-owned integration ONLY:
                              ▼ git fetch run-branch → real repo
                  ┌──────────────────────────┐
                  │  USER'S REAL REPOSITORY  │  ← agent never holds a handle
                  └──────────────────────────┘
```

## 2.2 Language: Rust

**Decision: Rust for the engine. No second language in v1.**

The decisive argument is Conductor-specific and follows from the fact that most of Conductor's code will be agent-written. The usual case for TypeScript is human iteration speed — fewer keystrokes, less ceremony. When an agent writes the code, keystroke cost largely stops being a human cost. What does not transfer to the agent is *verification*: someone must still establish the generated code is correct. A compiler that mechanically rejects a missing state transition, an unhandled error or a moved value is an automated reviewer that never tires.

Conductor's whole thesis is *do not trust an agent's self-report; verify mechanically.* Building that in the weaker of two available verification regimes, because the agent writing it would type less, is self-contradictory at the level of the product.

Then the ordinary comparison, on Conductor's own properties:

**Rust pays where errors are silent and durable.** Twelve task states and five other state machines with "no invalid transition" as an explicit deliverable — exhaustive `match` makes adding a state a compile error at every site, where TypeScript's equivalent is opt-in per switch and degrades silently when forgotten. Child-process lifecycle is the highest-leak surface in the system (spawn, stream, timeout, `SIGTERM`→`SIGKILL`, reap, orphan-detect, on every path including panics); ownership and `Drop` make "always reaped" structural rather than disciplinary, where Node's `child_process` accumulates zombies and dangling handles in long-running daemons. And Conductor is the tool you run when the environment is broken, so a single static binary independent of a runtime install is a functional requirement.

**Rust costs where protocols churn.** Agent CLIs are unstable — the Claude Code docs alone cite behavioural changes at v2.1.163, .182, .205, .211, .219, .221, .223. Tolerating unknown JSONL fields is free in TypeScript and deliberate in Rust. Compile times tax the agent's edit-verify loop.

**The tie-breaker:** Rust's costs concentrate in one small, isolated, fixture-testable layer (adapter parsing), while its benefits spread across state machines, supervision, transactions and recovery. Mitigations are cheap: workspace crates for incremental checks, `cargo check` not `cargo build` in the loop, and permissive serde on all agent input (`#[serde(flatten)]` catch-alls, **never** `deny_unknown_fields` on anything an agent produced).

**Pre-registered falsification.** If after S1–S5 more than 30% of commits are type/serde plumbing with no behaviour change, or median `cargo check` in the agent loop exceeds 90 s, this decision is wrong and must be reversed while reversal is still cheap.

> ### ✅ EVALUATED AT S5 — **the trigger does not fire, and it is not close.**
>
> | Slice | median `cargo check --all-targets` | plumbing share |
> |---|---:|---:|
> | S1 | 0.33 s | 0 of 1 commit |
> | S2 | 0.33 s | 0 of 2 |
> | S2.5 | 1.23 s | 0 of 3 |
> | S3 | 3.60 s | 0 of 4 |
> | S4 | 2.21 s | ~15 % |
> | **S5** | **3.09 s** | **~12 %** |
>
> Against thresholds of **90 s** and **30 %**: 29× inside on compile time, and less than
> half the plumbing ceiling. Check time has been **flat since S3** even as the workspace
> grew ~35 % — the workspace-crate split (§2.3) is doing the work it was chosen for.
>
> **The qualitative half is the more interesting result.** §2.2 argued the compiler would
> act as "an automated reviewer that never tires". It has, repeatedly and specifically:
> the exhaustive `match` on `Precondition` fired the instant S5 added the two git effect
> kinds and pointed straight at the file that had forgotten them; S4's completion gate
> uses the same mechanism deliberately, so S7 and S11 **cannot compile** without deciding
> what their evidence means. Those are exactly the "silent, durable" errors §2.2 predicted
> Rust would catch and TypeScript would not.
>
> **Decision: Rust stands. This trigger is retired for v1** — it was pre-registered for
> S1–S5 and has been answered. Revisit only if a later slice shows a *qualitative* change
> (a protocol-churn layer that turns into serde thrash), not on compile-time drift alone.

**Dependencies:** `rusqlite` (bundled), `clap` (derive), `serde`/`serde_json`/`serde_yaml`, `blake3`, `thiserror`/`anyhow`, `tokio` (process supervision), `tempfile` (dev). Git is invoked as a **subprocess**, never via `libgit2`/`gix` — Conductor's job is to observe the same state the user observes, and the user's ground truth is the `git` binary including their `core.*` settings, hooks and filters.

## 2.3 Crate layout

```
conductor/
├── Cargo.toml                       workspace
├── crates/
│   ├── conductor-core/              PURE. types, state machines, decision fns.
│   │                                deps: serde, blake3, thiserror. NO I/O.
│   ├── conductor-store/             SQLite schema, migrations, transactions
│   ├── conductor-git/               WorkspaceProvider, baseline, reconciliation
│   ├── conductor-agent/             AgentAdapter trait, fake, codex, claude
│   ├── conductor-run/               supervision, leases, verification, policy,
│   │                                approvals, packets, orchestration
│   └── conductor-cli/               clap binary + socket client + daemon
└── tests/                           integration + acceptance suite
```

**Split rule:** a new crate is justified by a real dependency boundary or a measured compile-time win, never by taxonomy. `conductor-core` having no I/O dependencies is the one boundary that is load-bearing — it is what makes the domain testable without a runtime.

## 2.4 Process model

**Foreground supervisor for S1–S13. Daemon from S14.**

`kill -9` on a foreground process is the cheapest crash test in existence, and S1–S9 are almost entirely about surviving it. Adding daemon lifecycle, socket handling, stale-PID logic and launchd integration before recovery semantics are proven means debugging two things at once.

Lease-based claiming (§5.2) is designed in from S1, so the daemon is a new entry point, not a rewrite. It arrives when its acceptance criterion is independently testable: *close the terminal, the run continues, `conductor status` from a new shell reports it.*

## 2.5 Domain boundaries

**Two traits, one concrete store, everything else is functions.**

A trait earns existence when it has more than one real implementation, or I/O that tests must fake. Measured against that:

| Candidate | Verdict |
|---|---|
| `AgentAdapter` | **Trait.** Three implementations (fake, codex, claude); the fake is load-bearing for the whole test suite. |
| `WorkspaceProvider` | **Concrete for now** — revised at S2. The stated justification ("tests need a fake") was falsified by building the tests: all 88 run real `git` against real disposable repositories, which is *strictly stronger* evidence for an isolation boundary than a fake could ever give — a faked clone cannot demonstrate that a real one does not share inodes. One implementation, no fake needed, so per CLAUDE.md a trait here would be a pure function in a costume. Promote when a second isolation strategy actually exists, or when a later slice needs to fake workspace I/O for speed — same rule as `Verifier`. |
| `Store` | **Concrete.** One SQLite database, one transaction domain. Splitting into `RunStore`/`ApprovalStore`/`ArtifactStore` would invite a write spanning two "stores" and therefore two transactions — precisely the bug class §5 exists to prevent. |
| `PolicyEvaluator` | **Function.** Pure, no I/O, one implementation. |
| `Verifier` | **Function for now.** One implementation (spawn a command); tests use fixture commands. Promote only if a second execution strategy appears. |
| `RepositoryEvidenceProvider` | **Not yet.** One implementation. Trait when a second exists (§1.4). |

The pure core *is* the portability insurance. If the execution substrate ever changes, six total functions with no I/O are unaffected — stronger than an interface guessing at what a future substrate will need.

**Explicitly rejected: a `WorkflowRuntime` trait.** An interface with one implementation, designed to ease adopting a backend we have argued against, that leaks `claim`/`heartbeat`/`lease` into domain code. It is the most likely single path to accidentally building Temporal.

## 2.6 Runtime: build it, don't adopt one

**Decision: custom, single-machine, SQLite-backed. Not Hatchet. Not Temporal.**

Conductor has ten hard problems. A workflow engine removes two.

| Hard problem | Removed by a workflow engine? |
|---|---|
| Isolating a repository from an agent with a shell | No |
| Determining what the agent actually did, from git | No |
| Deciding whether verification results are trustworthy | No |
| Policy algebra, locking, scoped exceptions | No |
| Binding an approval to one exact action | No |
| Reconstructing state after host death, from disk | **No — and this is the important one** |
| Supervising an agent subprocess, classifying its death | Partially |
| Not losing state when the daemon crashes | Yes |
| Timers, waits, retries | Yes |
| Distributed workers, queue fairness, HA | Yes — and Conductor has none |

**The structural reason, not the economic one: durable execution is not reconciliation.** Temporal guarantees your workflow reaches the same state given the same history. Conductor must determine the *true* state given whatever actually happened on disk — where the user may have edited the branch, the host may have rebooted, and the agent may have lied. A perfectly replayed workflow still has to go read `git status`. The expensive part is not removed; it is made invisible.

**Operational cost, from current docs.** Temporal's durable self-host path is Docker Compose with PostgreSQL **and Elasticsearch**; its single-binary dev server prints *"not intended for production use"* and, decisively, `--db-filename` is documented as *"by default, Workflow Executions are lost when the server process dies"* — the easy path is the non-durable one. Hatchet self-hosting means API server + gRPC engine + PostgreSQL + optional RabbitMQ + dashboard; `hatchet-lite` requires Docker and is *"for development and low-volume use-cases."* **This machine has no container runtime at all** (§0.2).

**Revisit triggers** (any one):
1. Execution spans more than one machine. *(The real trigger — everything else is downstream.)*
2. More than 3 concurrent human operators against shared state.
3. Sustained >50 concurrent runs, or **p99 claim latency >100 ms at ≤4 concurrent
   writers with `fullfsync=1` under `rusqlite`, measured at an inter-claim gap ≥25 ms
   per worker**. **Currently 24.6 ms** (ADR-0005, measured at S1).
   *(Scoped twice, each time because the previous wording fired for the wrong reason.
   ADR-0004 added the durability mode and concurrency after the unscoped version fired
   at a concurrency Conductor will never reach. ADR-0005 added the **arrival rate**:
   without it the trigger is satisfied by any benchmark that saturates the writer —
   under saturation this same configuration reaches 265 ms — and would then recommend
   Temporal, whose durable-write path is a Postgres round-trip plus a network hop and
   would make the measured quantity **worse**. A trigger whose firing recommends an
   action that worsens what it measures is measuring the wrong thing. Saturation
   numbers are retained in ADR-0005 as the worst-case ceiling; they are not this
   trigger.)*
4. A single run must stay suspended >30 days across daemon/OS upgrades.
5. **Runtime bug-fixes exceed 15% of commits over any 30-day window**, measured from `git log`. *(This is the falsification trigger for the decision itself.)*
6. Cross-run orchestration graphs — fan-out/fan-in with joins — rather than a sequential task queue.

If (1) or (5) fire: **Temporal over Hatchet**, because at that point the value is replay/versioning and ecosystem, not the queue.

## 2.7 Runtime scope — what is owned, what is forbidden

**Owned (~1,500 lines):** task/run states · atomic claims · leases · fencing epochs · heartbeats · attempt lifecycle · bounded retries · approval waits · startup reconciliation · side-effect intents/receipts · idempotency precondition checks.

**Forbidden without a written ADR:** generic DAG engine · fair/priority scheduler · cron framework · workflow DSL · replay engine · distributed locks · multi-region HA · worker marketplace · arbitrary workflow composition.

**Gate on every proposed runtime feature:** *which row of the acceptance suite (Part 5) requires this?* If none, defer it.

---

# Part 3 — Storage and Representation

## 3.1 The split

### Git — `<repo>/.conductor/` (committed, authoritative)

```
.conductor/
├── project.yaml            identity, adapter, scope defaults, review cadence,
│                           execution_requirements
├── policy.yaml             project policy rules
├── verification.yaml       check profiles, toolchain fingerprint commands
├── plans/
│   └── v3/
│       ├── plan.yaml       milestones · slices · tasks · acceptance criteria
│       │                   · scope globs · verification bindings
│       └── APPROVED        plan content hash · approver · timestamp · policy hash
└── decisions/
    └── D-0007-clone-not-worktree.md
```

Authoritative for **what we agreed to do, and what we are allowed to do.**

### SQLite — `~/.local/share/conductor/conductor.db` (local, disposable)

Authoritative for **what actually happened, and what is happening now.** Tasks, runs, attempts, leases, fencing epochs, event journal, verification executions, approvals, findings, side-effect ledger, adapter/session metadata, containment probe cache.

### Artifacts — `~/.local/share/conductor/artifacts/<run-id>/` (local, content-addressed, gitignored)

Packets, agent JSONL streams, raw verification logs, diffs, reports, review packets.

## 3.2 Why plans live in git

Four Conductor-specific requirements, each independently sufficient:

1. An approved plan must survive loss of `conductor.db`.
2. It must be reviewable as a diff, in a PR, by a human.
3. It must travel with the repository to another machine.
4. It must be readable without Conductor installed.

**Invariant, enforced by an acceptance test:** *deleting `conductor.db` loses no plan, no decision, no policy, and no verification definition.*

## 3.3 The security consequence of `.conductor/` being inside the repository

`.conductor/` lives in the repo, therefore inside the agent's workspace, therefore an agent **can write `.conductor/plans/v3/APPROVED` in its own clone**. Three controls, all required:

1. `.conductor/**` is in the always-forbidden write scope. Any change to it arriving on a run branch is **rejected at reconciliation, unconditionally**, with a finding. Conductor never fetches such a change.
2. Conductor reads plan approval **only** from the registered repository's working tree, never from a run branch.
3. The store records the approval independently at grant time. If file and store disagree, **execution halts** — it is never resynced.

## 3.4 Durable audit via git trailers

Conductor-owned commits carry trailers, so the audit trail for anything consequential survives total local state loss and travels with the repository:

```
Conductor-Run: r-0041
Conductor-Plan: v3@blake3:9ac2…
Conductor-Policy: blake3:41ef…
Conductor-Approval: AG-0019 binding=blake3:7d31…
Conductor-Verification: blake3:5b8e…
```

## 3.5 What is lost if `conductor.db` disappears

**Lost:** run/attempt history, timings, event journal, verification cache (recomputable), pending approval requests, unresolved findings, lease state, containment probe cache (re-probeable).

**Not lost:** every approved plan, every decision, all policy, all verification definitions, project identity, and — via §3.4 — which run, plan version, policy snapshot and approval produced every Conductor-authored commit.

**Recovery from total local loss:** re-register the project → read `.conductor/` → rebuild the task list from the approved plan → scan `workspaces/` for `.conductor-run.json` descriptors and reconcile each against git → read commit trailers to reconstruct which runs produced which commits under which approvals.

## 3.6 Canonical representation

**Plans, policy, verification config → canonical YAML. Decisions → Markdown with YAML frontmatter. Nothing is generated into a second committed file.**

- A **plan is a data structure** — milestones containing slices containing tasks, each with criteria bound to named checks, scope globs and dependencies. Prose is confined to `rationale:` and `objective:` fields. Markdown-with-frontmatter would put the load-bearing structure in the frontmatter and decoration in the body — exactly inverted.
- **Policy and verification are configuration.** YAML.
- A **decision is an argument.** Small fixed metadata (`id`, `status`, `supersedes`, `date`) in schema-validated frontmatter; the value is prose a human reads and a packet quotes.

**No generated duplicates.** Rendering plans to Markdown and committing both creates two representations that silently disagree the first time someone edits the wrong one. `conductor task show` renders on demand, to stdout, never to a tracked file. *(Corrected at S11: this said `conductor plan show`, which §7.1's 13 commands do not include — §7.1 folded plan rendering into `task show` and `status`.)*

**Hash semantics, not bytes.** The plan hash is computed over parsed-and-canonically-reserialized content: keys sorted, LF endings, no trailing whitespace, no timestamps. Therefore reformatting does **not** invalidate approval; changing any field **does**. YAML comments are excluded from the hash and **must not carry meaning** — `plan validate` warns on comments over a threshold, because a long comment is usually load-bearing prose in the wrong place.

**Stable IDs:** `M-01`, `S-05`, `T-0012`, `D-0007` — assigned once, never reused. A revision that preserves a task's meaning preserves its ID; a task whose meaning changes gets a new ID and the old is `SUPERSEDED`.

## 3.7 `plan validate` refuses

Duplicate IDs · dangling `verified_by` references · verification IDs absent from `verification.yaml` · forward dependencies · scope globs matching no path · **any acceptance criterion not bound to at least one check**.

That last one matters most: an unbound criterion is the mechanism by which a task reaches `COMPLETE` on an agent's word. Escape hatch: mark it `manual: true`, which forces a review boundary.

**Four clarifications forced by implementing this list at S11.**

1. **"Forward dependencies" means declaration order, and it is kept.** It is a
   strictly stronger rule than cycle detection, and the two are not
   interchangeable: a plan can point forward and still be a perfectly executable
   DAG, so a cycle checker accepts a file this rule refuses. Keeping the literal
   reading is what makes acyclicity **structural** — a graph whose every edge
   points backwards cannot contain a cycle — and it settles a question the
   content hash would otherwise leave open: if declaration order is semantically
   constrained then reordering tasks *is* a semantic change, and invalidating an
   approval on reorder is correct rather than incidental. Cycle detection is
   retained as defence in depth and to catch the one case that is not a forward
   edge, a task depending on itself. **Every cycle necessarily contains a
   forward edge**, so the two defects co-occur; that is not double-counting.
2. **"Dangling `verified_by` references" and "verification IDs absent from
   `verification.yaml`" are one rule, not two.** A `verified_by` entry has
   exactly one possible referent — a check id — so the two phrases describe the
   same check from two directions. Implemented once.
3. **The check catalogue is an input to `validate`, not something it resolves.**
   §5.1 makes `verification_profile` a per-task path while this section names a
   single `verification.yaml`; until S11's persistence step settles that, the
   validator takes the catalogue as a parameter and the caller assembles it. A
   pure function that reached into the filesystem to resolve per-task profiles
   would be deciding the question rather than deferring it.
4. **A plan document cannot declare its own state.** `state:` is not a field of
   `plan.yaml`: §3.3 gives an agent write access to `.conductor/`, so a document
   that could say `APPROVED` would be self-approval. The state lives in the
   store, and writing `state: APPROVED` into a plan file is inert *and* changes
   the plan's content hash — which is what makes the tampering visible rather
   than merely ineffective.

---

# Part 4 — The Hard Parts

## 4.1 Workspace isolation

**Decision: per-run local clone with `--no-hardlinks`. Mandatory. No optimized mode.**

Rejected: **worktrees**, which share `.git/config`, refs, hooks and the object store — file isolation, not repository isolation. An agent running `git remote set-url` in a worktree mutates the user's real repository, and a `pre-commit` hook it writes fires later under the human's own hands.

Rejected: **default (hardlinked) clone**, refuted by M1/M2. A 16-byte in-place write from inside the clone took the source repository from `fsck exit=0` to `fatal: loose object … is corrupt`, `cat-file HEAD: FAILED`. Object files are mode `-r--r--r--`, but the same user owns them, so `chmod u+w` succeeds trivially. (Deletion in the clone is safe — `unlink` only decrements the link count. The danger is exclusively in-place mutation of a shared inode.)

The earlier assumption that safety would cost latency is **refuted by M4**: `--no-hardlinks` is 2.5× faster at 22 MB. Shipping a faster-but-unsafe mode that is in fact slower would be indefensible, so no such mode exists.

```bash
git clone --no-hardlinks --no-checkout "$SOURCE" "$WORKSPACE"
git -C "$WORKSPACE" checkout -b "conductor/$TASK_ID/$RUN_ID" "$BASE_COMMIT"
git -C "$WORKSPACE" remote remove origin          # nothing to push to
git -C "$WORKSPACE" config core.hooksPath /dev/null
git -C "$WORKSPACE" config user.name  "Conductor Agent"
git -C "$WORKSPACE" config user.email "conductor@localhost"
git -C "$WORKSPACE" config commit.gpgsign false
```

Residual shared `.git` state: **none.** Own config, refs, reflog, hooks path, object store, index.

**Baseline captured at run creation:** `base_commit`, tree hash, `git status --porcelain=v2`, untracked list, `git config --list --local`, `git remote -v`, all refs, submodule status, stash count, hooks directory listing, **nested-repository list** (the same paragraph below requires nested repos to be "detected at baseline", so the baseline must actually carry them — added at S2).

**Workspace descriptor** — `.conductor-run.json` in every workspace, containing `run_id`, `task_id`, `base_commit`, `policy_hash`, `created_at`. This is what makes recovery possible with **no database at all**.

**The descriptor must be hidden from git via `.git/info/exclude`**, not `.gitignore`. Found at S2 by a test: an agent running `git add -A` otherwise commits Conductor's own bookkeeping file onto the run branch — the branch Conductor later fetches into the user's real repository. A `.gitignore` would not do, because adding one is itself a tracked change appearing in the agent's diff.

**Branch model:** `conductor/<task-id>/<run-id>`, from `base_commit`, existing only in the run clone until Conductor fetches it. Never pushed. Never auto-merged into the default branch.

**Dirty user checkout:** proceed. The clone is from a commit, so uncommitted user work is neither copied nor endangered — strictly better than worktrees, which share the index and stash. Baseline records the dirty state so a later finding can distinguish "user had this modified" from "the agent did it."

**Target branch moves mid-run:** the run is unaffected (fixed commit). At integration, if the target ref moved, the run enters `AWAITING_REVIEW` with the divergence attached. **Conductor never rebases or merges automatically** — automatic conflict resolution is an unbounded correctness risk in a system whose thesis is that it does not guess.

**Submodules:** v1 **refuses**. Non-empty `git submodule status` at registration is a hard error. Nested repositories are detected at baseline, excluded from scope checks, and raise a finding if modified.

**Cleanup:** workspaces retained until the run is terminal and artifacts are captured, then `keep_workspaces_days` (default 7). Orphans found at startup are **quarantined**, never deleted — an orphan may hold the only copy of an hour of work.

**Revisit trigger:** if a registered repo's clone exceeds 10 s, revisit toward `--reference` + `--dissociate`, or a cached base clone refreshed by fetch. **Never toward hardlinks.**

## 4.2 Execution-security capability model

Conductor must know what enforcement an execution mode actually provides, and must never treat a weaker mode as equivalent to a stronger one.

```rust
enum Enforcement {
    Hard,        // kernel-enforced deny, verified by probe with a positive control
    Restricted,  // enforced, with a known and enumerated exception set
    AuditOnly,   // not prevented; reliably detected after the fact
    None,        // neither prevented nor detected
}

struct ExecutionCapabilities {
    filesystem_write:  Enforcement,   // gates
    network_egress:    Enforcement,   // gates
    control_surface:   Enforcement,   // gates
    credential_read:   Enforcement,   // gates
    tool_interception: Enforcement,   // INFORMATIONAL ONLY — never gates
    exceptions:        Vec<PathBuf>,  // populated for Restricted
}
```

**Deliberately cut from the model:**
- `repository_isolation` — after §4.1 it is a constant Conductor guarantees identically for every adapter. A dimension whose value never varies is an invariant, not a dimension.
- `tool_interception` **kept but non-gating.** Hooks have known bypasses (`sh -c`, script-then-execute, alternate spellings). Letting a policy *require* it would let a non-boundary satisfy a boundary requirement — the exact confusion this model exists to prevent.

**Values are measured, not declared.** `conductor doctor --containment` runs the probe suite against each configured (adapter × launcher) on the actual host, caching by `(adapter_version, launcher_version, os_version)`. **A stale or absent probe forces every dimension to `None` — fail closed.** This is the model's most important property: sandbox behaviour changes with OS and CLI versions, and a hardcoded table would silently become a lie after an upgrade.

**Measured classification:**

| | FakeAgent | **Codex** `--sandbox workspace-write` | **Claude Code** (bare) | Claude + `codex sandbox` launcher |
|---|---|---|---|---|
| `filesystem_write` | n/a | **Restricted** — `/tmp`, `$TMPDIR` (M6–M8) | **None** | **Restricted** — `/tmp`, `$TMPDIR` (M28) |
| `network_egress` | n/a | **Hard** (M9) | **None** | **Hard** (M28) |
| `control_surface` | n/a | **Hard** (M10, M11) | **None** | **Hard** (M28) |
| `credential_read` | n/a | **None** (M12) | **None** | **None** (M28) |
| `tool_interception` | n/a | not investigated | **Restricted** — measured out-of-band (M17–M19) | Restricted |

**On the fourth column (added at S2.5).** It was *unverified* until the probe harness measured it, and it comes out identical to Codex's — which is what M13 and ADR-0002 predict, since **containment is a property of the launcher, not the agent**. Two caveats must travel with this number or it will be over-read: it measures what the launcher does to an *arbitrary payload*, and it does **not** show that Claude Code actually functions under that launcher — a real Claude run needs network egress, which the same sandbox denies at `Hard`. Eligibility is about what is prevented; usability is a separate question, and "sandboxed Claude" (a Conductor-authored profile permitting only the model endpoint) remains post-v1.

**On `tool_interception` (added at S2.5).** The probe harness **cannot** measure it: a hook only fires inside a live agent session, which costs a model invocation. It is therefore measured **out-of-band** — by S0's live probe (ADR-0003) — and the probe harness reports it as *unmeasured*, never inferring a value. This is safe precisely because it never gates (§4.2's rule), so an unmeasured informational dimension cannot weaken any decision. Do not "fix" the harness by having it guess this row.

FakeAgent is Conductor's own code and not an adversary; recording it as `Hard` would be a category error.

**Containment is a property of the launcher, not the agent** (M13). Conductor is not forced to accept an adapter's native tier — it can supply one. That is the path to making Claude Code eligible for sensitive work later, and it is why the model is keyed on (adapter × launcher) rather than adapter alone.

**Eligibility check — the entire subsystem:**

```yaml
# .conductor/project.yaml, or per-task override
execution_requirements:
  filesystem_write: restricted
  control_surface:  hard
```

```
before launching an attempt:
    caps = probe_cache.get(adapter, launcher, host)      // None if stale/absent
    if any required dimension > caps dimension:
        refuse to launch unattended
        emit the dimension, the requirement, and the measured value
        offer: attended mode | different adapter | a sandbox launcher
```

~50 lines and one table. It does not rank adapters and does not choose between eligible options. Any growth beyond "compare a vector, refuse or proceed" is scope creep.

## 4.3 Approval architecture

**A `0600` unix socket does not distinguish a human from a same-user subprocess, and removing an environment variable is obscurity.** Approval integrity is a property of the execution mode.

| Tier | Mechanism | Integrity |
|---|---|---|
| **A. Sandboxed** | seatbelt AF_UNIX default-deny (M10); socket under `$HOME/.conductor/`, which the agent cannot write (M6), so it cannot squat or replace it | **Enforced.** Measured, with positive control (M11). |
| **B. Unsandboxed + operator nonce** | grant requires a nonce printed **only** to the controlling terminal; only `hash(nonce)` is persisted, so reading `conductor.db` does not yield it | **Raises cost substantially. Not a kernel boundary.** |
| **C. Unsandboxed, no nonce** | socket permissions and env scrubbing only | **Not a boundary. Approvals are advisory.** |

**Binding rule:** a task whose policy can produce an approval gate **may not run unattended** below tier A. Enforced by §4.2's eligibility check, not by documentation.

**Additional control, all tiers:** the binary reachable from a workspace, if any, exposes read-only verbs only and physically lacks the approval code path — asserted by a source-scan test that fails if anyone wires approval into it.

> **RESOLVED at S8 — "if any" is the accurate half; there is no shim in v1.** §4.3
> hedges ("the binary reachable from a workspace, *if any*") while the S8 slice's
> Verify line says "**the** shim" as though one exists. Nothing in v1 is a shim.
> What *is* reachable from a workspace is the pair of binaries Conductor runs **in
> the agent's position**: `conductor-fake-agent` (it is the agent; its cwd is the
> workspace) and `conductor-probe-action` (§4.2's payload, run under the launcher
> being measured). Those two are the census the source scan governs.
>
> Absence is proven three ways rather than one, because a substring scan alone is
> a rule someone renames their way past: a code-shaped needle scan; a rule that no
> workspace-facing binary may name `conductor_run` or `conductor_store` **at all**
> (§4.3's "read-only verbs only", with no subset to get wrong); and a manifest
> rule that `conductor-agent` must not depend on `conductor-run`. The census
> itself is guarded — an unclassified binary fails the suite, which is how the
> guard caught S8's own new test binary.

**Request and grant:**

```yaml
approval_request:
  id: AR-0031
  run_id: r-0041
  action: dependency.add.runtime
  facts: {dependency: serde_yaml, version: "0.9", manifest: Cargo.toml}
  facts_source: deterministic
  policy_hash: blake3:41ef…
  matched_rules: [global.runtime-dependency, project.no-unapproved-runtime-dependency]
  explanation: "Adds a runtime dependency not present at base commit."
  evidence: {diff: artifacts/r-0041/deps.patch, sha256: …}
  requested_at: 2026-08-12T14:03:00Z
  expires_at:   2026-08-13T14:03:00Z

approval_grant:
  id: AG-0019
  request_id: AR-0031
  binding_hash: blake3(action ‖ canonical(facts) ‖ policy_hash ‖ scope)
  scope: {run: r-0041}
  reuse: false
  expires_at: 2026-08-12T15:03:00Z
  granted_by: krish
  channel: unix-socket
  nonce_hash: blake3:…        # tier B only
```

**`binding_hash` is the scoping mechanism.** A grant authorizes an operation only if the recomputed hash matches at use time. So `dependency.add.runtime:foo` cannot authorize `…:bar` (different facts), cannot authorize `deployment.execute` (different action), and stops applying if the policy snapshot changed — which is correct, not inconvenient.

**Revocation (Scenario S):**

| State at revocation | Result |
|---|---|
| Not yet consumed | Effect never happens; run → `AWAITING_APPROVAL` |
| `INTENDED`, effect not started | Aborted before starting |
| `INTENDED`, effect in flight | **Cannot be cancelled.** Complete or fail it, record the receipt, halt with a finding |
| `CONFIRMED` | Cannot be undone. Record revocation, raise `POST_HOC_REVOCATION` finding |

**Four distinct approval kinds — never collapse into `approved: bool`:**

| Kind | Authorizes | Granularity | Expires |
|---|---|---|---|
| Plan approval | a plan version becoming authoritative | one plan version | no |
| Policy approval | one policy-gated action, once | one `binding_hash` | yes |
| Policy exception | temporarily loosening a rule | rule + scope, within the ceiling | yes (mandatory) |
| Review acceptance | that completed work is accepted | one review packet | no |

Collapsing them would let a plan approval satisfy a deployment gate.

## 4.4 Policy architecture

A policy engine decides **what Conductor will do**. It does not decide what an agent with a shell can do — §4.2 handles that.

**Effects form a total order.** The join is `max`:

```
allow  <  require_approval  <  deny
```

**Evaluation is two stages, not one.** A single "most restrictive wins" join makes locked rules peers with project rules, which means locking does no work in the one direction it exists for.

```
Stage 1 — CEILING (locked policy)
    Locked global rules produce a maximum permissiveness.
    Nothing below can exceed it — not project rules, not exceptions,
    not a human grant. Unlocking is a separate, audited operation.
    ← S6/S7: the ceiling's load-bearing work is on EXCEPTIONS and GRANTS.

Stage 2 — JOIN
    effect = max(builtin_invariant, global_default, project_rule, task_constraint)
    then, if a scoped exception matches exactly, is unexpired,
    and the Stage-1 ceiling permits it:
        effect = exception.effect
```

**Invariants:** a project can always tighten, never loosen past the ceiling · an exception can only lower an effect within the ceiling · **unknown action → `deny`** (fail closed; the taxonomy will be incomplete on day one and incompleteness must not read as permission) · built-in invariants are not configurable at all (never write outside the run workspace; never print a value matching a secret detector; never push to a remote; never operate on an unregistered repository).

> **CLARIFIED at S7 — "a project can always tighten, never loosen past the
> ceiling" is true but *vacuous as stated*.** Stage 2's join is `max`, so a
> project rule is structurally incapable of loosening anything: the ceiling is
> never what stops it. The construct that *lowers* an effect is the **exception**
> (and, from S8, a human grant), and bounding those is the ceiling's only
> load-bearing role. Stated plainly because a test written against the sentence as
> written would assert something that cannot fail — the exact vacuity S2, S5 and
> S6 each produced. S7's ceiling test therefore exercises the **exception** path,
> with an unlocked-rule positive control proving the lock is what stopped it.
>
> **Two of the four built-in invariants are not action-keyed.** "Never write
> outside the run workspace" and "never push to a remote" map onto typed actions;
> "never operate on an unregistered repository" and "never print a value matching
> a secret detector" are *conditions*, and are modelled as fact-conditioned
> invariants. Their real enforcement point is elsewhere (workspace creation, the
> secret scanner); they exist in policy so that no policy file can override them.

**Typed actions:**

```
git.commit.local · git.push · git.remote.modify · git.branch.delete · git.force_push
dependency.add.runtime · dependency.add.dev · dependency.remove · lockfile.modify
database.migration.create · database.migration.apply · database.destructive_change
filesystem.write.outside_workspace · network.external_access · credential.access
deployment.execute · release.publish · architecture.change
authentication.change · authorization.change · billing.spend · service.paid_addition
```

**Facts declare their derivation.** Every fact carries `source: deterministic | model_assisted | human`. A `require_approval` may rest on any; **a `deny` must rest only on `deterministic` facts** — meaning the facts that *rule* names in its `when:`, not every fact in the request (ADR-0010). A model must never be the sole reason Conductor blocks work — a hallucinated block is indistinguishable from a real one and trains the user to override blocks.

> **SCOPED at S7 (ADR-0010).** Read as "every fact present", the sentence is a
> **weakening vector**: one unrelated model-assisted observation anywhere in the
> request would downgrade an otherwise-deterministic `deny` to
> `require_approval`. That inverts the rule's purpose — it exists to stop a model
> *causing* a block, not to let a model *remove* one. The cap therefore applies
> only to the facts a rule's own `when:` clause depends on; a rule with no `when:`
> is a standing policy statement and still denies.

**Global policy** lives at `$XDG_CONFIG_HOME/conductor/policy.yaml`, falling back to
`~/.config/conductor/policy.yaml` — the convention `Store::default_path` already uses.
Project policy is `.conductor/policy.yaml` (§3.1). An absent file is not an error; a
present, malformed one is, and stops the run (fail closed).

**Expiry timestamps are RFC 3339, UTC only.** §4.3 writes `expires_at:
2026-08-13T14:03:00Z`, and §2.2's dependency list contains no date crate. S7 hand-writes a
UTC-only parser rather than adding one, and **rejects offsets** instead of guessing: an
expiry an hour out is an exception outliving its grant.

| Action | Deterministic fact source |
|---|---|
| `dependency.add.runtime` | diff of `[dependencies]` / `dependencies` in the manifest |
| `lockfile.modify` | path match on `Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`, `uv.lock` |
| `git.remote.modify` | `git config --get-regexp '^remote\.'` before vs after |
| `git.push` | new commits on a remote-tracking ref; reflog |
| `database.migration.create` | new file under configured migration globs |
| `filesystem.write.outside_workspace` | comparison against workspace root; sandbox denial events |
| `credential.access` | env allowlist violations; secret-pattern scan of the diff |
| `architecture.change` | **not deterministic.** Path globs are a proxy → `model_assisted` → `require_approval` at most, never `deny`, always with the diff attached |

**Snapshots.** At run creation Conductor canonically serializes the resolved policy (sorted keys, no timestamps), hashes it BLAKE3, stores it content-addressed, and pins `policy_hash` on the run. **A run evaluates against its snapshot for its entire life** — an approval granted under one policy must not authorize an action under a different one. **Exception:** if the new policy is *strictly more restrictive* for a pending action, the run pauses and asks. Never silently proceeds under a policy the human just tightened.

**`conductor policy explain <action>`** prints: action · resolved effect · the ceiling that applied · every rule that matched **and every rule considered that did not, with the reason** · facts and their sources · policy hash · any exception with scope and expiry. Negative results are what people debug.

## 4.5 Verification

**Verification is authoritative. The agent's report is not.**

```yaml
verification:
  toolchain_fingerprint:            # participates in the result cache key
    - "rustc --version"
    - "cargo --version"
  required:
    - id: typecheck
      command: cargo check --all-targets
      timeout_seconds: 600
      on_timeout: inconclusive      # NOT failure
    - id: unit-tests
      command: cargo test
      timeout_seconds: 1200
      flaky_retry: 1                # exactly one; disagreement ⇒ INCONCLUSIVE
  conditional:
    - when: {changed_paths: ["migrations/**"]}
      commands: [migration-validate]
  invariants:                        # cheap, always, never skipped
    - id: no-secrets
      command: conductor scan secrets
    - id: git-invariants
      command: conductor scan git-invariants
```

**Results are bound to a tree, not to a run:**

```
(tree_hash, check_id, command_hash, toolchain_fingerprint) → outcome
```

Consequences: free caching (re-verifying an unchanged tree is a lookup, which matters most in repair loops) · crash-safe (after a daemon crash, an unchanged tree with a valid result is not re-run) · and **a result whose tree moved under it is `VOID`, not `PASS`**. Conductor hashes the tree before and after each check; a mismatch voids the result and raises a finding. This closes a hole where an agent or a stray watcher modifying files mid-test produces a green result for a tree that never existed.

**Four outcomes:** `PASS` · `FAIL` · `INCONCLUSIVE` · `VOID`.

`INCONCLUSIVE` covers timeout, infrastructure error and flaky-retry disagreement. It is **not** a failure, because the distinction determines what happens next: `FAIL` → repair; `INCONCLUSIVE` → bounded infra retry, then human. Collapsing them is how a broken cache turns into three wasted agent attempts.

> **S5 implemented the second half of that sentence as unreachable; corrected at
> S6.** S5's `finish` routed any refusal *without* a `FAIL` straight to
> `AWAITING_REVIEW`, so an `INCONCLUSIVE` check went to a person with no retry at
> all and acceptance row 8's "infra retry ×1, **no budget spent**" could never
> happen. No test pinned the old behaviour, which is why it survived S5's suite.
>
> §5.2 gives `VERIFYING` exactly one non-human successor, so the bounded retry has
> to leave through `REPAIRING`. What makes that safe is that the bound is real:
> `decide()` counts infrastructure attempts against `max_infra_retries` and hands
> the run to a person after them, **without** touching the work budget §4.7
> protects. The routing predicate is now `retry_kind(report)` — `Work |
> Infrastructure → REPAIRING`, `None → AWAITING_REVIEW` — so the one place that
> classifies a report is the one place that routes it.

**Logs** go to `artifacts/<run>/verification/<check>-<attempt>.log`, **content hash (BLAKE3) recorded** (ADR-0007), never inlined into packets, **secret-scanned before any excerpt enters a packet.**

The log name is qualified in practice (`.retryN`, `.<tree12>`): §4.5 itself requires re-running a `VOID` check at the new tree, which is the *same* check and attempt at a *different* tree, so `<check>-<attempt>` alone is not unique. Logs are opened with `create_new`, so a surprise collision is an outcome rather than a silent overwrite.

**`tree_hash` means the working tree, not `HEAD^{tree}`** (S4). Agent edits are uncommitted by definition, so binding results to the committed tree would make every edit invisible and `VOID` undetectable. It must honour `.gitignore`, or any check that writes to `target/` voids itself.

**Completion criteria — a task may reach `COMPLETE` only when all hold:**
1. Every required check `PASS` **at the current tree hash**.
2. Every conditional check triggered by the actual diff has run and passed.
3. All invariant checks pass.
4. Zero unresolved findings **of blocking severity (`CRITICAL`)**.

   *(Amended at S5, which found this in direct contradiction with Part 9 row 5: a
   malformed report raises `REPORT_UNPARSEABLE` and row 5's expected outcome is
   `COMPLETE` + finding, Human? **no**. Read absolutely, criterion 4 blocks that
   forever, because findings never auto-resolve (§4.8) — so one cosmetic finding would
   permanently strand a task whose tests all pass. The severity-graded reading is also
   the one consistent with the product thesis: verification is authoritative and the
   agent's report is not, so a garbled report is evidence quality, not a correctness
   signal. It is recorded, not obeyed. `CRITICAL` is the severity S3 and S4 already
   use for halting cases.)*
5. Every acceptance criterion binds to ≥1 passing check.
6. Reconciliation verdict ∈ {`CLEAN_COMPLETE`, `CLEAN_NO_REPORT`} — **or `POLICY_SENSITIVE` where criterion 7 is satisfied**.

   *(Amended at S9, ADR-0013.* Read without the qualifier, criterion 7 is unreachable
   and therefore not a criterion at all. A policy-sensitive action is policy-sensitive
   because a sensitive path changed, and **approval does not un-modify the file** — the
   verdict is still `POLICY_SENSITIVE` after the human grants. So criterion 6 refuses
   first, every time, and every run criterion 7 could speak about is already rejected.
   It also makes rows 12 and 13 vacuous in the other direction: "resumes on grant"
   would resume a run that then refuses to complete no matter what the human said.
   The reading that makes both criteria load-bearing is that criterion 6 excludes the
   verdicts **nobody has resolved** — `CORRUPT`, `CONTRADICTED`, `OUT_OF_SCOPE`,
   `NO_CHANGE`, and `POLICY_SENSITIVE` *without* a grant — and that an authorised
   `POLICY_SENSITIVE` is resolved by criterion 7. Enforced in the type system: the
   evidence variant carries the authorising grant as a required field, so "authorised"
   cannot be claimed by a caller that has nothing to name.)*
7. Every policy-sensitive action detected has a matching, unexpired, correctly-scoped grant.

Note what is absent: **the agent's report.**

**Independent agent review is NOT a verification type in v1.** A nondeterministic check inside a subsystem whose entire value is determinism makes `COMPLETE` probabilistic. Post-v1 it may attach *advisory* findings to a review packet, where a human is already reading.

## 4.6 Bounded repair

```
fingerprint(failure) = blake3(
      sorted(failing_check_ids)
   ‖  normalized(first_failing_assertion)   # paths, line numbers, addresses, timings stripped
)

progressed(prev, next) :=
      NOT (prev.failing_checks ⊂ next.failing_checks)        # not a pure regression  ← S6
  AND (   next.failing_checks ⊂ prev.failing_checks          # strictly fewer
       OR (next.fingerprint ≠ prev.fingerprint AND tree changed) ) # different problem, real edit
```

> **AMENDED at S6 (ADR-0008). The leading clause is new; the two disjuncts are
> unchanged.** As originally written the predicate called a pure regression
> progress. The failing-check set is hashed into the fingerprint, so `{alpha} →
> {alpha, beta}` has a different fingerprint, and an agent that edits files
> changes the tree — both halves of the second disjunct hold. Everything that
> failed before still fails, one more fails now, and the loop was told to keep
> spending. `progressed()` is consulted directly by the loop-breaker (it is the
> only stop that fires when `stop_on_identical_fingerprint` is off), so this
> removed a stopping condition, in the silent direction §4.6's own normalizer
> doctrine names as the dangerous one.
>
> `⊂` is **proper** containment in both directions. The guard refuses only
> unambiguous regression: `{alpha} → {beta}` — alpha fixed, beta revealed — is not
> a superset and remains progress, which is the ordinary shape of real repair.

```yaml
repair:
  max_attempts: 2
  stop_on_identical_fingerprint: true
  escalate_after: 2                  # → AWAITING_REVIEW
  new_session_on_attempt: 2          # fresh context; the stuck one is stuck
  max_infra_retries: 1               # ← S6; row 8's "infra retry ×1"
```

> **`max_infra_retries` added at S6.** §4.7 bounds infrastructure retries in prose
> ("backoff, does **not** consume budget") without giving a number, and acceptance
> row 8 supplies one ("infra retry ×1"). A bound with no configured value is a
> bound somebody hardcodes at a call site. It also has to exist for the ceiling
> below to be finite: infrastructure attempts cost no *work* budget, so without a
> term of their own they are unbounded in the only count that matters — spawns.

**Three loop-breakers, because loops have three causes:**
1. **Identical fingerprint twice** → stop immediately; do not spend attempt 2.
2. **Oscillation** — fingerprint alternates A→B→A → stop. Detected by keeping the last 4.
3. **Empty edit** — a repair attempt producing `NO_CHANGE` → stop.

`new_session_on_attempt: 2` is deliberate: a stuck agent's context *is* the problem, and resuming re-imports the stuckness. The repair packet's `do_not_retry` list carries forward what matters.

**Acceptance property:** no configuration of the fake agent can produce more than

```
ceiling = 1 + max_attempts + max_infra_retries      # defaults: 1 + 2 + 1 = 4
```

agent invocations. Asserted by counting spawns.

> **AMENDED at S6 (ADR-0009), in two ways.**
>
> **(1) The number was wrong.** `max_attempts` counts *repairs*, so the initial
> attempt was never inside it, and §4.7 exempts infrastructure retries from the
> budget entirely. Taken literally the property was unsatisfiable by its own
> definitions. The ceiling above is a function of **configuration only** and never
> of agent behaviour, which is what the property was reaching for.
>
> **(2) The bound must be durable, or it is not a bound.** §4.6 as written implies
> an in-memory count. Acceptance rows 10 and 11 restart runs, and a count held in
> a process resets when that process dies — so crash-restart cycles produce
> unbounded invocations while every loop-breaker still reads as correct. The
> ceiling is therefore enforced from durable state (`attempt` rows, which §4.7's
> supervisor commits **before** `spawn()`) immediately before every spawn, in
> addition to — never instead of — the three loop-breakers. The breakers stop the
> loop early and say why; the ceiling is the backstop that holds when the history
> they read has been lost.

## 4.7 Durable runtime semantics

**There is no separate job entity.** A `run` in a claimable state *is* the job. Adding a `job` table over `run` is the first step toward a generic scheduler.

**Claim — one atomic statement, the named transaction boundary:**

```sql
BEGIN IMMEDIATE;
UPDATE run
   SET state='RUNNING', lease_owner=?1, lease_expires_at=?2,
       lease_epoch = lease_epoch + 1
 WHERE id = (SELECT id FROM run
              WHERE state IN ('READY','RECOVERING')
                AND (lease_expires_at IS NULL OR lease_expires_at < ?3)
              ORDER BY priority, created_at LIMIT 1)
RETURNING id, task_id, policy_hash, lease_epoch;
INSERT INTO event(run_id, seq, kind, payload) VALUES (…, 'RUN_CLAIMED', …);
COMMIT;
```

> **RESOLVED at S3** (contradiction surfaced at S1). `RECOVERING` was never a state:
> §5.2's task machine does not define it, and the run state mirrors its task, so half
> the claim's predicate could never match — a dead disjunct in a safety predicate reads
> as coverage without being it. The `RunState::Recovering` variant is **deleted** and
> the predicate is now `state IN ('READY','RECONCILING')`: a `RECONCILING` run whose
> lease has expired is exactly what a restarting worker must take, and the existing
> `lease_expires_at` clause already protects a live worker's run.
>
> The claim additionally **preserves** `RECONCILING` rather than overwriting it with
> `RUNNING`, because §5.2 has no `RECONCILING → RUNNING` edge:
> `SET state = CASE WHEN state='RECONCILING' THEN 'RECONCILING' ELSE 'RUNNING' END`.
>
> **Re-measured after the change** (ADR-0005's evidence is tied to this statement):
> 39,400 claims, **0 duplicates, 0 invariant failures**; worst-case claim latency
> 5147 ms → **4950 ms**. The 60 s lease keeps its ~12× margin; M26 stands.

**`lease_epoch` is the fencing token.** Every subsequent write by that worker carries its epoch and is rejected if the epoch moved. Without fencing, a process that stalls past its lease and then wakes will happily write over its successor's work.

**Lease duration is coupled to worst-case claim latency**, and the coupling is measured, not assumed: the 60 s lease must exceed the worst starvation window, which S1 measured at **5147 ms** under `rusqlite` (up from 1093 ms in S0) — a ~12× margin. Do not shrink the lease without re-deriving this (M26, ADR-0005).

**Leases:** 60 s. **Heartbeat:** every 15 s, **conditional on the agent process still existing** (`kill(pid, 0)`) — a supervisor that heartbeats while its child is dead is worse than one that crashes.

**Retries are attempts within a run**, bounded by `attempt_budget`, not queue redelivery. Two kinds, never conflated: **infrastructure retry** (spawn failed, adapter missing, auth expired) → backoff, does **not** consume budget; **work retry** (agent failed the criteria) → consumes budget, requires a repair packet. Conflating them is how a broken API key silently exhausts a task's budget.

**Timers: two only** — lease expiry and approval TTL, both `WHERE … < now()` on a 5-second tick. **No timer service.** A third timer need is a signal to look hard.

**Startup recovery:**

```
1. Open DB, migrate, integrity_check.
2. Find runs in RUNNING/RECONCILING/VERIFYING with expired leases.
3. Probe recorded pid → alive & start-time matches?
       alive → adopt or terminate (config); record.
       dead  → attempt := STALE.
4. Locate workspace. Absent → run BLOCKED + finding.
5. Capture current git state; diff against stored baseline.
6. Re-run verification only if the tree hash has no cached valid result.
7. Classify (§4.8) and route.
8. Scan for orphaned workspaces → QUARANTINE, never delete.
9. Expire overdue approvals; restore AWAITING_APPROVAL waits.
```

**Idempotency — intent → precondition → act → receipt:**

```
operation_id = blake3(kind ‖ run_id ‖ attempt_ordinal ‖ tree_hash)

BEGIN IMMEDIATE; INSERT side_effect(operation_id, kind, 'INTENDED', precondition); COMMIT;
    perform the effect                                    ← crash window
BEGIN IMMEDIATE; UPDATE side_effect SET state='CONFIRMED', receipt=?; COMMIT;
```

On restart, an `INTENDED` row is resolved by **re-checking the precondition against the world**, never by blind retry:

| Kind | Did it happen? |
|---|---|
| `git.commit.local` | does a commit with this tree and message exist on the run branch? |
| `git.fetch_into_main` | does the target ref point at the expected sha? |
| `workspace.create` | does the path exist with the expected `HEAD`? |
| `artifact.write` | does the file exist with the expected sha256? |

**Design constraint that follows:** *an effect Conductor cannot verify afterwards is an effect Conductor may not own.* This is what keeps deployment out of v1 automation by construction rather than by scope decision.

**Ambiguous** (precondition indeterminate) → mark `AMBIGUOUS`, halt the run, raise a finding, require a human. **Never guess.**

**The actual execution guarantee, in three tiers:**

1. **Agent attempts: at-least-once.** Safe because attempts are isolated in a per-run clone and reconciled, not assumed.
2. **Conductor-owned side effects: at-most-once, effectively exactly-once when the precondition is checkable.** Where indeterminate, the guarantee degrades explicitly to *halted and reported*, never to *retried*.
3. **Agent-caused external side effects: no guarantee whatsoever.** §4.2's credential and sandbox controls exist to make this class empty, not to manage it.

**Conductor must never print or document "exactly-once" without the tier-2 qualifier.**

## 4.8 Reconciliation

`reconcile()` is a pure function of (baseline, observed, report?, verification?, **scope**, **sensitive_patterns**) returning exactly one verdict. **Every exit from `RUNNING` passes through it** — success, crash, timeout, cancel. This is the invariant that makes agent self-report non-authoritative.

*(The last two parameters were added at S2. The original four-argument signature could not produce `OUT_OF_SCOPE` at all: nothing in a baseline or an observation declares what the scope is — that lives on `task.scope_globs` (§5.1) and, for sensitive paths, in the policy snapshot (§4.4). A verdict the signature cannot compute is not a verdict.)*

| Verdict | Meaning | Route |
|---|---|---|
| `NO_CHANGE` | tree identical to baseline | attempt failed to act → repair or review |
| `CLEAN_COMPLETE` | changes in scope, report present and consistent | → VERIFYING |
| `CLEAN_NO_REPORT` | changes in scope, no report | → VERIFYING (report is not required for correctness) |
| `OUT_OF_SCOPE` | changes outside declared scope | finding → AWAITING_REVIEW |
| `POLICY_SENSITIVE` | deps/lockfile/migrations/git-config touched | policy evaluation → approval or review |
| `CONTRADICTED` | report contradicts observed state | finding; **git wins** → AWAITING_REVIEW |
| `CORRUPT` | repo broken (merge in progress, detached, index lock) | → BLOCKED |

**Reconciled surface** (after every attempt, before any state advances): `git status --porcelain=v2 --branch` · staged and unstaged diffs · untracked files · new commits and parents · **`git config --list --local` diffed** · **`git remote -v` diffed** · **all refs diffed** · reflog · stash list · dependency manifests · lockfiles · migration globs · files outside scope · secret-pattern scan over the whole diff · hooks directory · submodule state · **`/tmp` delta during the attempt window**.

Any unexplained delta raises a `Finding`. **Findings never auto-resolve.**

**Verdict precedence** (unspecified originally; fixed at S2 because row 14 forces it). Exactly one verdict is returned, highest wins:

```
CORRUPT  >  CONTRADICTED  >  POLICY_SENSITIVE  >  OUT_OF_SCOPE
         >  CLEAN_COMPLETE / CLEAN_NO_REPORT   >  NO_CHANGE
```

`POLICY_SENSITIVE` must outrank `NO_CHANGE`: Part 9 row 14 sets a remote inside the clone, which leaves the *tree* identical to baseline while changing something that matters. A tree-first classifier would report `NO_CHANGE` and advance. `CORRUPT` outranks everything because a broken repository makes every other reading unreliable.

**Nested-repository modification raises a finding but does not by itself set a verdict.** Nested repos are excluded from scope checks, so forcing `POLICY_SENSITIVE` would conflate "outside the declared scope" with "policy-relevant"; the finding alone still reaches a human, because findings never auto-resolve.

## 4.9 Security model — what is prevented, what is only detected

| Layer | Prevents | Available today |
|---|---|---|
| 1. Prompt instructions | **nothing** — this is documentation, not a control | yes, worth ~0 |
| 2. Deterministic policy | Conductor's own actions | yes |
| 3. Conductor-owned effects | agent performing push/deploy/migrate *through Conductor* | yes |
| 4. Agent permission hooks | specific tool calls by pattern; **known bypasses** | Claude only (M17) |
| 5. OS sandbox | writes outside workspace; network; AF_UNIX | **Codex only** (M6–M11) |
| 6. **Credential absence** | **push, deploy, cloud API, DB access** | **yes, and cheapest** |
| 7. Network control | egress | Codex only (M9) |
| 8. Post-run audit | nothing | detects almost everything |

**Layer 6 is the primary control**, not the fourth. *An agent with no push credential cannot push, regardless of what it types, what it is told, or whether any hook fires.* Concretely, the agent subprocess is spawned with:

- An **allowlisted** environment (`PATH`, redirected `HOME`, `LANG`, `TERM`, the adapter's own auth variable, nothing else). Not a denylist — a denylist misses the next variable name.
- `GIT_TERMINAL_PROMPT=0`; `GIT_ASKPASS` → **a program Conductor writes itself**, into the per-run `HOME` at mode `0500`, which exits non-zero and prints nothing. *(Corrected at S9, ADR-0011: S5 pointed this at `/bin/false`, which **does not exist on macOS**. It failed safe only because `GIT_TERMINAL_PROMPT=0` caught the fallback — a named mechanism that was not present. Referencing a host path is what made that possible.)*
- **`GIT_CONFIG_NOSYSTEM=1`**, with `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` pinned to `/dev/null`. *(Added at S9, ADR-0011.* A **system** gitconfig is located by absolute path, so it survives both `env_clear` and a redirected `HOME`. On a macOS host with Xcode's git it declares `credential.helper=osxkeychain` — a credential source reachable by an agent that adds its own remote, defeating layer 6 without touching a single environment variable. No item in the list above closes it.*)*
- `SSH_AUTH_SOCK` unset; no `~/.netrc`; no `GH_TOKEN`/`GITHUB_TOKEN`; no cloud or database variables.
- A **per-run `HOME`**, so `~/.aws`, `~/.config/gh`, `~/.kube` are simply absent.
- A **per-run `TMPDIR` inside the workspace**, so ordinary temp usage stays contained despite M7.
- And the run clone has **no `origin`** — nothing to push to, before credentials are even discussed.

**v1 limitations, stated plainly (they belong in `SECURITY.md`, not a comment):**

1. Under a non-sandboxed launcher, Conductor cannot prevent writes anywhere the user can write. Detection only.
2. Under Codex, `/tmp` remains writable (M7) — secret staging is possible; mitigated by per-run `TMPDIR` and `/tmp` delta auditing, not prevented.
3. **Reads are not contained even under the sandbox** (M12). The agent can read `~/.codex/auth.json` and `~/.ssh`. Network denial (M9) means reading ≠ exfiltrating, but staging for later pickup is possible.
4. Conductor cannot distinguish a human at the socket from a process that reached it; §4.3 minimizes the paths and tier C is honest about the rest.
5. Conductor cannot undo an external side effect. It can refuse to be the one that performs it, and detect afterwards.
6. **Prompt injection from repository content is unmitigated.** Packets label repository-derived spans as untrusted; labelling is mitigation, not prevention.

---

# Part 5 — Data and State

## 5.1 Schema (v1)

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = FULL;      -- this DB is the recovery record
PRAGMA fullfsync    = 1;         -- REQUIRED on macOS: synchronous=FULL issues fsync(),
PRAGMA checkpoint_fullfsync = 1; -- which does not flush the drive write cache. F_FULLSYNC
                                 -- does. Costs median 0.065ms -> 2.733ms (M22, ADR-0004);
                                 -- irrelevant at Conductor's claim rate, and it is what
                                 -- the "recovery record" justification above actually requires.
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;      -- NOTE: busy_timeout absorbs contention into LATENCY,
                                 -- not into errors. busy_errors=0 is not evidence of low
                                 -- contention (ADR-0004). Budget p99, never median.

CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);

CREATE TABLE project (
  id             TEXT PRIMARY KEY,           -- p-<short>
  root_path      TEXT NOT NULL UNIQUE,
  repo_identity  TEXT NOT NULL,              -- blake3(first_commit ‖ normalized_origin)
  default_branch TEXT NOT NULL,
  config_hash    TEXT NOT NULL,
  created_at     INTEGER NOT NULL
);

-- index over .conductor/plans/vN/ ; git is authoritative
CREATE TABLE plan_version (
  id           TEXT PRIMARY KEY,
  project_id   TEXT NOT NULL REFERENCES project(id),
  version      INTEGER NOT NULL,
  content_hash TEXT NOT NULL,                -- of canonical semantic content
  state        TEXT NOT NULL,                -- DRAFT|VALIDATED|AWAITING_APPROVAL|APPROVED|SUPERSEDED
  approved_at  INTEGER, approved_by TEXT,
  source_path  TEXT NOT NULL,
  UNIQUE(project_id, version)
);

CREATE TABLE decision (
  id              TEXT PRIMARY KEY,          -- D-0007
  project_id      TEXT NOT NULL REFERENCES project(id),
  status          TEXT NOT NULL,             -- OPEN|ACCEPTED|REJECTED|SUPERSEDED
  supersedes      TEXT REFERENCES decision(id),
  content_hash    TEXT NOT NULL,
  source_path     TEXT NOT NULL
);

CREATE TABLE task (
  id                   TEXT PRIMARY KEY,     -- T-0012
  plan_version_id      TEXT NOT NULL REFERENCES plan_version(id),
  slice_id             TEXT NOT NULL,
  state                TEXT NOT NULL,
  scope_globs          TEXT NOT NULL,        -- json
  verification_profile TEXT NOT NULL,
  attempt_budget       INTEGER NOT NULL DEFAULT 3,
  created_at           INTEGER NOT NULL
);

CREATE TABLE run (
  id               TEXT PRIMARY KEY,         -- r-0041
  task_id          TEXT NOT NULL REFERENCES task(id),
  policy_hash      TEXT NOT NULL REFERENCES policy_snapshot(hash),
  workspace_id     TEXT REFERENCES workspace(id),
  base_commit      TEXT NOT NULL,
  run_branch       TEXT NOT NULL,
  state            TEXT NOT NULL,
  priority         INTEGER NOT NULL DEFAULT 100,
  lease_owner      TEXT,
  lease_expires_at INTEGER,
  lease_epoch      INTEGER NOT NULL DEFAULT 0,   -- fencing token
  created_at       INTEGER NOT NULL
);
CREATE UNIQUE INDEX ix_run_one_active_per_task ON run(task_id)
  WHERE state NOT IN ('COMPLETE','CANCELLED','SUPERSEDED');
CREATE INDEX ix_run_claim ON run(state, lease_expires_at);

CREATE TABLE attempt (
  id               TEXT PRIMARY KEY,
  run_id           TEXT NOT NULL REFERENCES run(id),
  ordinal          INTEGER NOT NULL,
  kind             TEXT NOT NULL,            -- IMPLEMENT|REPAIR|CONTINUE
  adapter          TEXT NOT NULL,
  launcher         TEXT NOT NULL,            -- none|codex-sandbox|sandbox-exec
  caps_snapshot    TEXT NOT NULL,            -- measured ExecutionCapabilities, json
  agent_session_id TEXT,
  pid              INTEGER, pid_start_time INTEGER,
  started_at       INTEGER, ended_at INTEGER,
  exit_code        INTEGER, signal INTEGER,
  outcome          TEXT,                     -- EXITED|CRASHED|TIMED_OUT|STALE|RECONCILED
  UNIQUE(run_id, ordinal)
);

CREATE TABLE workspace (
  id          TEXT PRIMARY KEY,
  run_id      TEXT NOT NULL REFERENCES run(id),
  path        TEXT NOT NULL UNIQUE,
  kind        TEXT NOT NULL DEFAULT 'CLONE_NO_HARDLINKS',
  source_repo TEXT NOT NULL,
  state       TEXT NOT NULL,                 -- ACTIVE|RETAINED|QUARANTINED|REMOVED
  created_at  INTEGER NOT NULL, removed_at INTEGER
);

-- append-only EVIDENCE log. NOT event sourcing; state is never replayed from it.
CREATE TABLE event (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id  TEXT REFERENCES run(id),
  seq     INTEGER NOT NULL,
  kind    TEXT NOT NULL,
  payload TEXT NOT NULL,
  at      INTEGER NOT NULL
);
-- UNIQUE, not plain: a duplicate claim must fail at INSERT time rather than be
-- recorded and noticed later by an offline checker (M23, ADR-0005).
CREATE UNIQUE INDEX ix_event_run ON event(run_id, seq);

CREATE TABLE verification_check (
  id                    TEXT PRIMARY KEY,
  run_id                TEXT NOT NULL REFERENCES run(id),
  attempt_id            TEXT REFERENCES attempt(id),
  tree_hash             TEXT NOT NULL,
  commit_sha            TEXT NOT NULL,
  toolchain_fingerprint TEXT NOT NULL,
  check_id              TEXT NOT NULL,
  command_hash          TEXT NOT NULL,
  exit_code             INTEGER,
  duration_ms           INTEGER,
  outcome               TEXT NOT NULL,       -- PASS|FAIL|INCONCLUSIVE|VOID
  log_path              TEXT
);
CREATE UNIQUE INDEX ix_verif_cache
  ON verification_check(tree_hash, check_id, command_hash, toolchain_fingerprint)
  WHERE outcome IN ('PASS','FAIL');

CREATE TABLE policy_snapshot (
  hash TEXT PRIMARY KEY, canonical_blob TEXT NOT NULL, created_at INTEGER NOT NULL
);

CREATE TABLE approval_request (
  id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES run(id),
  action TEXT NOT NULL, facts TEXT NOT NULL, facts_source TEXT NOT NULL,
  policy_hash TEXT NOT NULL, matched_rules TEXT NOT NULL, explanation TEXT NOT NULL,
  evidence_ref TEXT, state TEXT NOT NULL,     -- REQUESTED|GRANTED|DENIED|EXPIRED
  requested_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);

CREATE TABLE approval_grant (
  id TEXT PRIMARY KEY, request_id TEXT NOT NULL REFERENCES approval_request(id),
  binding_hash TEXT NOT NULL, scope TEXT NOT NULL, reuse INTEGER NOT NULL DEFAULT 0,
  state TEXT NOT NULL,                        -- GRANTED|CONSUMED|EXPIRED|REVOKED
  nonce_hash TEXT, channel TEXT NOT NULL,
  granted_by TEXT NOT NULL, granted_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);
CREATE INDEX ix_grant_binding ON approval_grant(binding_hash, state);

CREATE TABLE side_effect (
  operation_id TEXT PRIMARY KEY,              -- blake3(kind ‖ run ‖ ordinal ‖ tree_hash)
  run_id       TEXT NOT NULL REFERENCES run(id),
  kind         TEXT NOT NULL,
  state        TEXT NOT NULL,                 -- INTENDED|CONFIRMED|FAILED|AMBIGUOUS
  precondition TEXT NOT NULL, receipt TEXT,
  intended_at  INTEGER NOT NULL, resolved_at INTEGER
);

CREATE TABLE finding (
  id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES run(id),
  kind TEXT NOT NULL, severity TEXT NOT NULL, evidence_ref TEXT NOT NULL,
  resolution TEXT, created_at INTEGER NOT NULL   -- never auto-resolves
);

CREATE TABLE artifact (
  id TEXT PRIMARY KEY, run_id TEXT REFERENCES run(id),
  -- content_hash, not sha256: §2.2 authorises blake3 and no SHA-2, and every other
  -- hash in the design is BLAKE3. Renamed by schema v3 (ADR-0007).
  kind TEXT NOT NULL, path TEXT NOT NULL, content_hash TEXT NOT NULL, created_at INTEGER NOT NULL
);

CREATE TABLE containment_probe (
  id TEXT PRIMARY KEY, adapter TEXT NOT NULL, adapter_version TEXT NOT NULL,
  launcher TEXT NOT NULL, launcher_version TEXT NOT NULL, os_version TEXT NOT NULL,
  capabilities TEXT NOT NULL, probed_at INTEGER NOT NULL,
  UNIQUE(adapter, adapter_version, launcher, launcher_version, os_version)
);
```

**Entities deliberately absent:** `Milestone`/`Slice` as tables (structure lives in `plan.yaml`; status is **computed** from child tasks — storing an aggregate invites it to disagree with its children) · `Checkpoint` (agent-internal progress is unobservable and untrustworthy; Conductor-internal progress is already an `event`) · `Continuation` (an artifact) · `Failure` (covered by `attempt.outcome` + `finding` + verification) · `ReviewPacket` (an artifact) · `PolicyRule` as rows (rules live in YAML, hashed as a snapshot; normalizing creates a second editing surface) · `job` (a claimable `run` is the job).

**Every write uses `BEGIN IMMEDIATE`.** SQLite's deferred transactions take a read lock and upgrade, producing `SQLITE_BUSY` on upgrade that no `busy_timeout` resolves.

## 5.2 State machines

### Plan (5 states)

```
DRAFT ──validate──► VALIDATED ──request──► AWAITING_APPROVAL ──human──► APPROVED
  ▲                    │                          │                        │
  └────────────────────┴──────────────────────────┘                   SUPERSEDED
        (validation failure or rejection returns to DRAFT)      (by a later APPROVED)
```

Dropped from the baseline: `IMPORTED` (indistinguishable from `DRAFT`) and terminal `REJECTED` (a rejected plan is a `DRAFT` again; a terminal reject state is a graveyard nobody queries).

**Authority:** `APPROVED` only via a human at the control socket. **Evidence:** `content_hash` + validation report. **Invalid:** `DRAFT → APPROVED`; `APPROVED → *` except `SUPERSEDED`. **Restart:** re-hash on load; a mismatch on an `APPROVED` plan is a hard error, cleared by re-running `conductor plan approve <version>` on the changed document. *(Corrected at S11: §7.1's 13-command surface has no `plan reapprove`, so as written the hard-error state had no exit. `plan approve` is already human-only and socket-only, which is exactly the authority re-approval requires — a fourteenth command would add a second door to the same room.)*

### Task (12 states)

```
PENDING ──deps met──► READY ──claim+eligibility──► RUNNING
                        ▲                             │
                        │                             ▼
                        │                        RECONCILING ◄────────┐
                        │                             │               │
             ┌──────────┼──────────┬──────────────────┼───────┐       │
             ▼          │          ▼                  ▼       ▼       │
      AWAITING_APPROVAL │      VERIFYING          BLOCKED  AWAITING_  │
             │          │          │                       REVIEW     │
         (granted)      │    ┌─────┴─────┐                  │         │
             └──────────┘    ▼           ▼                  │         │
                        COMPLETE     REPAIRING ─────────────┼─────────┘
                             ▲            │                 │
                             └────────────┴─────────────────┘
                              accept / repair / revise / stop
                                            │
                                    CANCELLED  SUPERSEDED
```

Terminal: `COMPLETE`, `CANCELLED`, `SUPERSEDED`.

**Four corrections to the diagram, all forced by building the vertical at S5.** The
diagram above is kept as the shape; the legality table in `conductor-core/src/task.rs`
is the authority, and these are the differences:

1. **`REPAIRING → RECONCILING` cannot work and is replaced by `REPAIRING → READY`.**
   §4.7's claim *preserves* `RECONCILING`, so a run entering repair that way would be
   re-reconciled with **no agent ever running**. S3's report had already recorded
   `REPAIRING → READY` as the edge that actually functions.
2. **`RECONCILING → REPAIRING` is missing from the diagram** but is forced by §4.8's
   `NO_CHANGE` route. Added.
3. **`BLOCKED` is drawn with no outgoing edge yet is not terminal** — a trap that would
   strand every blocked run. Only `→ CANCELLED` / `→ SUPERSEDED` are permitted.
4. **`AWAITING_APPROVAL` has no exit for a denial.** Added `→ AWAITING_REVIEW`.
   **S8 owns revisiting this** when denial semantics are actually built.
5. **`READY → BLOCKED` is missing, and row 30 requires it** (found at S9, ADR-0012).
   The edge is drawn as `READY ──claim+eligibility──► RUNNING`: two gates named, only
   the both-pass outcome drawn. Row 30 says an ineligible task ends `BLOCKED` with the
   attempt never started, and no path from `READY` reached `BLOCKED` — nor did one from
   `RUNNING`. Added. `RUNNING → BLOCKED` was considered and **rejected**: the gate runs
   before the claim, which keeps §4.8's "every exit from `RUNNING` passes through
   reconciliation" literally true for a run that never launched an agent.
6. **`AWAITING_APPROVAL → RECONCILING` is missing, and `(granted)` pointing at `READY`
   is wrong** (found at S9, ADR-0012). `READY` re-runs the agent, and
   `ensure_workspace` re-captures the baseline from a workspace that already holds the
   approved change — so the next attempt reconciles as `NO_CHANGE` and **the approval
   authorises nothing**. This is the same failure as correction 1, from the other
   direction. `RECONCILING` is what works: the claim predicate already accepts and
   preserves it, and §4.7's recovery compares against the **stored baseline artifact**.
   `READY` is retained for denial and plan revision.

**Changed from the baseline's 14 states:** `COMMITTING` removed (a commit is a Conductor-owned effect inside the `RECONCILING → COMPLETE` transaction, protected by the side-effect ledger; a state meaning "we are mid-effect" is what the ledger replaces, and having both means two mechanisms for one problem). `FAILED` removed (everything routes to `AWAITING_REVIEW` or `BLOCKED`; a terminal `FAILED` invites abandoning tasks with no decision record). `ABANDONED` removed (that is `CANCELLED` with a reason). `SUPERSEDED` added.

**`RECONCILING` is mandatory and unskippable** — enforced in the type system, not by convention.

**Authority:** Conductor drives all transitions except `AWAITING_APPROVAL →` (human), `AWAITING_REVIEW →` (human), `→ CANCELLED` (human).
**Evidence required:** `RUNNING → RECONCILING` requires a terminal attempt. `VERIFYING → COMPLETE` requires §4.5's seven criteria.
**Invalid:** `RUNNING → COMPLETE`; any `→ COMPLETE` without verification bound to the final tree hash.
**Restart:** anything in `RUNNING`/`RECONCILING`/`VERIFYING` with an expired lease is forced to `RECONCILING`.

### Attempt (8 states)

```
CREATED → STARTING → ACTIVE ─┬─► EXITED     ─┐
                             ├─► CRASHED    ─┤
                             ├─► TIMED_OUT  ─┼─► RECONCILED (terminal)
                             └─► STALE      ─┘
```

`STALE` ≠ `CRASHED`. `CRASHED` means we observed a nonzero exit; `STALE` means **we do not know**, and unknown must not be recorded as known. Every path ends at `RECONCILED` — an attempt is never finished until Conductor has looked at the repository.

> **RESOLVED at S3** (surfaced at S1). `attempt` had **no `state` column** — only
> `outcome` — so `CREATED`, `STARTING` and `ACTIVE` were unpersistable and a supervisor
> could not record that an attempt was in flight, which is exactly what startup recovery
> must read. **Schema v2** (forward migration; v1 untouched) adds
> `attempt.state TEXT NOT NULL DEFAULT 'CREATED'` plus a partial index on the in-flight
> set. The default is deliberately `CREATED`: a pre-existing row then reads as *in
> flight*, so recovery goes and looks at the world rather than assuming the attempt
> finished — failing in the safe direction.

### Approval (5 states)

```
REQUESTED ─grant─► GRANTED ─consume─► CONSUMED (terminal)
    │                 │
    ├─deny─► DENIED   ├─ttl─► EXPIRED
    └─ttl──► EXPIRED  └─human─► REVOKED
```

### Review (3 states)

```
PENDING ─export─► EXPORTED ─import─► DECIDED
          {accept · repair · revise_plan · pause · stop}
```

`revise_plan` creates a new `plan_version` in `DRAFT` and supersedes affected tasks.

### Run

Deliberately thin — mirrors its task, exists to hold the lease and policy snapshot. **An independent run state machine was considered and rejected:** two state machines over one lifecycle is two things to keep in agreement, and one will drift.

---

# Part 6 — Adapters and Packets

## 6.1 Adapter interface

Conductor owns spawning, killing, timeouts and streaming. Adapters are **pure translation** — build argv+env, parse lines, classify exits. This makes every adapter testable against recorded JSONL fixtures with no process at all, which is what you want because agent output is the least stable thing in the system.

```rust
trait AgentAdapter {
    fn id(&self) -> &str;
    fn capabilities(&self) -> FunctionalCapabilities;
    fn command(&self, input: &StartInput) -> Result<AgentCommand>;   // does NOT spawn
    fn parse_event(&self, line: &str) -> Result<Option<AgentEvent>>;
    fn extract_report(&self, out: &RunOutputs) -> Result<Option<AgentReport>>;
    fn classify_exit(&self, code: Option<i32>, sig: Option<i32>) -> AttemptOutcome;
    fn resume_command(&self, input: &ResumeInput) -> Option<AgentCommand>;
}

struct FunctionalCapabilities {          // security capabilities live in
    conductor_assigned_session_id: bool, // ExecutionCapabilities (§4.2) —
    session_resume:                bool, // conflating them hides the
    schema_enforced_final_output:  bool, // distinction that matters
    streaming_events:              bool,
    hermetic_config:               bool,
    spend_cap:                     bool,
}
```

**Dropped from the baseline interface:** `streamEvents`, `inspect`, `interrupt`, `terminate`, `resume` as session-object methods. Both real adapters are process launchers writing JSONL to stdout; the interface should say that.

**Conductor drives agents as subprocesses, never in-process SDK calls.** A subprocess can be `SIGKILL`ed, inspected, resource-limited, launched with a scrubbed environment, and — decisively — **survives the supervisor's own death**. Conductor's core promise is recovering from its own crash; an agent embedded in Conductor's process dies with Conductor every time.

## 6.2 Codex adapter (first real adapter)

Verified against `codex exec --help`, codex-cli 0.142.0:

| Need | Flag |
|---|---|
| Non-interactive | `codex exec [PROMPT]` (or stdin) |
| Event stream | `--json` — JSONL: `thread.started`, `turn.*`, `item.*`, `error` |
| Structured report | `--output-schema <FILE>` + `-o/--output-last-message <FILE>` |
| Resume | `codex exec resume <SESSION_ID>` / `--last` |
| **OS sandbox** | `-s/--sandbox read-only\|workspace-write\|danger-full-access` |
| Working root | cwd **and** `-C/--cd <DIR>` *(corrected at S10)* |
| Hermeticity | `--ignore-user-config`, `--ignore-rules` — **not `--ephemeral`** *(corrected at S10)* |

**Three things S10 measured against codex-cli 0.142.0 that the table above did not say.**

1. **`--output-schema` shapes *every* `agent_message`, not only the final one.**
   The recorded run emits five, and the first four claim `PARTIAL` while the last
   claims `COMPLETE`. An adapter that takes the first schema-shaped message as
   "the report" reads a mid-run status as the outcome. The report is
   `-o/--output-last-message`'s file, with the **last** `agent_message` as the
   fallback.
2. **`files_touched` carries absolute paths.** §4.8 reconciles in
   workspace-relative paths, so an adapter that does not normalise makes every
   successful run disagree with its own observation and reconcile as
   `CONTRADICTED` — a false "the agent lied" on the happy path. A path genuinely
   outside the workspace is left alone, because that is evidence.
3. **`--ephemeral` is incompatible with `resume`**, which the same slice scopes:
   it discards the session `resume` needs. Dropped; `--ignore-user-config` and
   `--ignore-rules` carry hermeticity.

**And one that is not about flags:** Codex blocks indefinitely reading `stdin`
when `stdin` is not a TTY. Conductor is immune because `supervise::spawn` uses
`Stdio::null()`, which was chosen for unrelated reasons — so the immunity is
load-bearing and accidental, and is now recorded as a requirement rather than a
coincidence.

**Chosen first, for one dominant reason:** on a machine with no container runtime, `--sandbox workspace-write` is the only mechanism that actually prevents writes outside the workspace, denies network, and denies the control socket (M6, M9, M10). Building the first adapter against the agent that offers real containment means the enforcement layer is exercised from the beginning rather than stubbed. Its `--output-schema` also makes the report contract enforced by the agent runtime rather than validated afterward.

Session identity arrives in `thread.started`, so it cannot be pre-assigned; a crash before the first line leaves an unknown session. That costs the resume *optimization*, not correctness — the run clone is the evidence.

## 6.3 Claude Code adapter (second)

Verified against `claude --help`, v2.1.228:

| Need | Flag |
|---|---|
| Non-interactive | `-p` / `--print` |
| Event stream | `--output-format stream-json` (+ `--include-partial-messages`) |
| **Hook audit trail** | `--include-hook-events` |
| Structured report | `--output-format json --json-schema <schema>` → `structured_output` |
| **Session identity** | `--session-id <uuid>` — **Conductor assigns it before spawn** |
| Resume | `--resume <id>`, `--continue`, `--fork-session` |
| Tool restriction | `--allowedTools`, `--disallowedTools` |
| Permission baseline | `--permission-mode dontAsk` (never `bypassPermissions`) |
| Hermeticity | `--setting-sources`, `--settings <file>` |
| Spend cap | `--max-budget-usd <amount>` → `billing.spend` policy |

**Not available:** `--permission-prompt-tool` (M16), OS sandbox. **Do not use `--bare` for real runs** — it skips hook loading and would disable interception; it is for the fake-agent harness only.

**Hook integration** (measured — ADR-0003). Conductor writes a per-run settings JSON containing a `PreToolUse` hook invoking `conductor hook`, which evaluates the run's policy snapshot and returns allow/deny. Three requirements, each from measurement:

- **`matcher: "Bash"` with no `if` filter.** The `if` field is a *pre-filter*: if its pattern does not match, the hook never runs and Conductor never sees the command. Using it makes the audit channel exactly as leaky as the pattern. Conductor takes every Bash call and classifies it itself.
- **`--setting-sources project`** is mandatory. `--settings` alone is not hermetic — ambient user-level hooks still fire (M20).
- **`--include-hook-events`** with `--output-format stream-json` emits paired `system/hook_started` and `system/hook_response` events. This is the audit trail, and it is the main reason to install hooks at all.

Measured: **100% fire rate** on Bash, with the raw command string delivered intact including `sh -c '…'` and `cd . && …` (M18). A normalized classifier catches direct, wrapper, chain, absolute-path and git-global-flag forms with **zero false positives**. **Known bypasses, confirmed end-to-end rather than theorized** (M19): `$(echo git) push` and `g=git; $g push` both passed the hook and reached `git`. The whole surviving class is the one requiring shell evaluation to resolve — command substitution, backticks, variable indirection, pipe-to-shell, write-then-execute, alias, encoded. **Hooks are an audit channel and a speed bump, never a boundary**, which is why `tool_interception` can never satisfy an `execution_requirements` clause (§4.2).

**Ships at its measured tier** (`None` across all four gating dimensions), therefore eligible only for tasks that pass §4.2's gate at that tier. `execution_requirements` is never weakened to accommodate it. "Sandboxed Claude" — a Conductor-authored `sandbox-exec` profile permitting only the Anthropic API endpoint — is separate, later, and measured.

## 6.4 Failure classification

| Signal | Outcome |
|---|---|
| exit 0 + report present + parses | `EXITED` |
| exit 0, no report | `EXITED` (report optional; reconciliation is authoritative) |
| exit ≠ 0, or `SIGKILL`/`SIGSEGV` | `CRASHED` |
| wall-clock budget exceeded | `TIMED_OUT` (Conductor kills: `SIGTERM`, grace, `SIGKILL`) |
| no output for `idle_timeout` | `TIMED_OUT`, `reason=stall` |
| process gone, no exit observed | `STALE` |
| auth/rate-limit error event | `CRASHED`, `kind=infrastructure` → infra retry, no budget consumed |

## 6.5 Packets

**Every packet is generated from durable state, content-hashed, and stored as an artifact.** No packet is assembled from conversation history. A packet that cannot be regenerated from the store plus the repository is a bug.

**Implementation packet** (target: <4 KB):

```yaml
packet: implementation
packet_version: 1
run_id: r-0041
task_id: T-0012
plan_version: 3
plan_hash: blake3:9ac2…
policy_hash: blake3:41ef…

objective: "…"                     # from plan.yaml
context: {milestone: M-02, slice: S-05, why_now: "…"}

acceptance_criteria:               # each MUST bind to a check
  - {id: AC-1, statement: "…", verified_by: [typecheck, unit-tests]}

scope:
  allowed_globs:   ["src/policy/**", "tests/policy/**"]
  forbidden_globs: [".conductor/**", "migrations/**"]

decisions:                         # ONLY those touching scope or explicitly referenced
  accepted: [{id: D-0007, statement: "…"}]
  rejected: [{id: D-0004, statement: "…", reason: "…"}]

repository:
  base_commit: 3f2a1c…
  branch: conductor/T-0012/r-0041
  workspace: /Users/…/workspaces/r-0041

verification:
  commands: [{id: typecheck, command: "cargo check --all-targets"}]

boundaries:
  requires_approval: [dependency.add.runtime, database.migration.create]
  forbidden:         [git.push, git.remote.modify, deployment.execute]

evidence_links:                    # linked, never embedded
  - {kind: prior_diff, path: artifacts/r-0039/diff.patch, sha256: …}

report_schema: schemas/agent-report.v1.json
```

**Context minimization:** decisions selected by touching the task's scope globs or explicit refs — never "all accepted decisions." Prior diffs linked by path and hash. Verification logs never embedded; failing *excerpts* only (§6.6).

**Agent report** (schema-enforced via `--output-schema` / `--json-schema`):

```json
{
  "task_id": "T-0012",
  "status": "complete | partial | blocked",
  "summary": "≤500 chars",
  "files_changed": ["src/policy/eval.rs"],
  "commands_run": [{"command": "cargo test", "exit_code": 0}],
  "acceptance_criteria": [{"id":"AC-1","claim":"met|not_met|unknown","evidence":"…"}],
  "deviations": [{"from":"…","reason":"…"}],
  "blockers": [],
  "unverified_claims": []
}
```

**The report is evidence, never authority.** `status: "complete"` with `reconcile() == NO_CHANGE` is `CONTRADICTED` and a finding. The schema exists to make contradiction machine-detectable, not to be believed.

**Repair packet** adds only: failing check IDs · the failure fingerprint · a bounded log excerpt (first failing assertion + 40 lines, never the full log) · the diff of what the previous attempt changed · attempt ordinal and remaining budget · an explicit `do_not_retry` list of approaches already tried. That last field is what stops attempt 2 from being attempt 1 again.

**Continuation packet** = implementation packet **plus observed reality**: `reconciliation_verdict`, current tree hash vs base, the actual diff so far, which criteria already verify green at the current tree, commits in the run clone, the partial report if any, and explicitly:

> *"The previous agent's reasoning is not available. Treat its intent as inferable only from the diff."*

**Review packet:** plan version and hash · task IDs · base commit and end state · the actual diff (linked, stat inline) · agent claims vs reconciliation verdict, side by side · every verification command with exit code and duration · policy evaluations and explanations · approvals granted with scope · deviations · unresolved findings · proposed next state.

**Review decision** (imported): `accept | repair | revise_plan | pause | stop`, plus `decisions_to_record[]`, `plan_amendments[]`, `notes`. Importing is a **mutating** operation and goes through the control socket, never a file an agent could write.

## 6.6 Determinism requirement

Packets and policy snapshots must serialize **byte-identically** for identical state. Not a stylistic preference: `binding_hash` and `policy_hash` are worthless if serialization is nondeterministic. Sorted keys, LF, no timestamps inside hashed content.

---

# Part 7 — CLI

## 7.1 The v1 surface — 13 commands

```
conductor init                       # scaffold .conductor/ in the current repo
conductor doctor [--containment]     # env, adapters, store, git, socket, capability probe

conductor plan validate [--version N]
conductor plan approve <version>     # human-only, socket-only

conductor task list [--state …]
conductor task run <task-id>         # claim → execute → verify → report   ← the core verb
conductor task show <task-id>        # state, attempts, verification, findings, diff

conductor status                     # everything currently live
conductor approve <request-id> [--scope …] [--ttl …] [--reuse]
conductor deny <request-id> [--reason …]

conductor review export [--since …]
conductor review import <file>

conductor policy explain <action>    # why was this denied — the 2 a.m. command
conductor recover [--scan]
```

`--json` on every command.

**Cut from the baseline's ~25, with reasons:** `daemon start/stop` (auto-started on demand; `doctor` reports it) · `project add/list/inspect` (folded into `init`/`status` — the project is the repo you are in) · `plan import` (a plan is a file you write; `validate` reads it) · `plan history` (`git log .conductor/plans/`) · `run start/inspect/cancel/resume` (a run is an implementation detail of a task) · `policy show` (`cat .conductor/policy.yaml`) · `verification show` (part of `task show`) · `artifacts` (paths are in `task show --json`).

## 7.2 Exit codes

Derived from Conductor's own outcome classes, using `sysexits.h` for the standard slots:

```
0   success
1   generic failure
2   no project / not initialized / store unhealthy
3   action required — approval or review pending    ← scriptable "human needed"
4   policy denied
5   verification failed
64  usage error        (EX_USAGE)
70  internal error     (EX_SOFTWARE)
```

Code `3` earns a dedicated slot because "Conductor stopped and needs a human" is the most common non-success outcome and must be distinguishable from failure by a wrapper script.

## 7.3 API

Unix socket at `$HOME/.conductor/conductor.sock`, mode `0600`, line-delimited JSON-RPC. **No TCP port in v1** — a loopback port is reachable by any process on the machine, including the agent; a socket in `$HOME/.conductor/` is not writable by a sandboxed agent (M6) and not connectable (M10). Hand-rolled JSON-RPC framing; no async RPC framework.

## 7.4 Dashboard — after S16

Read-only first. Then one absolute rule: **every dashboard mutation maps to an existing, tested CLI/API operation; the dashboard never gains a capability the CLI lacks.** And approval must remain possible from the CLI forever — if it ever becomes dashboard-only, the system stops working over SSH and §4.3's boundary gets rebuilt in a browser.

---

# Part 8 — Implementation Manual

Each slice is one focused unit of work. Hard gates are marked. Every slice ends with a stop point.

**Global rules for every slice:** never touch `.conductor/**` from agent-facing code paths · no `deny_unknown_fields` on anything an agent produced · every write transaction is `BEGIN IMMEDIATE` · no new dependency without a line in the slice's ADR.

---

### S0 — Measurement and pre-registered falsification  ✅ **COMPLETE 2026-08-12**

**Outcome.** Both questions answered. ADR-0001 … ADR-0004 written. Master plan amended
in four places (Part 0 M18–M22, Part 5.1 pragmas, §2.6 trigger 3, §6.3 hook integration).
Report: `docs/reports/S0-completion-report.md`.

**Objective.** Answer the two remaining empirical questions; record all findings (including M1–M17 from prior passes) as ADRs with falsification triggers.
**Why now.** Two decisions still rest on unmeasured claims. The prior pass found two experiments whose first design was wrong and produced *false permissive* results — that is the failure mode this slice exists to catch.
**Dependencies.** None.
**Scope.** **Q1:** does a Claude `PreToolUse` hook injected via `--settings` reliably deny `Bash(git push*)`, and what is the cheapest bypass? **Q2:** SQLite `BEGIN IMMEDIATE` claim latency at 1/4/16 concurrent writers. Write up M1–M17 as `ADR-0001…`.
**Out of scope.** All product code. (Worktree config-sharing is no longer decision-relevant — clones won on safety *and* speed — but must be recorded as an unverified claim rather than kept as fact.)
**Files.** `docs/decisions/ADR-00*.md`, `scripts/measure/`.
**Tests.** None — this slice produces evidence.
**Failure injection.** n/a.
**Verify.** Two ADRs each contain a measured number, a decision, and a pre-registered falsification trigger.
**Stop point.** Before any `src/`.
**Risk.** Cheapest slice; most likely to be skipped.

---

### S1 — Store foundation  ✅ **COMPLETE 2026-08-12**

**Outcome.** Workspace + `conductor-core`/`-store`/`-cli`; schema v1 verbatim from
Part 5.1 (one amendment: `ix_event_run` is now UNIQUE); forward-only migrations;
`BEGIN IMMEDIATE` helper; the §4.7 claim; `conductor doctor` green. 52 tests.
ADR-0005 records the mandatory `rusqlite` re-measurement: **39,400 claims, 0
duplicates**, and the finding that p99 tracks offered load, which re-scoped §2.6
trigger 3. Two state-machine contradictions surfaced and were routed to S3 (see §4.7,
§5.2). Report: `docs/reports/S1-completion-report.md`.
**Not proven:** power-loss durability. `SIGKILL` ×100 proves crash atomicity only.

**Objective.** Schema v1, migrations, transaction helpers, `conductor doctor`.
**Dependencies.** S0/Q2.
**Scope.** Part 5.1 DDL; forward-only migrations with a version table; `BEGIN IMMEDIATE` helper; `doctor` reporting store, git, adapters, socket dir.
**Out of scope.** Policy, verification, approvals, packets, agents.
**Must not touch.** Nothing exists yet.
**Files.** `crates/conductor-store/src/{lib,schema,migrate,tx}.rs`, `crates/conductor-cli/src/doctor.rs`.
**Tests.** Migration idempotency · `integrity_check` after each migration · concurrent-writer contention · `synchronous=FULL` survives simulated power loss.
**Failure injection.** `SIGKILL` mid-transaction ×100.
**Verify.** `cargo test -p conductor-store` · 100 kill-restart cycles, zero corruption, zero partial rows.
**Stop point.** `conductor doctor` green.

---

### S2 — Workspace isolation  ✅ **COMPLETE 2026-08-12**

**Outcome.** `conductor-git` crate: §4.1 clone sequence, descriptor, baseline, pure
`reconcile()` with all seven verdicts, orphan quarantine, submodule refusal. 88 new
tests (140 total). Isolation proven **and proven non-vacuous twice** — a negative
control that asserts a default clone *is* damaged, and a mutation test removing
`--no-hardlinks`, independently reproduced. ADR-0006 records that the plan's own
prescribed recipe was vacuous in two ways. Report: `docs/reports/S2-completion-report.md`.
**Deferred by design:** `/tmp` delta and secret scanning (S9), policy evaluation (S7) —
left as documented seams, deliberately not stubbed.

**Objective.** Create, describe, reconcile and quarantine per-run workspaces.
**Why now.** The load-bearing safety mechanism; everything downstream assumes it.
**Dependencies.** S1. Strategy already settled by M1–M5.
**Scope.** `--no-hardlinks --no-checkout` clone at base commit · remove `origin` · set identity, `core.hooksPath=/dev/null` · write `.conductor-run.json` · baseline capture (§4.1) · `reconcile()` producing all seven §4.8 verdicts · orphan quarantine · fixture repos (clean, dirty, detached, with-submodule, with-nested-repo, large).
**Out of scope.** Agents, verification, policy.
**Must not touch.** The source repository, ever, except by `git fetch` from the clone.
**Files.** `crates/conductor-git/src/{clone,baseline,reconcile,quarantine}.rs`, `tests/fixtures/`.
**Tests.** Every verdict from a hand-constructed git state · submodule repo refused at registration · dirty source proceeds and stays untouched.
**Failure injection.** Delete workspace mid-run · corrupt `.git` · leave `index.lock` · move the source repo.
**Verify — the acceptance test that matters:** an adversarial script inside the clone that sets a remote, deletes a branch, writes a hook, **mutates object files in place, runs `gc`, and mutates again**, leaves the source repository byte-identical — asserted by hashing `.git/config`, `git show-ref` output, and the **output** (never the exit status) of `git cat-file --batch-all-objects --batch-check`, plus `git fsck`. This test is known non-vacuous: the default (hardlinked) clone **fails** it (M2).

**Two ways this recipe was vacuous as originally written, both measured at S2 (ADR-0006):** (a) mutating only *after* `gc` proves nothing, because `gc` unlinks the shared objects and unlink merely decrements a link count — the mutation must happen **before** `gc` while an inode is still shared; (b) `cat-file --batch-all-objects --batch-check` **exits 0 even on a corrupt object store**, printing `missing`, so an assertion on its exit status would be silently vacuous. Both non-vacuity guards — a negative control that asserts a default clone *is* damaged, and a mutation test that removes `--no-hardlinks` — are permanent parts of the suite.
**Stop point.** Isolation proven.

---

### S2.5 — Containment probe harness  ✅ **COMPLETE 2026-08-12**

**Outcome.** `conductor-core::containment` (the §4.2 model) + `conductor-run/containment/`
(probe suite, version-triple cache, fail-closed) + `doctor --containment`. 58 new tests
(198 total). Measured table reproduces §4.2 exactly for Codex; the previously *unverified*
Claude+launcher column is now measured (M28). **Every denial-based probe has a positive
control**, and `CaseReport` cannot emit `Denied` without one — a blocked operation with a
failed control becomes `Broken`. `tool_interception` is structurally unable to gate
(no `PartialOrd`, no accessor to `Enforcement`, absent from `GatingDimension`; three
`compile_fail` doctests plus a non-vacuity control doctest).
Report: `docs/reports/S2.5-completion-report.md`.
**Not measurable here:** `tool_interception` (needs a live model session — measured
out-of-band by ADR-0003).

**Objective.** Measure `ExecutionCapabilities` for each (adapter × launcher) on the actual host; cache and fail closed.
**Why now.** §4.2's eligibility gate is meaningless without measured input, and sandbox behaviour changes under CLI upgrades. **Must precede the first real adapter.**
**Dependencies.** S1.
**Scope.** Probe suite (filesystem write inside/outside, reads, AF_UNIX connect with positive control, network egress, child-process inheritance) · `containment_probe` cache keyed by version triple · staleness → all dimensions `None` · `conductor doctor --containment`.
**Out of scope.** The eligibility check itself (S7), adapters (S10).
**Files.** `crates/conductor-run/src/containment/{probe,cache}.rs`.
**Tests.** Probe results match the M6–M12 matrix on this host · a stale cache yields `None` across the board · **the AF_UNIX probe includes its positive control and fails if the control does not connect** (a probe that cannot distinguish "denied" from "test broken" is worse than no probe).
**Failure injection.** Adapter binary absent · adapter version bumped → cache invalidated.
**Verify.** `conductor doctor --containment --json` reproduces Part 4.2's table.
**Stop point.** Capabilities are measured, never declared.

---

### S3 — Fake agent, supervision, crash recovery *(merged)*  ✅ **COMPLETE 2026-08-13**

**Outcome.** `conductor-agent` (AgentAdapter + scenario-driven FakeAgent), supervisor
(std threads, **no `tokio`**), leases + `lease_epoch` fencing + liveness-conditional
heartbeat, persisted attempt machine (schema v2), startup recovery, side-effect ledger,
unique artifact-path ownership. 143 new tests (**341 total**, 3m24s). Both S1-surfaced
contradictions resolved (§4.7, §5.2 above). `RUNNING → COMPLETE` is now **unrepresentable**:
`leave_running()` requires a `TerminalAttempt` token and returns one destination, and
`ReconciledRoute` has no `Complete` variant at all until S4 supplies verification.
Report: `docs/reports/S3-completion-report.md`.

**Two findings that change how later slices must test:**
- **The 12 prescribed Conductor kill points were insufficient.** There was no point
  between `git clone` returning and the workspace being recorded, and that gap held a
  real bug that **stranded a run permanently**. Verified by experiment that the matrix
  as written did *not* catch it. A 13th point was added. **`assert_converged` is
  necessary but not sufficient** — carry this to S6.
- **A test can race its own timer.** The agent-kill matrix asserted `CRASHED` under a
  tight idle budget; under load the idle timer fired first and the supervisor correctly
  recorded `TIMED_OUT` (§6.4 makes a timeout outrank the signal used to enforce it). The
  product was right and the test was wrong. Budgets are now separated by what the test
  is actually asserting.

**Objective.** Spawn/supervise/classify a subprocess; recover every failure mode from evidence alone.
**Why now.** Conductor's spine. Real agents add nothing testable that a fake one does not.
**Dependencies.** S1, S2.
**Scope.** Process supervisor (spawn, JSONL read, idle + wall timeouts, `SIGTERM`→grace→`SIGKILL`) · leases with `lease_epoch` fencing · heartbeat conditional on child liveness · attempt state machine · startup recovery (§4.7) · fake agent driven by a scenario file.
**Fake-agent scenarios.** success · success-with-report · partial edits · crash before edits · crash after edits · stall · timeout · malformed report · missing report · verification failure · same failure repeatedly · unexpected dependency change · forbidden git change · duplicate attempt · **attempts to connect to the control socket**.
**Out of scope.** Verification, policy, repair, real adapters, daemon.
**Files.** `crates/conductor-run/src/{supervise,recovery}.rs`, `crates/conductor-agent/src/fake.rs`.
**Tests.** One per scenario, asserting persisted state, attempt outcome, reconciliation verdict.
**Failure injection.** `SIGKILL` the agent at 8 points · `SIGKILL` **Conductor** at 12 points · stall · malformed JSONL · duplicate spawn.
**Verify.** For every (scenario × kill-point) pair, restart converges to the correct state **with no human input** and loses no work. Fencing test: a stalled worker waking after lease expiry **cannot write**.
**Stop point.** The whole matrix green.
**Risk.** Highest-complexity slice; if it slips, everything slips — which is why it is third, not eighth.

---

### S4 — Verification runner  ✅ **COMPLETE 2026-08-13**

**Outcome.** `conductor-run/verify/` (profile, classify, runner, toolchain, secrets),
working-tree hashing in `conductor-git`, the content-addressed cache in
`conductor-store`, and the completion gate in `conductor-core`. 116 new tests
(**457 total**, 2m18s). The `VOID` invariant is proven non-vacuous: removing the
after-check comparison makes the row-26 test fail with `left: Pass, right: Void`.
`ReconciledRoute::Complete` now exists but carries a `VerifiedComplete` token whose
only constructor is the gate. Report: `docs/reports/S4-completion-report.md`.

**Enforced now: 5 of §4.5's 7 criteria.** Acceptance bindings (S11) and policy grants
(S7) are deferred — but *not* stubbed true: each has an evidence enum with a single
`NotEvaluated { owner }` variant, so when S7 or S11 adds a satisfied variant the
gate's exhaustive `match` **stops compiling**. A later slice cannot silently forget
to wire its criterion in.

**Two defects found by S4's own failure injection**, both the same shape as bugs found
in S3: a killed worker permanently blocked its successor from logging (fixed — the
lease already excludes two live workers), and an overrunning check **leaked its
grandchildren** (fixed with `setpgid` + process-group kill). The second is a
correctness issue, not just a delay: a killed `cargo test` would leave compilers
writing *inside the workspace* after the after-check hash. **`supervise.rs` has the
same latent gap for agents — S5 must close it.**

**Objective.** Run checks, bind results to tree hashes, classify four outcomes.
**Dependencies.** S2, S3.
**Scope.** Profile loading · required/conditional/invariant checks · timeouts → `INCONCLUSIVE` · tree-hash binding and `VOID` detection · content-addressed cache · log capture with secret scanning · toolchain fingerprint.
**Out of scope.** Repair, policy.
**Files.** `crates/conductor-run/src/verify/{profile,runner,cache,classify}.rs`.
**Tests.** pass/fail/timeout/infra-error · cache hit on identical tree · **`VOID` when the tree changes mid-check** (fixture writes a file from a background process while a check runs) · conditional triggering by changed paths.
**Failure injection.** Kill mid-check · fill the disk · remove the toolchain between runs.
**Verify.** `cargo test -p conductor-run verify::` — no check result is ever attributed to a tree it did not observe.
**Stop point.** Cache correctness proven.

---

### S5 — First vertical: task → agent → reconcile → verify → commit  ✅ **COMPLETE 2026-08-13**

**Outcome.** The spine runs: `PENDING → COMPLETE` with a real commit carrying real
trailers, `main` untouched. Task legality table, side-effect ledger for
`git.commit.local` and `git.fetch_into_main` with **3-valued** preconditions, six
integration kill points, `conductor task run|show|list`. 78 new tests (**536 total**,
3m21s). Schema v4 adds `run.target_branch`. **Rust falsification trigger evaluated: does
not fire** (§2.2). Report: `docs/reports/S5-completion-report.md`.

**Trailers populated at S5:** `Conductor-Run`, `Conductor-Policy`,
`Conductor-Verification` (recomputable from the artifacts). **`Conductor-Plan` (S11) and
`Conductor-Approval` (S8) are absent, not invented** — §3.4 exists so the trail survives
total local state loss, and a reader recovering from nothing cannot tell a fabricated
hash from a real one, so one made-up value poisons all five. A test asserts both the
presence and the absence.

**The S4 carry-forward is closed, and the gap was worse than S4 thought.** A grandchild
holding the agent's stdout pipe open didn't merely delay things — the reader threads
never saw EOF, `finish()` blocked in `join()`, and **the run could never be reconciled**.
Closed with `setpgid` + a group sweep in `finish()`, on every return path including a
clean exit.

**Objective.** One fake-agent task from `PENDING` to `COMPLETE`, end to end, with a real commit.
**Why now.** Proves the spine before policy, approvals or packets add surface.
**Dependencies.** S1–S4.
**Scope.** Task state machine · minimal task-spec file (not yet the plan ledger) · Conductor-owned commit via the side-effect ledger · git trailers (§3.4) · fetch of the run branch into the source repo · `conductor task run/show/list`.
**Out of scope.** Policy, approvals, repair, real agents, plan versioning.
**Files.** `crates/conductor-core/src/task.rs`, `crates/conductor-run/src/effects.rs`, `crates/conductor-cli/src/task.rs`.
**Tests.** Happy path · `RUNNING→COMPLETE` rejected without reconciliation · false-success report → `CONTRADICTED`.
**Failure injection.** Kill **between** intent and effect, and between effect and confirm, for both commit and fetch.
**Verify.** Kill at any of 6 points during commit/fetch → restart produces **exactly one** commit and one ref update, asserted by counting.
**Stop point.** The spine works.

---

### S6 — Bounded repair  ✅ **DONE**

**Outcome.** Loops are provably bounded, and the bound survives losing the state it
reasons from. `ceiling = 1 + max_attempts + max_infra_retries` (default 4) enforced from
durable `attempt` rows before every spawn, plus §4.6's three loop-breakers for the early,
informative stops. Schema v5 adds `repair_observation` so `RepairHistory` is rebuilt from
the database each pass rather than carried in a process. 58 new tests (**594 total**).
Four master-plan corrections (ADR-0008, ADR-0009, §4.5's unreachable infra retry, row 9's
off-by-one). Report: `docs/reports/S6-completion-report.md`.

**The slice's own predicate called a regression progress (ADR-0008).** Because the
failing-check set is hashed into the fingerprint, `{alpha} → {alpha, beta}` satisfied
"different fingerprint AND tree changed" — nothing fixed, something newly broken, and the
loop told to keep spending. It mattered because `decide()` consults `progressed()`
directly and it is the *only* stop that fires when `stop_on_identical_fingerprint` is off.

**The bigger correction is that the bound was not durable (ADR-0009).** §4.6 implies an
in-memory count, but rows 10 and 11 restart runs by design, and the supervisor commits the
`attempt` row *before* `spawn()` while the verification result lands *after*. A kill in
that window leaves an invocation on the record and no memory of what it produced, so
`decide()` says `Repair` forever. Every loop-breaker was individually correct and the
system could still spend without limit — the same shape of error as S2's vacuous isolation
test and S5's row 22.

**Verified by mutation, reproduced independently of the implementing agent.** Ceiling
disabled → the loop reached ordinal 12 against a ceiling of 4. Breaker 1 disabled → 2
tests fail. The durable write made a no-op → 7 tests fail. **Stated honestly:** because
`ceiling ≥ decide()`'s bound in-process by construction, the ceiling's non-vacuity rests
on exactly one test — the crash-window one. That is by design, not oversight, but it means
deleting that test would silently untest the backstop.

**Objective.** Failed verification → bounded repair with real loop detection.
**Dependencies.** S4, S5.
**Scope.** Failure fingerprinting · `progressed()` · three loop-breakers · repair packet · budget accounting · new session on attempt 2 · escalation to `AWAITING_REVIEW`.
**Files.** `crates/conductor-run/src/repair/`.
**Tests.** Identical fingerprint → stop at attempt 1 · oscillation → stop · empty edit → stop · genuine progress → continue · budget exhaustion → review.
**Failure injection.** Fake agents that always fail identically, that oscillate, that change nothing.
**Verify.** **No configuration of the fake agent can produce more than `max_attempts` agent invocations**, asserted by counting spawns.
**Stop point.** Loops provably bounded.

---

### S7 — Policy engine  ✅ **DONE**

**Outcome.** Two-stage evaluation with a locked ceiling that provably bounds exceptions,
22 typed actions with `unknown → deny` made structural, BLAKE3 snapshots pinned for a run's
lifetime, deterministic fact extractors, `conductor policy explain` naming non-matching
rules and why, and §4.2's eligibility gate as a pure function. **1024 precedence cells, all
asserted**, against hand-written literal join/meet tables so a wrong operator cannot agree
with itself. 86 new tests (**680 total**). Report: `docs/reports/S7-completion-report.md`.

**`unknown → deny` is structural, not conventional.** `Action::parse` is *infallible* — it
returns `Action`, never `Option`/`Result` — so there is no `.unwrap_or(Allow)` for a caller
to write. An unrecognised name becomes `Action::Unknown(String)` whose `floor()` is `Deny`,
and the floor participates in both the join and the exception clamp, so an exception cannot
grant an unnamed action either.

**`tool_interception` cannot gate, by type.** `ExecutionRequirements` is keyed by
`GatingDimension`, which has no such variant; `tool_interception` is an `Informational` with
no ordering and no accessor returning `Enforcement`. Two `compile_fail` doctests pin both.
The YAML loader's rejection of the name is a courtesy, not the mechanism.

**One stated invariant was vacuous as written** — see the §4.4 clarification: Stage 2's
`max` join makes a project rule structurally incapable of loosening, so the ceiling is never
what stops it. The ceiling's real work is on exceptions and grants, and S7's test exercises
that path with an unlocked positive control.

**A vacuity was caught in flight.** The first run-pinning mutation left the suite green:
the fixture seeded one run and one snapshot, so "newest snapshot" and "this run's snapshot"
were the same row. Reseeded with a second run pinned to the tightened policy; the mutation
then fails.

**Objective.** Typed actions, two-stage evaluation, snapshots, explain, eligibility gate.
**Why now.** Sensitive actions now exist (S5 commits, S6 retries), and the spine has revealed which facts are needed.
**Dependencies.** S5, S2.5.
**Scope.** Global + project YAML · ceiling/join evaluation (§4.4) · deterministic fact extractors · snapshot + BLAKE3 hash · run-lifetime pinning · `conductor policy explain` · `unknown → deny` · **`execution_requirements` eligibility check consuming S2.5's measured capabilities**.
**Out of scope.** Approvals (S8), enforcement (S9), model-assisted facts.
**Files.** `crates/conductor-run/src/policy/{model,load,evaluate,facts,explain,eligibility}.rs`.
**Tests.** Precedence matrix as a table test · project cannot loosen past a locked ceiling · unknown action denies · snapshot pinning across a mid-run edit · explain names non-matching rules and why · eligibility refuses when measured caps are below requirements · **stale probe → refuse**.
**Failure injection.** Malformed policy · conflicting rules · policy deleted mid-run (snapshot must still resolve).
**Verify.** Every cell of the precedence matrix asserted; policy hash **byte-identical across two serializations** of the same policy.
**Stop point.** Evaluation explainable.

---

### S8 — Approvals  ✅ **DONE**

**Outcome.** The four kinds kept structurally distinct, `binding_hash` recomputed at use
time, TTL and one-shot enforcement, revocation with a defined outcome in each of four
states, a `0600` control socket published without a permissive window, and the §4.3 source
scan. 38 new tests (**766 total**). Report: `docs/reports/S8-completion-report.md`.

**The socket is published by `rename(2)`, not by `chmod(2)`.** Bind-then-chmod leaves a
window in which the published name exists at the umask's mode. Instead the listener binds
at a private per-pid staging name inside a directory `mkdir(2)` itself creates `0700`, is
chmod'ed `0600` while nothing can name it, then atomically renamed onto `conductor.sock`.
Stale-vs-live is decided by **attempting to connect**, never by guessing: a refused connect
means the name is free (a crash must not make approvals permanently unreachable);
a successful one returns `AlreadyServing` rather than stealing the name from a live server.
Deletion while running is detected by `(dev, ino)` identity, so replacement reads the same
as removal.

**Ships at tier C on this host, and says so.** `serve` prints §4.3's integrity sentence
verbatim, including "**Not a boundary. Approvals are advisory.**" It never claims tier A
and structurally cannot: tier A is a *measured* `control_surface: Hard` acted on by §4.2's
eligibility check, not something this code can assert about itself.

**Two vacuous tests were found by mutation, one of them minutes old.** Mutating the socket
mode killed no unit test, because three assertions compared the observed mode against the
constant they were testing — the code agreeing with itself. They now assert §7.3's literal
`0o600`/`0o700`. Separately, an audit mutation making `authorize` refuse everything left
`the_binding_is_recomputed_at_use_time_and_not_read_back_from_the_row` **passing**: it
asserted only a refusal, which a function that never authorizes anything satisfies
trivially. A positive control was added at review and proven to fail under that mutation.

**Objective.** Durable, exactly-scoped, expiring approvals over a socket the agent cannot reach.
**Dependencies.** S7.
**Scope.** Unix socket at `$HOME/.conductor/`, mode `0600` · request/grant lifecycle · `binding_hash` · TTL · one-shot vs reuse · revocation semantics (§4.3) · persistence across restart · four distinct approval kinds · **operator-nonce mechanism, default off**, activated when `control_surface < Hard`.
**Out of scope.** Dashboard, notifications.
**Files.** `crates/conductor-run/src/approval/`, `crates/conductor-cli/src/socket.rs`.
**Tests.** Grant for action A does not satisfy B · grant for `dep:foo` does not satisfy `dep:bar` · expiry · restart during a wait restores it · revocation in each of the four states · **a source-scan test asserting no approval-granting code path is reachable from any workspace-facing binary**.
**Failure injection.** Kill during an approval wait · kill between grant and consume · socket file deleted.
**Verify.** Approval state survives 50 kill-restart cycles · no grant consumed twice · the layering test **fails** if someone wires approval into the shim.
**Stop point.** Approvals durable and bounded.

---

### S9 — Enforcement and post-run audit  ⛔ **HARD GATE before S10**

**Objective.** Make the environment, not the prompt, the boundary.
**Why now.** No real agent runs before this exists.
**Dependencies.** S2, S7.
**Scope.** Env allowlist · per-run `HOME` · **per-run `TMPDIR` inside the workspace** · `GIT_ASKPASS` that fails · `SSH_AUTH_SOCK` unset · secret scanner · full post-run audit surface (§4.8) incl. **`/tmp` delta scanning** · findings that never auto-resolve · `SECURITY.md` populated with **measured** values.
**Files.** `crates/conductor-run/src/enforce/{env,audit,launch,policy_gate}.rs`, `SECURITY.md`.

*(**`enforce/secrets.rs` was not created**, and `launch.rs`/`policy_gate.rs` were not
foreseen. S4 already built the scanner at `verify/secrets.rs`, with its detection
rules, its redaction and — most importantly — its published `NOT_DETECTED` list; a
second scanner would be a second answer to "is this text safe to show", and the two
would drift. `audit` calls the existing one. The two extra files are the call sites
this slice turned out to be about: `launch.rs` is §4.2's "before launching an
attempt", `policy_gate.rs` is §4.8's "policy evaluation → approval or review". S9's
work was never to re-decide anything — S7 and S8 had decided it — it was to make
those decisions **reachable**, and a call site is where that lives.)*
**Tests.** A fake agent attempting push, remote mutation, config edit, hook install, secret exfiltration, out-of-scope writes — each detected, each a finding · env allowlist asserted by dumping the child's environment and diffing against expected.
**Verify.** Every §4.9 sensitive operation is either **prevented** (mechanism named) or **detected** (evidence named), classified per item in `SECURITY.md`. **No item may be listed as prevented without a passing test.**
**Stop point.** The honesty table is complete and true.

---

### S10 — First real adapter: Codex

**Objective.** Replace the fake agent with `codex exec` behind the same interface.
**Dependencies.** S3, S2.5, S9.
**Scope.** Codex adapter · JSONL event mapping · `--output-schema` report · `--sandbox workspace-write` · `--ignore-user-config`/`--ignore-rules` · `resume` · exit classification.
**Out of scope.** Claude adapter, hooks.
**Files.** `crates/conductor-agent/src/codex.rs`, `tests/fixtures/codex-jsonl/`.
**Tests.** Adapter parses recorded JSONL fixtures with **no process spawned** · malformed lines tolerated · schema-invalid report → `CONTRADICTED`.
**Failure injection.** Kill Codex mid-run · auth failure · truncated JSONL · schema violation.
**Verify.** **The entire S3 crash matrix passes with Codex substituted for the fake agent.** If any scenario needs adapter-specific handling, that is a design smell to fix in the interface, not the adapter.
**Stop point.** One real slice of real work completes on a fixture repo.
**Risk.** First nondeterminism. Keep the fake agent as the primary CI harness **forever**; real-agent tests are a separate, non-blocking suite.

---

### S11 — Plan ledger and decisions

**Objective.** Repo-tracked, versioned, immutable plans; append-only decisions.
**Dependencies.** S5.
**Scope.** `.conductor/plans/vN/` · `plan validate` (§3.7) · `plan approve` (socket-only) · content hashing over canonical semantics · task materialization · supersession · decisions with `ACCEPTED|REJECTED|SUPERSEDED` · **`.conductor/**` rejection rule (§3.3)**.
**Files.** `crates/conductor-run/src/plan/`, `crates/conductor-run/src/decision/`.
**Tests.** Approved plan immutable (edit → hard error) · revision creates a version and supersedes tasks · in-flight runs keep their old plan version · **a change to `.conductor/**` arriving on a run branch is rejected with a finding** · reformatting does not invalidate approval; a field change does.
**Verify.** **Delete `conductor.db`, rebuild: no plan, decision, policy or verification definition is lost.**
**Stop point.** Project truth outlives execution state.

---

### S12 — Packets and reports

**Objective.** Generate every packet from durable state.
**Dependencies.** S11, S10.
**Scope.** Implementation, repair, continuation packets · report schema · context minimization · evidence linking · determinism.
**Tests.** **The same state produces a byte-identical packet twice** · packet size budget enforced · continuation packet regenerable after total process restart.
**Verify.** An agent handed **only** a continuation packet completes a task interrupted mid-way, on a fixture, **with no session resume**.
**Stop point.** Recovery does not depend on hidden state.

---

### S13 — Review bridge

**Objective.** The human review loop, mechanized.
**Dependencies.** S12.
**Scope.** `review export` / `review import` · the five decision outcomes · review cadence config · boundary detection (milestone end, repeated failure, policy violation, ambiguous recovery, plan deviation).
**Tests.** Each outcome routes correctly · `revise_plan` creates a version and supersedes · import is socket-only.
**Verify.** A full loop — export → hand-edited decision → import → resume — with no manual state editing.

---

### S14 — Daemon, concurrency, multi-project

**Objective.** Survive terminal close; run 2+ projects concurrently.
**Why now.** Deferred until recovery semantics are proven (§2.4).
**Dependencies.** S3 (recovery, leases, fencing, startup reconciliation, side-effect recovery) all green.
**Scope.** `conductor daemon` · auto-start on demand · per-project locking · concurrent-run limits · stale-socket handling.
**Verify.** Close the terminal → the run continues; `conductor status` from a new shell reports it. Two projects concurrently with no cross-contamination. Reboot with live workspaces → state reconstructed.

---

### S15 — Claude Code adapter

**Scope.** Adapter · Conductor-generated settings with `PreToolUse` → `conductor hook` · `--include-hook-events` audit trail · `--session-id` pre-assignment · `--max-budget-usd` · `--setting-sources` hermeticity.
**Tests.** Hook denies a forbidden Bash pattern (S0/Q1 evidence) · denial appears in the event stream · **documented bypasses asserted as known-failing tests**, so nobody later mistakes them for coverage.
**Verify.** S3 crash matrix passes with Claude; `SECURITY.md` updated with Claude-specific "detect only" rows. **Ships at its measured tier; `execution_requirements` is not weakened.**

---

### S16 — Dogfooding

Conductor manages one real Conductor slice, with review at every task, `git.push` forbidden, and a human accepting every result.

---

### Post-v1 (not part of v1, not prerequisites for anything above)

Sandboxed-Claude research (a Conductor-authored `sandbox-exec` profile permitting only the model API endpoint) · optional third-party repository-evidence providers behind a `RepositoryEvidenceProvider` trait written only when a second provider exists · dashboard (read-only first; no dashboard-only mutation) · additional adapters.

---

# Part 9 — Acceptance Suite

Every row is a test. "Retry?" = an automatic agent attempt. "Human?" = execution halts for a person.

| # | Scenario | Injected | Expected persisted state | Automatic behaviour | Retry? | Human? | Final |
|---|---|---|---|---|---|---|---|
| 1 | Success | — | `EXITED`, `CLEAN_COMPLETE`, all `PASS` | commit, fetch, advance | no | no | `COMPLETE` |
| 2 | Crash before edits | kill at t=1s | `CRASHED`, `NO_CHANGE` | new attempt, same packet | yes | no | `COMPLETE` |
| 3 | Crash after edits | kill after writes | `CRASHED`, `CLEAN_NO_REPORT` | verify current tree; continuation packet | yes | no | `COMPLETE` |
| 4 | Missing report | exit 0, no report | `EXITED`, `CLEAN_NO_REPORT` | verification decides | no | no | `COMPLETE` |
| 5 | Malformed report | invalid JSON | finding `REPORT_UNPARSEABLE` | verification decides; finding stays | no | no | `COMPLETE` + finding |
| 6 | False success | "complete", tree unchanged | `CONTRADICTED` | halt | no | **yes** | `AWAITING_REVIEW` |
| 7 | Verification failure | test fails | `FAIL` | repair packet | yes ≤2 | if unfixed | `COMPLETE` / review |
| 8 | Verification timeout | hang > timeout | `INCONCLUSIVE` | infra retry ×1, **no budget spent** | infra only | after 2 | `AWAITING_REVIEW` |
| 9 | Repeated identical failure | same fingerprint | the **next** attempt not started — *see note* | stop at once | no | **yes** | `AWAITING_REVIEW` |
| 10 | Daemon crash mid-run | kill Conductor | lease expires | adopt or reconcile on restart | no | no | resumes |
| 11 | Reboot with live workspaces | reboot | leases expired, workspaces on disk | scan descriptors, reconcile each | no | no | resumes |
| 12 | Crash during approval wait | kill in `AWAITING_APPROVAL` | request `REQUESTED` | wait restored, TTL preserved | no | yes (as before) | resumes on grant |
| 13 | Dependency policy violation | agent adds a dep | `POLICY_SENSITIVE`, request created | halt | no | **yes** | `AWAITING_APPROVAL` |
| 14 | Git remote mutation | `set-url` in the clone | config diff vs baseline | **contained** — source unaffected; finding | no | **yes** | `AWAITING_REVIEW` |
| 15 | **Object-store corruption attempt** | in-place write to a `.git` object | clone damaged, **source `fsck` exit 0** | finding; workspace quarantined | yes | **yes** | `AWAITING_REVIEW` |
| 16 | Target branch moved | user commits to `main` | divergence at integration | no rebase, no merge | no | **yes** | `AWAITING_REVIEW` |
| 17 | Dirty user repo | uncommitted changes at start | recorded in baseline | proceed (clone from commit) | no | no | `COMPLETE`, user tree untouched |
| 18 | Abandoned workspace | orphan on disk | orphan detected | **quarantine, never delete** | no | no | reported in `status` |
| 19 | Evidence provider absent | no third-party binary on `PATH` | n/a | proceed unchanged | no | no | `COMPLETE` |
| 20 | Concurrent projects | 2 runs, 2 repos | independent rows and clones | no cross-contamination | no | no | both `COMPLETE` |
| 21 | Plan revision mid-flight | approve v4 during a v3 run | run keeps `plan_version=3` | finish under v3; new tasks under v4 | no | at review | `COMPLETE` under v3 |
| 22 | Duplicate side effect | kill between effect and confirm | `side_effect` `INTENDED` | re-check precondition; do not re-run | no | only if ambiguous | **exactly one commit** — *see note* |
| 23 | Policy change during run | edit policy mid-run | run keeps `policy_hash` | old snapshot; **pause if strictly tighter** | no | if tighter | `COMPLETE` / `AWAITING_APPROVAL` |
| 24 | Verification passes, policy violated | tests green + forbidden change | `POLICY_SENSITIVE` | **policy wins over green tests** | no | **yes** | `AWAITING_APPROVAL` |
| 25 | Approval revoked mid-effect | revoke during `INTENDED` | grant `REVOKED`, effect recorded | complete/fail the effect, then halt | no | **yes** | `AWAITING_REVIEW` |
| 26 | Tree mutated during verification | background write | result `VOID` | re-run at the new tree; finding | verify only | if repeated | `COMPLETE` / review |
| 27 | Stale worker wakes late | pause past lease, resume | fencing epoch stale | **all writes rejected** | no | no | successor unaffected |
| 28 | **Agent reaches for the control socket** | agent connects to `conductor.sock` | connect denied (sandboxed) or attempt logged | finding; no grant created | no | **yes** if unsandboxed | `AWAITING_REVIEW` |
| 29 | **`.conductor/` mutated on a run branch** | agent writes `APPROVED` | change rejected at reconciliation | never fetched; finding | no | **yes** | `AWAITING_REVIEW` |
| 30 | **Ineligible execution mode** | sensitive task, caps below requirement | attempt never starts | refuse with dimension named | no | **yes** | `BLOCKED` | *(decided at S7, **enforced at S9** — see note)* |

Rows 14, 15, 22, 24, 26, 27, 28, 29, 30 are the ones that most distinguish this design.

> **DISCHARGED AT S9.** The two notes below record why these four rows were held
> at `NOT RUN` through S8. S9 wired every call site they name, and each row is now
> scored from end-to-end evidence through `vertical::run_task` — never from unit
> coverage. See `docs/reports/S9-completion-report.md` for the row-by-row
> evidence, and ADR-0012 / ADR-0013 for the two state-machine defects that had to
> be corrected before the rows were reachable at all. The notes are kept because
> they are the reason the rows were not scored earlier, and deleting them would
> erase the discipline that caught them.

**Note on rows 12, 13, 25 — S8 built the mechanism, S9 wires the call site.**
S8 proves approvals are durable, exactly scoped, expiring, revocable and not double-spendable
— including across 50 real `SIGKILL` cycles. What it does **not** do is turn a
`require_approval` decision into an `approval_request` from inside a run: nothing in the run
path creates one, because that is enforcement and S9 owns it. Therefore, for the v1 sweep:
**row 13 is `NOT RUN`**; **row 12 is half enforced** (TTL and `REQUESTED` survive restart —
proven; "resumes on grant" — not reachable); **row 25 is mechanism-only** (all four
revocation outcomes tested, but no real run reaches revocation). Scoring any of these `PASS`
on the strength of the unit coverage would be exactly the "a similarly named test exists"
error the sweep forbids.

**Note on row 30 — decided at S7, NOT YET ENFORCED (S9 owns the call site).**
S7 implements `eligibility::check` as a pure function — requirements vs measured
capabilities, stale-or-absent probe refuses, `tool_interception` structurally unable to
satisfy a gating dimension — with full coverage including a positive control. It is **not
wired into the attempt-launch path**: §4.2 says "before launching an attempt", but that is
enforcement, which S9 owns, and wiring it at S7 would mean seeding a probe row into every
pre-existing test. Until S9 does so, **row 30 must be scored `NOT RUN`, not `PASS`** — the
decision is proven, the refusal is not yet reachable from a real launch.

**Note on row 9 — "attempt 2 not started" was off by one (found at S6).**
A fingerprint cannot be *identical* until it has been produced twice, so the earliest
point breaker 1 can fire is after the second attempt has already run. The invocation it
prevents is therefore the **third** — the second repair — not "attempt 2". The row's
intent (stop the moment repetition is provable, without spending another agent) is
implemented exactly; only its arithmetic was wrong. Row 9's fixture stops at 2
invocations against a ceiling of 4, counted three independent ways.

**Note on row 22 — its counting assertion is necessary but not sufficient (found at S5).**
"Assert exactly one commit and one ref update" cannot fail for either git effect under a
fixed `operation_id`, because **git is itself idempotent for both**: `git fetch` of an
unchanged ref performs no ref update at all (measured: reflog 1 before and after a blind
second fetch), and git refuses an empty commit. Disabling the fetch precondition re-check
therefore leaves the whole matrix green.

So the counting assertion must be paired with a test of the property that *can* fail —
that an effect Conductor cannot decide is recorded `AMBIGUOUS` and halts, rather than
being overwritten. `a_ref_conductor_did_not_move_is_noticed_rather_than_overwritten` is
that test, and it was falsified (reducing the precondition to two values makes it fail
with a non-fast-forward rejection). Keep both.

---

# Part 10 — Risks and Open Items

## 10.1 Resolved by measurement (no longer risks)

Hardlink isolation (M1–M3) · clone cost (M4–M5) · Codex containment (M6–M12) · control-surface reachability (M10–M11) · exit-code propagation (M15) · `--permission-prompt-tool` absence (M16) · hook denial (M17).

## 10.2 Open, with recommended temporary choices — none blocks S0–S9

| # | Question | Recommended choice | Binds at |
|---|---|---|---|
| 1 | Is bare Claude Code eligible for unattended sensitive work? | **No.** Ships at measured tier; sandboxed-Claude is separate and later. Never weaken `execution_requirements` to accommodate an adapter. | S15 |
| 2 | `/tmp` + `$TMPDIR` writable under Codex (M7) | Per-run `TMPDIR` inside the workspace; `/tmp` delta scanning in post-run audit; residual hole documented | S9 |
| 3 | Unrestricted credential reads under sandbox (M12) | Per-run `HOME`; adapter auth pinned via its own variable; secret scanning on all artifacts; `SECURITY.md` row "credential read: not prevented" | S9 |
| 4 | Operator nonce in v1? | Build the mechanism in S8, **default off**, activated by the eligibility check. Retrofitting the grant path later is where security bugs come from. | S8 |
| 5 | Does `plan validate` refuse unbound acceptance criteria? | **Refuse.** An unbound criterion is how a task reaches `COMPLETE` on an agent's word. Escape hatch: `manual: true` forces a review boundary. | S11 |

## 10.3 Intentionally deferred

Multi-machine anything · dashboard · adapters beyond two · container isolation (revisit if a runtime is installed) · network egress control for non-sandboxed adapters · submodules · automatic merge/rebase · cost tracking beyond `--max-budget-usd` · notifications · plan authoring assistance.

---

# Part 11 — Overengineering and Underengineering Guards

## 11.1 Containment rules

| Trap | Rule |
|---|---|
| Temporal clone | The `event` table is append-only and is **never replayed to produce state**. If replay is needed, that is revisit trigger §2.6(5). |
| Hatchet clone | One claim query, FIFO within priority. A second scheduling dimension requires an ADR **before** code. |
| Generic scheduler | No timers except lease expiry and approval TTL. Nightly runs are `launchd` calling `conductor task run`. |
| Event-sourcing framework | Materialized projections are **deleted from the design**. Reintroducing them requires refuting §3 in writing. |
| CI platform | Verification runs configured commands, in one workspace, sequentially. No matrix, no cross-check parallelism. If you want a matrix, you want CI. |
| Agent marketplace | Adapters are compiled in. Two. A third requires evidence that two are insufficient. |
| Security sandbox project | Conductor uses containment its launchers provide and otherwise **removes capabilities**. Conductor does not build isolation primitives. |

**General rule:** every subsystem must trace to a row in Part 9. If a feature makes no acceptance row pass, it is not in v1.

## 11.2 Where simplicity is dangerous

| Area | Tempting simplification | Minimum acceptable |
|---|---|---|
| Crash recovery | "resume the agent session" | Full reconstruction from git + fs + descriptors, tested with the session deliberately destroyed |
| Transaction boundaries | separate statements for claim/lease/event | One `BEGIN IMMEDIATE` per state change, event inside it |
| Git isolation | "clones are fine" | `--no-hardlinks`, proven by the S2 byte-identity test that the default clone fails |
| Duplicate effects | "retry the commit, git is idempotent" | It is not. Precondition-checked ledger; `AMBIGUOUS` halts |
| Policy evaluation | "most restrictive wins" | Ceiling + join, precedence matrix as a table test |
| Approval scope | `approved: bool` | `binding_hash` over action + facts + policy + scope; four distinct kinds |
| Approval channel | "0600 socket is human-only" | It is not. Three tiers; enforced only under a sandboxed launcher |
| Capability model | hardcoded per-adapter table | Measured on the host, cached by version triple, **fail closed when stale** |
| Plan versions | "just edit the roadmap" | Immutable approved versions, content-hashed, runs pinned |
| Verification | "tests passed, we're done" | Passed *when*, on *what tree*, with *what toolchain* — tree-hash binding, `VOID` on mid-run mutation |
| Stale attempts | "lease expired, take over" | `lease_epoch` fencing; every write carries the epoch |
| Secret handling | "we won't print secrets" | Scanner on every path into an artifact or packet, tested with planted secrets |

---

# Part 12 — Comparison Rubric (100 points)

| # | Dimension | Pts | Full marks require |
|---|---|---:|---|
| 1 | Product boundary & scope discipline | 8 | Human/automation line justified by which inputs have oracles; non-goals enforced by design |
| 2 | Global/project policy semantics | 10 | Precedence as an algebra; locked rules as a **ceiling**; scoped exceptions with expiry; snapshots + hashes; explainable negatives; fail-closed on unknown |
| 3 | Durable recovery & execution guarantees | 12 | Named transaction boundaries; fencing; startup reconciliation from **disk**; guarantee stated in tiers; no hidden-state dependency |
| 4 | Git safety & workspace isolation | 12 | Config/ref/hook/object-store isolation addressed; dirty tree, branch movement, orphans, submodules; **source repo provably unaffected by a hostile run** |
| 5 | Verification authority | 9 | Results bound to the tree observed; `INCONCLUSIVE` distinct from `FAIL`; completion criteria that exclude the agent's report |
| 6 | Repair correctness & loop prevention | 6 | Formal progress predicate; ≥2 independent loop-breakers; provable invocation bound |
| 7 | Approvals & security honesty | 12 | Approval integrity stated **per execution mode**, not absolutely; exact-action binding; revocation semantics per state; prevented-vs-detected table with a test behind every "prevented" |
| 8 | Plan versioning & decision ledger | 7 | Immutable approved plans; append-only decisions; runs pinned; survives database loss; `.conductor/**` protected from run branches |
| 9 | Agent abstraction & first adapter | 6 | Capabilities verified against installed binaries; no invented flags; first adapter justified by a measured capability |
| 10 | Human review bridge | 5 | Packets generated from durable state; deterministic; import is a mutating, gated operation |
| 11 | Implementation sequencing | 8 | Measurement before design commitment; vertical spine early; hard gates named; slices independently verifiable |
| 12 | Local-first usability | 3 | Works offline, no container runtime, no external services, single binary |
| 13 | Avoidance of premature platform complexity | 2 | No generic scheduler, DSL or DAG engine; named containment rules |
| | **Total** | **100** | |

**Scoring:** 0 = absent · 25% = mentioned · 50% = specified · 75% = specified with failure modes · 100% = specified with failure modes **and an acceptance test that would catch its absence**.

---

# Part 13 — The Ten Decisions That Matter Most

1. **The approval channel's integrity is a property of the execution mode, not of Conductor.** Enforced only under a sandboxed launcher, measured with a positive control. Everything else in the security model is downstream. (§4.3)
2. **Per-run clone with `--no-hardlinks`.** The difference between "the agent broke its sandbox" and "the agent broke your repository" — demonstrated, not argued. (§4.1, M2)
3. **Git and the filesystem are the source of truth; the agent's report is evidence.** Mandatory reconciliation on every exit from `RUNNING`. (§4.8)
4. **Build the runtime; don't adopt Temporal or Hatchet.** Durable execution is not reconciliation. (§2.6)
5. **Capabilities are measured on the host and fail closed when stale.** A hardcoded capability table becomes a lie after a CLI upgrade. (§4.2)
6. **Credential and capability absence is the primary enforcement mechanism.** What you never grant cannot be misused. (§4.9)
7. **Verification results are bound to the tree hash they observed.** Otherwise "passing" is a claim about a moment, not a state. (§4.5)
8. **Conductor may own only effects whose occurrence it can verify afterwards.** This keeps deployment out of v1 by construction. (§4.7)
9. **The plan and decision ledger lives in the repository**, and `.conductor/**` is unwritable from a run branch. (§3.2, §3.3)
10. **Fake agent and crash recovery are one slice, third.** If recovery is not proven early, everything built on it is unfalsifiable. (S3)

---

## Appendix — Evidence Index

| Claim | Source |
|---|---|
| Repository empty: 1 commit, 11-byte README, no schema, no migrations | `gh api repos/Krish-Verma/conductor/git/trees/main?recursive=1` |
| M1–M5 clone hardlinking, corruption, timing | `scratchpad/git-isolation-experiment.sh`, executed 2026-08-12 |
| M6–M15 Codex containment matrix | `scratchpad/codex-containment-2.sh`, executed 2026-08-12 (round 1 invalid — see below) |
| M16 no `--permission-prompt-tool` | `claude --help`, v2.1.228 |
| M17 `PreToolUse` denies a tool call | observed directly in session |
| Claude flags: structured output, sessions, hook events | `docs.claude.com/en/docs/claude-code/headless` + `claude --help` |
| Codex flags: sandbox, output-schema, resume, ephemeral | `codex exec --help`, `codex sandbox --help`, codex-cli 0.142.0 |
| Temporal self-host: Postgres + Elasticsearch; dev server loses state without `--db-filename` | `docs.temporal.io/self-hosted-guide/deployment`, `/cli/server` |
| Hatchet self-host: API server, engine, Postgres, RabbitMQ, dashboard; Lite needs Docker | `docs.hatchet.run/self-hosting`, `/self-hosting/hatchet-lite` |
| No container runtime; `sandbox-exec` present | `which -a docker podman colima`; `ls -la /usr/bin/sandbox-exec` |

**On the invalid experiment round.** The first containment run was wrong twice — the "outside" directory was itself under `/tmp` (which the policy permits), and the AF_UNIX test failed on `sun_path` length rather than on the sandbox. Both flaws produced **false permissive** results: escapes that had not occurred, and a socket denial that was a test bug. Failing toward "the sandbox is weaker than it is" is the safer direction, but it is exactly the kind of result that gets quoted as fact if it is not re-run. It is recorded here rather than deleted.
