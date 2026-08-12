# Conductor — Independent Architecture Review, Debate, and Revised Roadmap

**Reviewer:** Claude (Opus 5)
**Date:** 2026-08-12
**Status:** Review and proposal. Nothing implemented. No repository modified.
**Subject:** the baseline architecture supplied in the review prompt, treated as ChatGPT's proposal.

---

## 1. Repository Inspection

### 1.1 What is actually there

`https://github.com/Krish-Verma/conductor` — inspected via `gh api`, not assumed.

| Property | Value |
|---|---|
| Visibility | private |
| Created | 2026-08-03T18:45:27Z |
| Last push | 2026-08-03T18:45:28Z (one second after creation) |
| Default branch | `main` |
| Branches | `main` only |
| Commits | 1 — `6b830311` "Initial commit" |
| Tree | one blob: `README.md`, 11 bytes |
| `README.md` content | `# conductor` |
| `diskUsage` | 0 |

Local `/Users/krishverma/Documents/Conductor` is an **empty directory and not a git repository**.

**Absent:** `CLAUDE.md`, `AGENTS.md`, `.conductor/`, any package manifest (`Cargo.toml`, `package.json`, `pyproject.toml`), any source directory, tests, scripts, CI configuration, architecture documents, roadmap documents, ADRs.

### 1.2 Findings

- **The repository is empty for practical purposes.** No architecture has been encoded. No implementation has begun. There is nothing to conflict with this prompt because there is nothing there.
- **The only authoritative artifact today is this prompt** plus the user's global `~/.claude/CLAUDE.md` and `~/.claude/AGENTS.md`.
- **No conflicts** between repository truth and the prompt, because the repository asserts nothing.

### 1.3 The evidence that actually matters is next door

Conductor is empty, but the workflow Conductor intends to encode is **already running, in a sibling repository, in a form I can measure**.

`/Users/krishverma/Documents/Nerve` (also `github.com/Krish-Verma/Nerve`), Rust, clean working tree, `main`:

```
docs/
├── ARCHITECTURE.md      PRODUCT.md          ROADMAP.md
├── THREAT-MODEL.md      TESTING.md          CLEANROOM.md
├── CONTINUATION.md      FINAL-ACCEPTANCE.md PRODUCT-CAPABILITY-MATRIX.md
├── decisions/           ADR-0001 … ADR-0009
├── plans/               nerve-master-build-plan.md, slice-02 … slice-15 (28 files)
└── reports/             restart-recovery-report.md
```

Measured:

| Metric | Value |
|---|---|
| Commits | 157 |
| First commit | 2026-07-31 |
| Last commit | 2026-08-11 |
| Distinct working days | 10 |
| Rust files | 155 |
| Lines of Rust | 129,751 |
| Files under `tests/` | 54 |

**This is the single most important finding in the inspection**, and it changes several recommendations below:

1. **The "control plane" Conductor wants to automate already exists as files in a repository.** `docs/plans/slice-NN-*.md` are the approved plans. `docs/decisions/ADR-NNNN-*.md` are the accepted decisions. `docs/reports/*.md` are the review packets. `docs/CONTINUATION.md` is the continuation packet. Conductor does not need to invent a plan ledger; it needs to **make the one already in use machine-readable and enforce it**. I build on this in §5 and §8.

2. **The user's engineering culture is unusually exacting, and the architecture should assume it.** Nerve's slice plans contain sections literally titled *"Refutations of this plan's own first draft"*, ADRs contain *"Falsification trigger (pre-registered)"*, and acceptance criteria are written as named tests rather than prose. `docs/plans/slice-14-human-confirmed-memory.md` contains a correction that reads: *"Corrected 2026-08-08 — 'MCP may propose' would break an invariant already proved."* A design document that hand-waves will be rejected by the person reading it. I have tried to write to that standard.

3. **Nerve has already solved, and documented, the hardest single problem in Conductor's threat model.** See §7.1. This is not a coincidence and it should be reused rather than rediscovered.

4. **`docs/reports/restart-recovery-report.md` is a hand-executed instance of Conductor's Recovery Principle (prompt §7).** After a host restart on 2026-08-01, the user located the repository, checked for interrupted git operations, ran `git fsck`, inspected one dangling commit, proved it superseded by diffing it against `HEAD`, determined that a killed subagent *"had not written to disk"* because the tree was clean, re-ran verification, and — critically — recorded that the dead agent's one durable output *"is treated as a hypothesis to re-derive empirically, not as an established result, because it exists only in session notes and no artefact in the repository supports it."*

   That last sentence is the Recovery Principle, already stated better than the prompt states it. Conductor's recovery engine should be a mechanization of this exact report.

### 1.4 Environment facts (measured on this machine, 2026-08-12)

These constrain the architecture and several are decisive.

| Fact | Value | Why it matters |
|---|---|---|
| OS | macOS 26.6, arm64 | — |
| Rust | `rustc 1.97.1`, `cargo 1.97.1` | §5.2 stack decision |
| Node | `v24.15.0` | §5.2 |
| SQLite | `3.51.0` | §8 |
| **Container runtime** | **none — `docker`, `podman`, `colima` all absent** | **Decisive for §4 (Hatchet/Temporal) and §7 (isolation)** |
| Git | `2.50.1` (Apple Git-155) | §11 |
| Claude Code CLI | `2.1.228` | §12 |
| Codex CLI | `codex-cli 0.142.0` | §12 |

### 1.5 Unknowns I did not resolve

- Whether the user intends Conductor to ever be distributed to other people, or to remain a personal tool. This changes the weight on installability and multi-user policy. **I assume personal-tool-first, distributable-later**, and flag where that assumption is load-bearing.
- Whether Nerve and Conductor are intended to merge commercially. I assume not.
- Actual concurrency target. I assume **1–4 concurrent runs on one machine**, and flag it as the single number that, if wrong by 100×, invalidates §4.

---

## 2. Product Definition and Boundary Review

### 2.1 Definition

> Conductor is a local-first execution supervisor for coding agents. It takes a human-approved, versioned plan; runs it as bounded agent tasks in isolated repository workspaces; independently verifies the result against the repository rather than against the agent's claims; reconstructs state from durable evidence when an agent or the host dies; refuses to advance past policy and approval boundaries; and produces the packets a human needs to review it.

I would sharpen the prompt's own definition in one place. The prompt says Conductor *"advances until the approved acceptance criteria are satisfied or a human decision is required."* Add: **and never advances on the basis of an agent's self-report.** That is the product, and it should be in the sentence.

### 2.2 Target user and job

**Target user (v1):** one engineer, on one machine, running one or two repositories, who is already doing this loop by hand and can prove it — see §1.3.

**Job to be done:** *"I approved a plan. Execute the next slice of it without me watching, do not let the agent lie to me about whether it worked, do not let it do anything I did not authorize, and if it dies, know exactly where we are without asking me."*

**Anti-job:** *"Decide what to build."* Conductor must never acquire an opinion about product direction.

### 2.3 Responsibility split

**Human keeps:** vision; product decisions; strategic architecture; the ChatGPT↔Claude debate; resolving disagreements; approving plans; architecture pivots; high-risk approvals; changing locked policy.

**Conductor owns:** plan/decision durability and versioning; task materialization and sequencing; policy evaluation; workspace isolation; agent process lifecycle; evidence capture; verification; reconciliation; bounded repair; approval gating and persistence; Conductor-owned side effects; packet generation; recovery.

**Neither owns:** correctness of the code. That belongs to tests, and Conductor's job is to run them honestly.

### 2.4 On the proposed human/automation boundary — I agree, with two moves

The prompt's §5 boundary (strategic planning stays human-mediated; execution is automated) is **correct and I would not weaken it**. The reason is not caution, it is a property of the artifact: a plan is the one input Conductor cannot verify. Everything else Conductor handles has a deterministic oracle — git state, exit codes, file hashes. A plan has none. Automating the production of the only unverifiable input, and then executing it unattended, is how you get a system that confidently does the wrong thing for six hours.

Two changes:

**Move OUT of v1 automation — "agent selection."** Prompt §6 lists *"select an eligible coding agent."* In v1 there will be one adapter, then two. Selection logic is a config field (`agent: codex`), not a subsystem. Building a selection policy before there is anything to select between is the first place this design invents work. **Verdict: REMOVE from v1.**

**Move INTO automation — plan *validation*, though not plan *authoring*.** The prompt keeps plans entirely human. But a plan can be checked mechanically for things a human reliably gets wrong: task IDs that do not resolve; acceptance criteria with no verification command bound to them; file scopes that name paths that do not exist; slices that depend on slices sequenced after them; verification commands that are not runnable in this repository. `conductor plan validate` should refuse to let a plan reach `APPROVED` with any of those. This is not the machine deciding direction; it is the machine refusing to accept an unexecutable instruction, which is exactly what a compiler does. **Verdict: ADD.**

### 2.5 Non-goals — I accept the prompt's list, and add three

Accepted as written (§3, §35). Added:

- **Not a code reviewer.** Conductor runs the checks; it does not form an opinion about code quality. "Optional independent agent review" (prompt §12) is where this leaks in. See §14.6.
- **Not a credential broker.** Conductor should hold as few secrets as possible, ideally none it did not create. Every secret Conductor stores is a secret an agent might reach.
- **Not a transcript archive.** Storing full agent transcripts by default is a privacy and disk liability with unclear payoff. Store the *event stream* and the *final report*; store raw transcripts only under an explicit per-project opt-in, and never commit them.

---

## 3. Evaluation of the Baseline ChatGPT Architecture

| Subsystem | Verdict | Reason | Recommended change |
|---|---|---|---|
| Human-mediated strategic planning | **KEEP** | The plan is the only unverifiable input; automating it removes the one oracle-free step from human hands. | Add mechanical `plan validate` (§2.4). |
| Custom runtime | **KEEP** | Neither Hatchet nor Temporal solves any Conductor-specific problem, and both cost a container runtime this machine does not have. Full argument §4. | Build the *minimum* owned runtime (§10), not a scheduler. |
| SQLite | **KEEP** | Right call, well-precedented (Nerve ADR-0001, measured). WAL gives one writer + many readers, which matches a single-daemon topology exactly. | Bundle it. `BEGIN IMMEDIATE` for all write transactions. |
| Event journal | **KEEP WITH MODIFICATIONS** | Justified as an **append-only evidence log**; *not* justified as event-sourcing of domain state. | Journal records observations about the world (process exited, sha observed, exit code). Domain state stays ordinary mutable rows. §8.2. |
| Materialized projections | **REPLACE** | Projections imply the journal is the source of truth. It is not — **Git and the filesystem are**. A projection you trust over the repository is the exact failure this product exists to prevent. | Delete the concept. Mutable state tables, transactionally updated, reconciled against Git. §8.2. |
| Daemon | **KEEP WITH MODIFICATIONS** | Needed eventually (survive terminal close), but not needed for slices 1–10, and it makes crash tests harder from day one. | Foreground supervisor first; daemon introduced at the slice whose acceptance criterion *is* "survives terminal close". §5.3, §18. |
| `WorkflowRuntime` abstraction | **REMOVE** | An interface with one implementation, designed to host a backend we have argued against adopting. It leaks `claim`/`heartbeat`/`lease` into the domain and is the single most likely path to accidentally building Temporal. | Replace with: domain logic as pure `(state, evidence) → decision` functions, plus a thin `Store`. §5.4. |
| Global/project policy inheritance | **KEEP WITH MODIFICATIONS** | Hierarchy is right; "most restrictive wins" is under-specified and wrong in one direction. | Effects form a lattice with a fixed join; locked rules are a *ceiling*, not a participant in the join. §6.2. |
| Typed policy actions | **KEEP** | Correct, and the most important single idea in the baseline. Natural-language policy is not enforceable. | Add `effect: unknown` and make unmapped actions fail closed. §6.4. |
| Isolated worktrees | **REPLACE** | **Worktrees share `.git/config`, refs, hooks and the object store.** They isolate files, not the repository. An agent running `git remote set-url` inside a worktree mutates the user's real repository. | **Per-run local clone** (`git clone` hardlinks objects locally), with `origin` removed. §11.2. This is my strongest technical disagreement. |
| Git-authoritative reconciliation | **KEEP** | Correct and non-negotiable. | Extend the reconciled surface to `.git/config`, refs, hooks, and stash. §11.5. |
| Deterministic verification | **KEEP WITH MODIFICATIONS** | Right principle; two gaps. | (a) A result is bound to the tree hash it observed — if the tree moved, the result is `VOID`, not `PASS`. (b) Timeout is `INCONCLUSIVE`, a third outcome, not a failure. §14. |
| Bounded repair | **KEEP WITH MODIFICATIONS** | Right, but "no progress" is undefined in the baseline, and undefined loop-detection is how budget burns. | Concrete progress predicate on failing-check sets and failure fingerprints. §14.5. |
| Approval model | **KEEP WITH MODIFICATIONS** | Scoped, durable approvals are right. But the baseline's approval channel is **reachable by the agent it is meant to constrain**. | Approval grant must not be issuable from the agent's environment. §7.1 — this is the review's #1 finding. |
| Focused context packets | **KEEP** | Correct; avoids transcript replay and makes recovery independent of hidden state. | Link large evidence by path/sha; embed only what is decision-relevant. §13. |
| Fake-agent-first | **KEEP** | The best sequencing idea in the baseline. | Merge the fake-agent slice with the crash-recovery slice; the scenarios *are* the tests. §18. |
| Optional Nerve integration | **KEEP WITH MODIFICATIONS** | Boundary is right. But the proposed integration (Conductor queries Nerve, embeds answers) is the weaker of the two available. | Primary integration is `--mcp-config` handing `nerve mcp` to the *agent*; Conductor's own use is a `nerve check` freshness gate. §16. |
| CLI-first | **KEEP** | Correct. | The listed surface is ~2.5× too large for v1. Cut to 12 commands. §17. |
| Dashboard later | **KEEP** | Correct, and stronger than the baseline states it. | Add the rule: no dashboard-only mutation ever. §17.3. |
| Agent selection | **REMOVE** | One adapter, then two. Config field, not subsystem. | `agent:` in project config. |
| Exactly-once framing | **REPLACE** | The prompt is right to be suspicious. The baseline never states the actual guarantee. | State it precisely, three tiers. §10.9. |
| "Optional independent agent review" as verification | **DEFER** | A nondeterministic check inside a subsystem whose entire value proposition is determinism. | Post-v1, and never gating. §14.6. |

---

## 4. Custom vs Hatchet vs Temporal

### 4.1 The question the comparison must answer

Not *"can this framework run durable workflows?"* — all three can. The question is:

> **What fraction of Conductor's difficulty does this framework remove?**

So first: what is actually hard in Conductor?

| Hard problem | Removed by a workflow engine? |
|---|---|
| Isolating a repository from an agent that has a shell | **No** |
| Determining what an agent actually did, from Git | **No** |
| Deciding whether verification results are trustworthy | **No** |
| Policy algebra, locking, scoped exceptions | **No** |
| Binding an approval to one exact action | **No** |
| Reconstructing state after the host dies, from disk | **No — and this is the important one** |
| Supervising an agent subprocess and classifying its death | **Partially** (retry/timeout scaffolding) |
| Not losing state when the daemon crashes | **Yes** |
| Timers, waits, retries | **Yes** |
| Distributed workers, queue fairness, HA | **Yes — and Conductor has none of these** |

Two of ten. And the two are the two that SQLite plus a `WITH`-guarded `UPDATE` also give.

### 4.2 The structural argument against Temporal specifically

Temporal's model is **deterministic replay of workflow code against an event history**. Its correctness guarantee is: *given the same history, your workflow function reaches the same state.*

Conductor's correctness requirement is different in kind: *given whatever actually happened on disk, determine the true state.* The prompt states this itself in §7 — recovery inspects Git, the filesystem, and stored output. **The ground truth is external and mutable.** A Temporal workflow that replays flawlessly still has to go re-read `git status`, because the user may have edited the branch while the workflow was suspended (prompt Scenario K). Replay does not make that unnecessary; it makes it invisible.

So Temporal would give Conductor a durable execution history it must then distrust, and the reconciliation engine — the expensive part — gets built anyway.

There is a second cost. Temporal's determinism constraints (no wall-clock, no direct I/O, no nondeterministic iteration in workflow code) plus versioning-for-replay would push Conductor's domain logic into a shape dictated by Temporal. The prompt asks whether this would happen. **Yes, and visibly**: policy evaluation, task selection and reconciliation all want to read the world, and under Temporal they must all become Activities, leaving workflow code as a thin orchestration shell whose only job is to be replayable. That is a large refactor tax paid for a guarantee Conductor cannot rely on anyway.

### 4.3 Operational cost, measured rather than asserted

**Temporal** — from current docs:
- The production self-hosted path is Docker Compose with **PostgreSQL and Elasticsearch**, gRPC frontend on 7233, Web UI on 8080. Docs explicitly warn that Temporal hosts *"should be secured similarly to a database"* and not exposed to the internet.
- The single-binary path, `temporal server start-dev`, prints a banner reading *"The development server is not intended for production use. It skips certain HTTP security checks."*
- And the detail that settles it: **`--db-filename` — "Path to file for persistent Temporal state store. By default, Workflow Executions are lost when the server process dies."**

  The easy Temporal path is the non-durable one. Adopting Temporal for durability and then running the dev server is a system that loses state on crash — the precise failure Conductor exists to prevent. The durable path is Docker + Postgres + Elasticsearch.

**Hatchet** — from current docs, self-hosting the control plane means deploying: **API Server, Engine (gRPC), PostgreSQL, optional RabbitMQ, and a Dashboard.** `hatchet-lite` reduces the footprint but *"requires Docker installed locally"* and is *"designed for development and low-volume use-cases."*

**And this machine has no container runtime at all** — `docker`, `podman`, `colima` are all absent (§1.4). Adopting either framework's durable configuration means installing and operating Docker Desktop plus PostgreSQL as a permanent prerequisite for a single-user tool that supervises perhaps two subprocesses at a time.

### 4.4 What custom actually costs — stated honestly

The prompt is right that custom-runtime cost is usually understated. Here is what Conductor must build, and what it must *not*:

**Must build (~1,200–1,800 lines of Rust, plus tests):**
- One `jobs` table with a status enum and a `lease_expires_at`.
- Atomic claim: a single `UPDATE … WHERE status='READY' AND (lease_expires_at IS NULL OR lease_expires_at < ?) RETURNING …` inside `BEGIN IMMEDIATE`.
- Heartbeat: `UPDATE jobs SET lease_expires_at = ?`.
- Startup reconciliation: find rows whose lease expired, reconcile each against Git.
- One timer need: approval expiry. Which is a `WHERE expires_at < now()` on wake, not a timer service.
- Idempotency: a `side_effects` table keyed by a deterministic operation ID, written in the same transaction as the state change.

**Must NOT build:** priority queues, fair scheduling, worker pools, backpressure, cron, DAG evaluation, retry policy DSLs, distributed locks, sagas, a replay engine, a workflow versioning scheme.

The honest risk is not that the above is hard. It is that it is *fun*, and it grows. §25 addresses containment.

### 4.5 Verdict

> **Build the runtime. SQLite in WAL mode, one process, lease-based claims, evidence-based reconciliation. Do not adopt Hatchet or Temporal for v1, and do not build an abstraction whose purpose is to make adopting them easy later.**

The last clause is deliberate and is where I disagree with the baseline's §26. A `WorkflowRuntime` interface written today, against a backend we have argued against, will be wrong in the specific ways that matter — because we do not yet know which semantics we need. The genuine portability insurance is different and cheaper: **keep the domain decisions pure.** If `next_action(state, evidence) -> Decision` is a total function with no I/O, it can be driven by anything later, and it is testable today without a runtime at all. Interfaces guess at the future; pure functions do not have to.

### 4.6 Triggers to revisit — objective, not vibes

Revisit if **any one** becomes true:

1. Execution spans **more than one machine** (a second worker host, or cloud agent execution). This is the real trigger — it invalidates single-writer SQLite, and everything else in this list is downstream of it.
2. **> 3 concurrent human operators** against shared state.
3. Sustained **> 50 concurrent runs**, or SQLite write contention appears in profiles (measure: p99 claim latency > 100 ms).
4. A single run must remain suspended **> 30 days** across daemon and OS upgrades — schema-evolution-under-suspension is where hand-rolled durability genuinely gets ugly.
5. **Cumulative runtime bug-fixing exceeds 15% of commits over any 30-day window**, measured from `git log`. This is the falsification trigger for §4.5 and it should be pre-registered in the ADR, in the style of Nerve's ADR-0001.
6. Requirements appear for cross-run orchestration graphs — fan-out/fan-in with joins — rather than a single sequential task queue.

If (1) or (5) fire, **Temporal over Hatchet**, because at that point the value is the replay/versioning model and the ecosystem, not the queue.

---

## 5. Proposed Final Architecture

### 5.1 Shape

```
┌────────────────────────────────────────────────────────────────────┐
│ HUMAN SURFACE — trusted; not reachable from an agent workspace     │
│   conductor CLI over a unix socket at $XDG_RUNTIME/conductor.sock  │
│   socket mode 0600 · owned by the user · path never exported to    │
│   an agent process · approval verbs live ONLY here                 │
└───────────────────────────────┬────────────────────────────────────┘
                                │
┌───────────────────────────────▼────────────────────────────────────┐
│ CONDUCTOR CORE (one process; foreground in v1, daemon from S15)    │
│                                                                     │
│  ┌── pure domain (no I/O, exhaustively tested, no runtime) ──────┐ │
│  │  next_action(state, evidence) -> Decision                     │ │
│  │  reconcile(baseline, observed) -> Reconciliation              │ │
│  │  evaluate(policy_snapshot, action, facts) -> Effect + why     │ │
│  │  classify(attempt_evidence) -> Outcome                        │ │
│  │  progressed(prev_failure, next_failure) -> bool               │ │
│  └───────────────────────────────────────────────────────────────┘ │
│                                                                     │
│  ┌── effectful adapters (thin; each independently fakeable) ─────┐ │
│  │  Store(SQLite)   Workspace(git)   Verifier(proc)              │ │
│  │  Supervisor(proc) Packets(fs)     Evidence(Nerve, optional)   │ │
│  └───────────────────────────────────────────────────────────────┘ │
└──────┬────────────────────────┬─────────────────────┬──────────────┘
       │                        │                     │
┌──────▼──────┐   ┌─────────────▼────────────┐  ┌─────▼─────────────┐
│ SQLite      │   │ RUN WORKSPACE            │  │ ARTIFACTS         │
│ WAL         │   │ per-run LOCAL CLONE      │  │ ~/.local/share/   │
│ execution   │   │ (not a worktree — §11.2) │  │ packets, reports, │
│ state only  │   │ origin REMOVED           │  │ logs, diffs       │
└─────────────┘   │ scrubbed env, no creds   │  └───────────────────┘
                  │                          │
                  │   ┌──────────────────┐   │
                  │   │ AGENT SUBPROCESS │   │  ← untrusted
                  │   │ codex exec / claude -p│  ← may run arbitrary shell
                  │   └──────────────────┘   │
                  └──────────┬───────────────┘
                             │ Conductor-owned integration only:
                             ▼ fetch run branch into the real repo
                  ┌──────────────────────────┐
                  │ USER'S REAL REPOSITORY   │  ← agent never has a handle on it
                  └──────────────────────────┘
```

### 5.2 Stack: Rust — and the reason is not the one you would expect

**Recommendation: Rust for the engine.** Reasons, in descending weight:

1. **Six state machines with "invalid transitions" as an explicit deliverable (prompt §23).** Rust enums plus exhaustive `match` make an unhandled transition a compile error. In TypeScript the same guarantee is a discriminated union plus a `never` check plus the discipline to keep writing it. The prompt names state machines as a place where underengineering is dangerous; this is where the type system pays rent.
2. **Measured velocity, not assumed velocity.** The standard argument against Rust is iteration speed. For this user it is refuted by evidence: **129,751 lines of Rust, 155 files, 54 test files, 157 commits, slices 1→14d, in 10 working days** (§1.3). This is not an argument that Rust is fast; it is an argument that *this developer* is fast in Rust, which is the only version of the claim that matters.
3. **Single static binary matters more than usual here**, because there is no container runtime (§1.4) and Conductor is the thing that must still work when everything else is broken.
4. **Operational reuse from Nerve**: bundled SQLite via `rusqlite`, migrations, loopback+token surface, `--json` on every command, stable exit codes. Conductor can adopt a proven skeleton.

**Explicitly not reasons:** performance (Conductor is ~99% blocked on subprocesses; throughput is irrelevant), memory safety in the abstract, or "it sounds like infrastructure." The prompt warns against that last one and it is a fair warning.

**The honest counter-argument.** Early slices will churn schemas hard — packets, reports, policy facts — and serde plus compile cycles tax churn more than `JSON.parse` does. And the Claude **Agent SDK's `canUseTool` in-process approval callback is TypeScript/Python only.** If Conductor drove Claude in-process, TypeScript would win outright.

That counter-argument fails for a specific reason, and it is worth stating because it is load-bearing: **Conductor should drive agents as subprocesses, not as in-process SDK calls.** A subprocess can be `SIGKILL`ed, reparented, inspected via `/proc`-equivalents, resource-limited, launched with a scrubbed environment, and — most importantly — *survives the supervisor's own death in a way an in-process SDK object does not*. Conductor's core promise is recovering from its own crash. An agent embedded in Conductor's process dies with Conductor every time. Once the transport is "spawn a CLI and read JSONL," host language stops mattering for adapters, and the `canUseTool` advantage evaporates — replaced by hooks, which work from any language (§12.6).

**If the user were not already fluent in Rust, TypeScript would be the correct answer.** I want that recorded, because it means this decision is about the team of one, not about the domain.

Deps: `rusqlite` (bundled), `clap`, `serde`/`serde_json`, `serde_yaml` or `toml`, `thiserror`/`anyhow`, `tempfile`. Async: **`tokio` is required here** — concurrent subprocess supervision with timeouts and cancellation is exactly what it is for. This reverses Nerve's "no async runtime" stance (ADR-0004/master plan §4), and that reversal should be written down as its own ADR with the reason, because Nerve's stance was correct *for indexing*, and Conductor is not indexing.

Git: **shell out to `git`**, do not link `libgit2`/`gix`. Reason: Conductor's job is to observe the same repository state the user observes, and the user's ground truth is the `git` binary — including their `core.*` settings, hooks and filters. A library that disagrees with `git status` is worse than useless in a product whose thesis is "trust the repository."

### 5.3 Process model: foreground first, daemon at S15

The baseline starts with a daemon. I would not, for a sequencing reason: **`kill -9` on a foreground supervisor is the cheapest crash test in existence**, and slices 1–10 are almost entirely about surviving that. Adding daemon lifecycle, socket handling, stale-PID logic and launchd integration before the recovery semantics are proven means debugging two things at once.

The daemon arrives when its acceptance criterion is testable on its own: *"close the terminal; the run continues; `conductor status` from a new shell reports it."* Same binary, `conductor daemon`, auto-started on demand by the CLI.

### 5.4 What changed vs. the baseline

1. **Worktree → per-run local clone** (§11.2). The largest correctness change.
2. **Materialized projections deleted**; journal demoted to an evidence log (§8.2).
3. **`WorkflowRuntime` interface deleted**; replaced by pure domain functions (§4.5).
4. **Approval channel moved out of the agent's reach** and stated as a boundary, not an identity check (§7.1).
5. **Credential absence promoted to the primary enforcement mechanism** (§7.3).
6. **Daemon deferred** to the slice that needs it (§5.3).
7. **Verification results bound to tree hash**; `INCONCLUSIVE` added as a third outcome (§14).
8. **Plan/decision ledger lives in the repository as files**, SQLite indexes it (§8.1).
9. **Agent selection removed** from v1.
10. **Nerve integration inverted**: give `nerve mcp` to the agent rather than embedding Nerve answers in packets (§16).

---

## 6. Policy Architecture

### 6.1 The one thing to be honest about first

A policy engine decides **what Conductor will do**. It does not decide what an agent with a shell can do. §7 handles that. Everything in this section is about Conductor's own behaviour and about *detecting* violations — not about preventing an agent from typing a command.

### 6.2 Precedence — and why "most restrictive wins" is not quite right

The prompt proposes: *"the most restrictive applicable rule wins unless an applicable rule explicitly allows override and the user grants a scoped exception."* That is close, but it conflates two different operations, and the conflation produces a bug: under a pure "most restrictive wins" join, a **locked** global rule and an ordinary project rule are peers, and a *sufficiently restrictive* project rule can shadow a locked global one — which sounds harmless until you realize it also means locking is doing no work in the one direction it exists for.

Split it:

**Effects form a total order (the join is `max`):**

```
allow  <  require_approval  <  deny
```

**Evaluation is two stages, not one:**

```
Stage 1 — CEILING (locked policy)
    Locked global rules produce a maximum permissiveness.
    Nothing below can exceed it. Not project rules, not exceptions,
    not a human grant (a locked rule must be explicitly unlocked
    first, which is its own audited operation).

Stage 2 — JOIN (everything else)
    effect = max(
        builtin_invariant,       -- deny, always
        global_default,
        project_rule,            -- may only tighten past global default
        task_constraint
    )
    then, if a scoped exception matches exactly and is unexpired
    and Stage 1's ceiling permits it:
        effect = exception.effect
```

Consequences, stated as invariants:

- **A project can always tighten. A project can never loosen past the ceiling.** Tested.
- **A scoped exception can only lower an effect within the ceiling.** An exception cannot unlock a locked rule; unlocking is a separate operation with its own record.
- **Unknown action → `deny`.** Fail closed. The prompt's typed-action list will be incomplete on day one; the incompleteness must not read as permission.
- **Built-in invariants are not configurable at all**: never write outside the run workspace; never print a value that matched a secret detector; never push to a remote; never operate on a repository that is not registered.

### 6.3 Snapshots, hashes, reproducibility

At **run creation**, Conductor serializes the fully resolved policy into a canonical form (sorted keys, no timestamps — the determinism discipline Nerve's export format already worked out), hashes it BLAKE3, and stores the blob keyed by hash. The run stores `policy_hash`.

**A run evaluates against its snapshot for its entire life.** Policy edited mid-run does not affect the run (prompt Scenario R). Rationale: an approval granted under one policy must not authorize an action under a different one, and a half-old/half-new evaluation is not explainable after the fact.

**Exception:** if the *new* policy is strictly more restrictive for a pending action, Conductor pauses the run and asks. Never silently proceeds under a policy the human has just tightened. This is the one place where snapshot-purity yields, and it yields toward safety.

### 6.4 Typed actions

Keep the prompt's §9 taxonomy. Four changes:

1. **Add `effect: unknown`** as a distinct evaluation outcome from `deny`, so `conductor policy explain` can say *"no rule matched and the default is deny"* rather than implying a rule existed.
2. **Facts must declare their derivation.** Every fact carries `source: deterministic | model_assisted | human`. A `require_approval` decision may rest on any of them; a **`deny` decision must rest only on `deterministic` facts.** A model must never be the sole reason Conductor blocks work, because a hallucinated block is indistinguishable from a real one and will train the user to override blocks.
3. **Deterministic fact extraction is the default and covers most of the taxonomy:**

   | Action | Deterministic fact source |
   |---|---|
   | `dependency.add.runtime` | diff of `[dependencies]` in `Cargo.toml` / `dependencies` in `package.json` |
   | `lockfile.modify` | path match on `Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`, `uv.lock` |
   | `git.remote.modify` | `git config --get-regexp '^remote\.'` before vs. after |
   | `git.push` | new commits on a remote-tracking ref; reflog |
   | `database.migration.create` | new file under configured migration globs |
   | `filesystem.write.outside_workspace` | comparison against workspace root; plus sandbox denial events |
   | `credential.access` | env allowlist violations; secret-pattern scan of the diff |
   | `architecture.change` | path globs the project declares as architectural |

4. **`architecture.change` is the one that cannot be made deterministic**, and the baseline should stop pretending otherwise. Path globs are a proxy. Treat it as `model_assisted` → therefore `require_approval` at most, never `deny`, and always with the diff attached so the human is deciding on evidence rather than on a classifier's opinion.

### 6.5 Explainability

`conductor policy explain <action> [--run <id>]` must print: the action; the resolved effect; the ceiling that applied; every rule that matched **and every rule that was considered and did not match, with the reason**; the facts and their sources; the policy hash; and, if an exception applied, its ID, scope and expiry. Negative results are the ones people debug.

---

## 7. Security and Threat Model

### 7.1 The finding that matters most — and it is already documented in Nerve

`docs/plans/slice-14-human-confirmed-memory.md` §1, in the user's own repository:

> *"At a local CLI, an agent invoking `nerve memory confirm` is byte-indistinguishable from a human invoking it. There is no cryptographic, procedural or observational control available inside this product that separates the two callers."*
>
> *"So the honest control is not an identity check. It is a surface boundary, and it is testable."*

**Conductor has exactly this defect, and the baseline architecture does not solve it.** The baseline puts `conductor approve` on the same CLI, on the same machine, reachable from the same shell that the coding agent is holding. An agent that has been told to implement a feature and hits an approval gate can, in principle, run `conductor approve <id>` and continue. Every approval semantic in prompt §30 — scope, expiry, one-time binding — is downstream of an authorization the agent can forge.

Nerve's answer was to make the write path **absent** from the agent's surface, not gated on it — *"the code path is absent, not gated"* — and to assert that absence with a source scan test.

**Conductor's answer must be the same shape, and it is stronger here because Conductor controls the agent's environment:**

1. **The control socket is not in the agent's world.** The daemon listens on a unix socket at a path under the user's runtime dir, mode `0600`. `CONDUCTOR_SOCKET` is **removed from the environment handed to the agent**, the socket path is outside the run workspace, and under Codex's `workspace-write` sandbox the agent cannot reach it at all.
2. **Mutating verbs require a socket connection.** There is no file-drop approval, no "write `approved: true` into a YAML in the repo" path. Anything an agent can write to, an agent can forge.
3. **The binary reachable from a workspace is not the same binary.** If a `conductor` shim is on the agent's `PATH` at all, it exposes read-only verbs (`status`, `task show`) and physically lacks the approval code path — asserted by a source-scan test in the style of Nerve's `crates/nerve-server/tests/layering.rs`.
4. **What cannot be enforced, written down:** a human who hands an agent an unsandboxed shell with their own `PATH` and `HOME` has removed this boundary, and Conductor cannot detect it. Nerve states this about itself; Conductor must state it too, in `SECURITY.md`, not in a comment.

This is the review's single most important change. Without it, the approval system is theatre with good record-keeping.

### 7.2 Threat layers — what each can actually do

| Layer | Prevents | Detects | Available today on this machine? |
|---|---|---|---|
| 1. Prompt instructions | **nothing** | nothing | yes, and worth ~0 as a control |
| 2. Deterministic policy | Conductor's own actions | proposed actions | yes |
| 3. Conductor-owned side effects | agent performing push/deploy/migrate *through Conductor* | — | yes |
| 4. Agent permission hooks | specific tool calls, by pattern | attempted calls | **Claude: yes** (§7.4) |
| 5. OS sandbox | writes outside workspace; possibly network | escapes | **Codex: yes.** Claude: no. Container: no runtime installed |
| 6. Credential isolation | **push, deploy, cloud API, DB access** | — | **yes, and cheap** |
| 7. Network control | egress | — | only via Codex sandbox; unverified |
| 8. Post-run audit | nothing | almost everything | yes |

**Layer 1 must be labelled as worth nothing.** Writing "do not push" in a prompt is documentation of intent, not a control. The baseline is right to separate these; I am making the ranking explicit so nobody later mistakes a well-written packet for a security boundary.

### 7.3 Credential isolation is the primary control, and the baseline underweights it

This is the cheapest high-value control available and it deserves to be first, not fourth:

> **An agent that has no push credential cannot push, regardless of what it types, what it is told, or whether any hook fires.**

Concretely, the agent subprocess is spawned with:

- An **allowlisted** environment — `PATH`, `HOME` (redirected), `LANG`, `TERM`, plus the agent's own auth variable and nothing else. Not a denylist. A denylist misses the next variable name.
- `GIT_TERMINAL_PROMPT=0`, and `GIT_ASKPASS` pointed at a binary that always exits non-zero.
- No `~/.netrc`, no SSH agent socket (`SSH_AUTH_SOCK` unset), no `GH_TOKEN`/`GITHUB_TOKEN`, no cloud provider variables, no `DATABASE_URL`.
- A per-run `HOME` so `~/.aws`, `~/.config/gh`, `~/.kube` and friends are simply not there.
- **And the run clone has no `origin`** (§11.2) — so there is nothing to push *to*, before we even discuss credentials.

Note what this converts: `git.push`, `deployment.execute`, `release.publish`, `billing.spend`, and most of `credential.access` move from *detect* to *prevent* — not because a rule forbids them but because the capability is absent.

### 7.4 Agent permission hooks — a real mechanism, with measured evidence and known bypasses

**Evidence, observed directly in this session:** a `PreToolUse` hook installed by a third-party plugin intercepted and **denied** a `WebFetch` call, returning a message that redirected the agent to a different tool. Separately, a `PreToolUse:Bash` hook injected advisory context into a Bash call. Claude Code's hook mechanism therefore demonstrably (a) blocks tool calls before execution and (b) returns structured feedback to the model.

**How Conductor should use it (verified against `claude --help`, v2.1.228):**

- `--settings <file-or-json>` — Conductor generates a per-run settings file containing its own `PreToolUse` hook pointing at a Conductor hook binary.
- `--setting-sources <user,project,local>` — Conductor restricts which ambient sources load, so runs are hermetic and do not inherit the user's personal hooks.
- **Do not use `--bare`** for real runs: it skips hook loading entirely, which would disable the interception. `--bare` is for the fake-agent harness only.
- `--include-hook-events` (requires `--output-format=stream-json`) — hook lifecycle events are emitted into the stream, so **Conductor gets a durable record of every interception**, which is exactly the audit trail policy decisions need.
- `--permission-mode` accepts `acceptEdits | auto | bypassPermissions | manual | dontAsk | plan`. Use `dontAsk` with an explicit `--allowedTools` list. Never `bypassPermissions`.
- **`--permission-prompt-tool` does not exist in 2.1.228** — I checked the help output and it is absent. Do not design around it. The in-process `canUseTool` callback is Agent-SDK-only, and §5.2 explains why Conductor should not be in-process anyway.

**Known bypasses, to be stated in `SECURITY.md` rather than discovered later.** A `Bash(git push*)` deny pattern is defeated by `sh -c 'git push'`, by a script written then executed, by `git -c ... push`, by an alias, or by any spelling the matcher did not anticipate. **Hooks are a speed bump, not a boundary.** They are worth installing — they turn accidents into blocked calls and produce an audit trail — but the security argument must rest on §7.3 and §11.2.

### 7.5 OS sandbox — Codex has one, Claude does not

Verified against `codex exec --help` (codex-cli 0.142.0): `-s, --sandbox <SANDBOX_MODE>` with `[possible values: read-only, workspace-write, danger-full-access]`, described as *"the sandbox policy to use when executing model-generated shell commands."* Plus `--add-dir`, `-C/--cd`, `--ephemeral`, `--ignore-user-config`, `--ignore-rules`.

Claude Code 2.1.228's help contains no sandbox flag; the word appears only in the description of `--dangerously-skip-permissions` ("Recommended only for sandboxes with no internet access") — i.e. as advice about where to run it, not as a feature it provides.

**This asymmetry is the main reason Codex should be the first real adapter** (§12.7).

**What must be measured, not assumed** — pre-registered in Slice 0 (§18):
- Does `workspace-write` actually deny a write to `$HOME` outside the workspace? (Attempt it; record the result.)
- Does `workspace-write` deny outbound network by default? (Attempt a fetch; record.)
- Does it apply to processes the agent spawns, or only to the agent's own shell invocations?

Claiming containment we have not observed is exactly the "false security claim" the prompt forbids.

### 7.6 v1 limitations — stated plainly

1. **Conductor cannot prevent a Claude-driven agent from writing anywhere the user can write.** No sandbox, no container runtime. Detection only, via §7.7.
2. **Conductor cannot prevent network egress** for any adapter today (pending the Codex measurement).
3. **Conductor cannot prevent an agent from reading any file the user can read.** Secret files outside the workspace are exposed to a Claude-driven run. Mitigation is credential absence in `HOME`, not confinement.
4. **Conductor cannot distinguish a human at the socket from a process that has reached the socket.** §7.1 minimizes the paths, and that is all.
5. **Conductor cannot undo an external side effect** — a deploy, a sent email, a dropped table. It can only refuse to be the one that performs it, and detect afterwards.
6. **Prompt injection from repository content is unmitigated.** An agent reading a malicious `README` may act on it. Nerve labels repository-derived spans as untrusted in MCP responses; Conductor should adopt the same labelling in its packets, and should say plainly that labelling is mitigation, not prevention.

### 7.7 Post-run audit — the one control that always applies

After **every** attempt, before any state advances, and using the pre-run baseline:

`git status --porcelain=v2 --branch` · staged and unstaged diffs · untracked files · new commits and their parents · **`git config --list --local` diffed against baseline** · **`git remote -v` diffed** · **all refs diffed** · reflog · stash list · dependency manifest diffs · lockfile diffs · migration globs · files outside declared scope · secret-pattern scan over the whole diff · hook directory contents · submodule state.

Any unexplained delta raises a `Finding` and moves the task to `AWAITING_REVIEW`. **Findings never auto-resolve.**

---

## 8. Persistence and Data Model

### 8.1 The split that matters: repository-tracked truth vs. local execution state

This is a structural change from the baseline, and §1.3 is the evidence for it.

The user's plans, decisions and reports are **already files in a git repository**. They are reviewable, diffable, survive machine loss, and travel with the project. Moving them into a local SQLite database would make Conductor's most valuable data less durable and less inspectable than it is today. That would be a regression delivered as a feature.

**Therefore:**

```
<repo>/.conductor/                     ← committed, human-editable, reviewable in PRs
├── project.yaml                       ← identity, agent, paths, review cadence
├── policy.yaml                        ← project policy
├── verification.yaml                  ← check profiles
└── plan/
    ├── v3/                            ← immutable once APPROVED
    │   ├── plan.yaml                  ← milestones, slices, tasks, criteria
    │   └── APPROVED                   ← approval marker: hash, approver, timestamp
    └── decisions/
        └── D-0007-clone-not-worktree.md
```

```
~/.local/share/conductor/              ← local, disposable, never committed
├── conductor.db                       ← EXECUTION state only
├── workspaces/<run-id>/               ← per-run clones
└── artifacts/<run-id>/                ← packets, reports, logs, diffs
```

```
~/.config/conductor/                   ← global, machine-local
├── config.yaml
├── agents.yaml
└── policy/global.yaml                 ← includes locked rules
```

**Invariant: deleting `conductor.db` must lose only execution history, never a plan or a decision.** That is a testable acceptance criterion and it should be one.

SQLite's role becomes: index over repo-tracked plan files (keyed by content hash), plus execution state that has no business in a repository.

### 8.2 Journal vs. state — the modification to the baseline

Keep an append-only table. Change what it means.

**`event` (append-only, immutable) records observations about the world:**
`RUN_CREATED`, `WORKSPACE_CREATED`, `BASELINE_CAPTURED`, `AGENT_SPAWNED(pid, session_id)`, `AGENT_EXITED(code, signal)`, `LEASE_EXPIRED`, `GIT_OBSERVED(sha, dirty)`, `VERIFICATION_STARTED/FINISHED(check, code)`, `POLICY_EVALUATED(hash, action, effect)`, `APPROVAL_REQUESTED/GRANTED/DENIED/EXPIRED`, `SIDE_EFFECT_INTENDED/CONFIRMED`, `FINDING_RAISED`, `RECONCILED(verdict)`.

**Everything else is ordinary mutable rows** updated in the same transaction as the event that justifies them.

**Why not event-sourcing.** Three concrete reasons:

1. **The source of truth is not in the database.** It is Git and the filesystem. A projection is a claim about the world derived from a log of past claims; the reconciler must go look anyway. Event-sourcing here creates a second, plausible-looking source of truth that will eventually be trusted over the repository — which is the exact failure mode this product exists to prevent.
2. **Schema evolution.** Replaying a two-month-old event stream through today's projection code is a real cost, paid every migration, for auditability that the append-only `event` table already provides.
3. **Projection bugs are silent.** A wrong `UPDATE` is visible in a test. A projection that drifts from its log is visible only when someone asks the right question.

**Verdict: journal as evidence — KEEP. Materialized projections — REPLACE with plain state.**

### 8.3 Entities

Kept, with purpose, key fields, and the invariants that matter.

**`project`** — a registered repository. `id`, `root_path`, `repo_identity` (hash of the initial commit + normalized origin), `default_branch`, `config_hash`. *Invariant: `root_path` must resolve to a git repository whose `repo_identity` matches, or every command refuses.* This survives the directory being moved or a different repo being placed at the path.

**`plan_version`** — index over `.conductor/plan/vN/`. `id`, `project_id`, `version`, `content_hash`, `state`, `approved_at`, `approved_by`, `source_path`. *Invariant: `APPROVED` ⇒ `content_hash` matches the file on disk; a mismatch is a hard error, not a resync.*

**`decision`** — `id`, `plan_version_id`, `status` (`OPEN|ACCEPTED|REJECTED|SUPERSEDED`), `supersedes`, `content_hash`, `source_path`. Append-only; supersession never deletes. *This is Nerve's memory model and should be built by reading `slice-14`, whose first draft was refuted twice in that document before it was implemented.*

**`task`** — the unit of agent work. `id`, `plan_version_id`, `slice_id`, `state`, `acceptance_criteria_ref`, `scope_globs`, `verification_profile`, `attempt_budget`. *Invariant: a task belongs to exactly one plan version; a new plan version creates new tasks, and old tasks are `SUPERSEDED`, never mutated.*

**`run`** — one execution of one task under one policy. `id`, `task_id`, `policy_hash`, `workspace_id`, `base_commit`, `run_branch`, `state`, `lease_expires_at`, `lease_owner`. *Invariant: at most one `run` per task in a non-terminal state.*

**`attempt`** — one agent process. `id`, `run_id`, `ordinal`, `kind` (`IMPLEMENT|REPAIR|CONTINUE`), `adapter`, `agent_session_id`, `pid`, `started_at`, `exit_code`, `signal`, `outcome`. *Invariant: `agent_session_id` is assigned by Conductor before spawn where the adapter allows it (Claude's `--session-id <uuid>` does), so a session is findable even if the process dies before announcing itself.*

**`workspace`** — `id`, `run_id`, `path`, `kind` (`CLONE`), `source_repo`, `created_at`, `removed_at`. *Invariant: never removed while its run is non-terminal; orphans detected at startup.*

**`verification_run` / `verification_check`** — `id`, `run_id`, `attempt_id`, **`tree_hash`**, `commit_sha`, `toolchain_fingerprint`, `check_id`, `command_hash`, `exit_code`, `duration_ms`, `outcome` (`PASS|FAIL|INCONCLUSIVE|VOID`), `log_path`. *Invariant: a result is valid only for the `tree_hash` it observed. Unique index on `(tree_hash, check_id, command_hash, toolchain_fingerprint)` gives free caching and makes "have we already verified this exact tree?" a lookup.*

**`policy_snapshot`** — `hash`, `canonical_blob`, `created_at`. Content-addressed, deduplicated.

**`approval_request` / `approval_grant`** — §15.

**`side_effect`** — the idempotency ledger. `operation_id` (deterministic, unique), `kind`, `state` (`INTENDED|CONFIRMED|FAILED|AMBIGUOUS`), `precondition`, `receipt`, `observed_at`. §10.7.

**`finding`** — audit deltas. `id`, `run_id`, `kind`, `severity`, `evidence_ref`, `resolution`. Never auto-resolves.

**`artifact`** — `id`, `run_id`, `kind`, `path`, `sha256`. Content-addressed; packets are artifacts.

### 8.4 Entities I removed, and why

| Removed | Reason |
|---|---|
| `Milestone`, `Slice` as tables | Structure lives in `plan.yaml`; status is **computed** from child tasks. The prompt's own §23 suspects this. Storing an aggregate status invites it to disagree with its children. |
| `Checkpoint` | Conflates two things. Agent-internal progress is not observable and must not be trusted; Conductor-internal progress is already an `event`. Nothing left. |
| `Continuation` | A continuation packet is an `artifact`. It needs no table. |
| `Failure` | `attempt.outcome` + `finding` + verification results cover it. A separate table would duplicate all three. |
| `ReviewPacket` | An `artifact` with `kind='review_packet'`. |
| `PolicyRule` as rows | Rules live in YAML and are hashed as a snapshot. Normalizing them into rows buys query power nobody needs and creates a second editing surface. |
| `Event` as domain source of truth | Demoted to evidence (§8.2). |

### 8.5 Indexes and transactions

Indexes: `run(state, lease_expires_at)` — the claim query; `task(plan_version_id, state)`; `event(run_id, seq)`; `verification_check(tree_hash, check_id)`; unique `side_effect(operation_id)`; unique partial index enforcing one non-terminal run per task.

Transactions: **every write uses `BEGIN IMMEDIATE`**. SQLite's deferred transactions take a read lock first and upgrade, which produces `SQLITE_BUSY` on upgrade that no `busy_timeout` will resolve. Immediate takes the write lock up front. WAL mode, `synchronous=FULL` (not `NORMAL` — this database is the recovery record, and a lost commit on power failure is exactly the case it exists for), `foreign_keys=ON`, `busy_timeout=5000`.

---

## 9. State Machines

Simplified from the baseline. Every state must have a distinct *operator action*; states that differ only in internal bookkeeping are collapsed.

### 9.1 Plan

```
DRAFT ──validate──► VALIDATED ──request──► AWAITING_APPROVAL ──human──► APPROVED
  │                    │                          │                        │
  └──────────◄─────────┴──────────◄───────────────┘                   SUPERSEDED
         (validation failure returns to DRAFT)                   (by a later APPROVED)
```

Dropped `IMPORTED` (indistinguishable from `DRAFT`) and `REJECTED` (a rejected plan is a `DRAFT` again; a terminal `REJECTED` state is a graveyard nobody queries).

- **Authority:** `APPROVED` only via a human at the control socket. Never an agent, never automatically.
- **Evidence:** `content_hash` of `plan.yaml`, plus validation report.
- **Invalid:** `DRAFT → APPROVED`. `APPROVED → *` except `SUPERSEDED`.
- **Terminal:** `SUPERSEDED`.
- **Restart:** re-hash on load; mismatch on an `APPROVED` plan is a hard error requiring `conductor plan reapprove`.

### 9.2 Task

```
PENDING ──deps met──► READY ──claim──► RUNNING
                                          │
                                          ▼
                                     RECONCILING  ◄──────────┐
                                          │                  │
                    ┌─────────────────────┼──────────┐       │
                    ▼                     ▼          ▼       │
              AWAITING_APPROVAL      VERIFYING    BLOCKED    │
                    │                     │                  │
                 (granted)          ┌─────┴─────┐            │
                    │               ▼           ▼            │
                    └────────► COMPLETE     REPAIRING ───────┘
                                   ▲            │
                                   │       (budget spent)
                                   │            ▼
                              (accepted)   AWAITING_REVIEW
                                                │
                                    accept / repair / revise / stop
```

Terminal: `COMPLETE`, `CANCELLED`, `SUPERSEDED`.

Changes from the baseline:

- **`COMMITTING` removed as a state.** A commit is a Conductor-owned side effect inside the `RECONCILING → COMPLETE` transaction, protected by the `side_effect` ledger. A state whose only purpose is "we are mid-side-effect" is exactly what the idempotency ledger replaces, and having both means two mechanisms for one problem.
- **`FAILED` removed.** In practice everything routes to `AWAITING_REVIEW` (human decides) or `BLOCKED` (dependency/environment). A terminal `FAILED` invites abandoning tasks without a decision record.
- **`ABANDONED` removed.** That is `CANCELLED` with a reason field.
- **`RECONCILING` is mandatory and unskippable.** Every exit from `RUNNING` — success, crash, timeout, cancel — passes through it. This is the invariant that makes agent self-report non-authoritative, and it should be enforced in the type system, not by convention.

**Authority:** Conductor drives all transitions except `AWAITING_APPROVAL →` (human), `AWAITING_REVIEW →` (human), `→ CANCELLED` (human).
**Evidence required:** `RUNNING → RECONCILING` requires a terminal `attempt`. `VERIFYING → COMPLETE` requires all required checks `PASS` **at the current tree hash** and zero unresolved findings.
**Invalid:** `RUNNING → COMPLETE` (must reconcile). `* → COMPLETE` without a verification run bound to the final tree hash.
**Restart:** any task in `RUNNING`, `RECONCILING` or `VERIFYING` at startup whose lease expired → forced into `RECONCILING`.

### 9.3 Attempt

```
CREATED ──► STARTING ──► ACTIVE ──┬──► EXITED    ──┐
                            │      ├──► CRASHED   ──┤
                            │      ├──► TIMED_OUT ──┼──► RECONCILED (terminal)
                            └──────┴──► STALE     ──┘
                                    (lease lost)
```

`STALE` = heartbeat lapsed and liveness could not be confirmed. It is deliberately distinct from `CRASHED`: `CRASHED` means we observed a nonzero exit; `STALE` means **we do not know**, and unknown must not be recorded as known. **Every path ends at `RECONCILED`** — an attempt is never finished until Conductor has looked at the repository.

### 9.4 Approval

```
REQUESTED ──human grant──► GRANTED ──consumed──► CONSUMED (terminal)
    │                          │
    ├──human deny──► DENIED    ├──ttl──► EXPIRED
    └──ttl──► EXPIRED          └──human──► REVOKED
```

`GRANTED → CONSUMED` is one-shot by default. Reusable grants exist but require an explicit `--reuse` and always carry a TTL.

### 9.5 Review

```
PENDING ──export──► EXPORTED ──import──► DECIDED
                                            │
              accept · repair · revise_plan · pause · stop
```

`revise_plan` creates a new `plan_version` in `DRAFT` and supersedes affected tasks.

### 9.6 Run

Deliberately thin: `run` state mirrors its task and exists to hold the lease and the policy snapshot. **I considered giving `run` an independent state machine and rejected it** — two state machines over the same lifecycle is two things to keep in agreement, and one of them will drift.

---

## 10. Durable Runtime Semantics

### 10.1 Job lifecycle

There is no separate job entity. **A `run` in a claimable state is the job.** Adding a `job` table over `run` would be the first step toward a generic scheduler (§25).

### 10.2 Claim — one atomic statement

```sql
BEGIN IMMEDIATE;
UPDATE run
   SET state='RUNNING',
       lease_owner=?1,
       lease_expires_at=?2,
       lease_epoch = lease_epoch + 1
 WHERE id = (SELECT id FROM run
              WHERE state IN ('READY','RECOVERING')
                AND (lease_expires_at IS NULL OR lease_expires_at < ?3)
              ORDER BY priority, created_at
              LIMIT 1)
RETURNING id, task_id, policy_hash, lease_epoch;
-- same transaction:
INSERT INTO event(run_id, seq, kind, payload) VALUES (…,'RUN_CLAIMED',…);
COMMIT;
```

Claim, lease assignment, and the event are one transaction. This is the transaction boundary the prompt asks to be named. **`lease_epoch` is the fencing token**: every subsequent write by that worker carries its epoch and is rejected if the epoch has moved. Without fencing, a process that stalls past its lease and then wakes up will happily write over its successor's work.

### 10.3 Leases and heartbeats

Lease 60 s; heartbeat every 15 s; **the heartbeat is conditional on the agent process still existing** (`kill(pid, 0)`), not on the supervisor's own liveness. A supervisor that heartbeats while its child is dead is worse than one that crashes.

### 10.4 Retries

Retries are **attempts within a run**, bounded by `attempt_budget`, not queue-level redelivery. Distinguish:
- **Infrastructure retry** (spawn failed, adapter binary missing, auth expired): retried with backoff, does not consume attempt budget.
- **Work retry** (agent failed to satisfy criteria): consumes budget, requires a repair packet.

Conflating these is how a broken API key silently exhausts a task's budget.

### 10.5 Timers and waits

Two only: lease expiry and approval TTL. Both are `WHERE … < now()` scans on a 5-second tick. **No timer service.** If a third timer need appears, look at it hard — that is where a scheduler starts growing.

### 10.6 Startup recovery

```
1. Open DB; run migrations; integrity_check.
2. Find runs in RUNNING/RECONCILING/VERIFYING with expired leases.
3. For each: probe the recorded pid → alive & matching start-time?
      alive   → adopt or terminate (config), record.
      dead    → attempt := STALE.
4. Locate the workspace. Absent → run BLOCKED with a finding.
5. Capture current git state; diff against the stored baseline.
6. Re-run verification only if the tree hash has no cached valid result.
7. Classify (§10.8) and route.
8. Scan for orphaned workspaces with no live run → quarantine, never delete.
9. Expire overdue approvals; restore AWAITING_APPROVAL waits.
```

"Never delete" in step 8 is deliberate: an orphaned workspace may hold the only copy of an hour of work. Quarantine and report.

### 10.7 Idempotency and side effects

Every Conductor-owned side effect is: **intent → precondition check → act → receipt**, with a deterministic operation ID.

```
operation_id = blake3(kind || run_id || attempt_ordinal || tree_hash)
```

```
BEGIN IMMEDIATE;
  INSERT INTO side_effect(operation_id, kind, state, precondition) VALUES (…,'INTENDED', …);
  -- precondition e.g. "HEAD of conductor/T-12/r-3 == <sha>"
COMMIT;

  perform the effect                     ← crash window

BEGIN IMMEDIATE;
  UPDATE side_effect SET state='CONFIRMED', receipt=? WHERE operation_id=?;
COMMIT;
```

On restart, any `INTENDED` row is resolved by **re-checking the precondition against the world**, not by retrying blindly:

| Kind | Did it happen? |
|---|---|
| `git.commit.local` | does a commit with this tree and message exist on the run branch? |
| `git.fetch_into_main` | does the target ref point at the expected sha? |
| `workspace.create` | does the path exist with the expected `HEAD`? |
| `artifact.write` | does the file exist with the expected sha256? |

**Every Conductor-owned effect must be designed to be checkable this way. An effect that cannot be checked cannot be owned by Conductor** — it must be a human action instead. This is the rule that keeps deployment out of v1 automation, and it should be stated as a design constraint rather than as a scope decision.

**Ambiguous** (precondition indeterminate): mark `AMBIGUOUS`, halt the run, raise a finding, require a human. Never guess. This directly answers Scenario J.

### 10.8 Reconciliation verdicts

`reconcile()` is a pure function of (baseline, observed, report?, verification?) returning exactly one of:

| Verdict | Meaning | Route |
|---|---|---|
| `NO_CHANGE` | tree identical to baseline | attempt failed to act → repair or review |
| `CLEAN_COMPLETE` | changes present, in scope, report present and consistent | → VERIFYING |
| `CLEAN_NO_REPORT` | changes present and in scope, no report | → VERIFYING (report is not required for correctness) |
| `OUT_OF_SCOPE` | changes outside declared scope | finding → AWAITING_REVIEW |
| `POLICY_SENSITIVE` | deps / lockfile / migrations / git config touched | policy evaluation → approval or review |
| `CONTRADICTED` | report contradicts observed state | finding; **git wins**; → AWAITING_REVIEW |
| `CORRUPT` | repo in a broken state (merge in progress, detached, index lock) | → BLOCKED |

### 10.9 The actual execution guarantee — stated precisely

The prompt is right to forbid casual exactly-once claims. Three tiers:

1. **Agent attempts: at-least-once.** An attempt may be started more than once for the same task across crashes. This is safe because attempts are isolated in a per-run clone and their effects are reconciled, not assumed.
2. **Conductor-owned side effects: at-most-once, and effectively exactly-once when the precondition is checkable.** Guaranteed by §10.7. Where the precondition is indeterminate, the guarantee degrades explicitly to *"halted and reported,"* never to *"retried."*
3. **Agent-caused external side effects: no guarantee whatsoever.** If an agent deploys something, Conductor may detect it afterwards and can offer no exactly-once semantics. §7.3 exists to make this class empty by removing the capability, not to manage it.

**Conductor must never print or document the phrase "exactly-once" without the qualifier in tier 2.**

---

## 11. Git and Workspace Architecture

### 11.1 Registration and baseline

`conductor project add <path>` records `root_path`, `repo_identity`, `default_branch`, and refuses if the path is not a git repository, is a bare repository, or is itself inside another registered project's workspace directory.

Baseline captured at run creation: `base_commit`, tree hash, `git status --porcelain=v2`, untracked list, **`git config --list --local`**, **`git remote -v`**, all refs, submodule status, stash count, hook directory listing.

### 11.2 Worktree → per-run local clone: my strongest disagreement

**The claim in the baseline:** a dedicated git worktree per run provides isolation.

**Why it is wrong.** `git worktree` gives each worktree its own `HEAD`, index and working tree. It **shares** the repository: `.git/config`, all refs, the object store, hooks, and the reflog. Therefore, inside a worktree, an agent that runs:

| Command | Effect |
|---|---|
| `git remote set-url origin <attacker>` | **mutates the user's real repository config** |
| `git branch -D main` | **deletes a ref in the user's real repository** |
| `git config user.email …` | **mutates shared local config** |
| `git gc --prune=now` / `git reflog expire` | **operates on the shared object store** |
| writing `.git/hooks/pre-commit` | **installs a hook the user will execute later** |

That last one is the worst: a hook written during a run persists and fires the next time the *human* commits.

A worktree is **file isolation, not repository isolation**, and prompt §11 asks precisely the questions ("Can it be prevented?" for a remote change, Scenario H) that this answers — the baseline's answer under a worktree model has to be "no, only detected."

**Recommendation: one local clone per run.**

```bash
git clone --no-checkout "$REPO" "$WS"       # local path ⇒ objects hardlinked; fast, disk-cheap
git -C "$WS" checkout -b "conductor/$TASK/$RUN" "$BASE_COMMIT"
git -C "$WS" remote remove origin           # ← nothing to push to
git -C "$WS" config user.name  "Conductor Agent"
git -C "$WS" config user.email "conductor@localhost"
git -C "$WS" config commit.gpgsign false
git -C "$WS" config core.hooksPath /dev/null
```

What this buys, mapped to the threat model:

| Property | Worktree | Local clone |
|---|---|---|
| File isolation | yes | yes |
| **Config isolation** | **no** | **yes** |
| **Ref isolation** | **no** | **yes** |
| **Hook isolation** | **no** | **yes** |
| **Remote reachable by agent** | **yes** | **no — removed** |
| Object store | shared | hardlinked, but writes are new objects |
| Disk cost | lowest | near-lowest (hardlinks) |
| Create/destroy cost | ~ms | ~tens of ms on a local repo |
| Blast radius of `git gc` in the run | **user's repo** | run only |

**Tradeoffs I accept and state:**
- Integration is now a **fetch**, not a checkout: `git -C "$REPO" fetch "$WS" "conductor/…:conductor/…"`. Conductor performs it. The agent cannot.
- Hardlinked objects mean an aggressive `git gc` in the *source* repo while a clone exists can, in principle, disturb shared objects. Mitigation: Conductor never runs `gc`, and clones are short-lived. If this proves fragile, `--no-hardlinks` costs disk and removes the concern — a measurable tradeoff, not a guess.
- Submodules need explicit handling (`--recurse-submodules` or refusal). See §11.8.
- Very large repositories make cloning more expensive than a worktree. **Threshold to measure in Slice 0:** if `git clone --local` of the target repo exceeds ~2 s, offer worktrees as an opt-in `isolation: worktree` with the config/ref/hook exposure documented at the point of configuration.

### 11.3 Branch model

`conductor/<task-id>/<run-id>`, created from `base_commit`, existing only in the run clone until Conductor fetches it. Never pushed. Never merged automatically into the default branch — integration into `main` is a Conductor-owned effect gated by policy, and by default it is a human action.

### 11.4 Dirty user checkout, and the target branch moving

**Dirty checkout at run start:** proceed. Because the run is a clone from `base_commit`, uncommitted user work is neither copied nor endangered. This is strictly better than the worktree model, where `git worktree add` succeeds but the user's index and stash are shared. Conductor records the dirty state in the baseline so that a later finding can distinguish "user had this modified before" from "the agent did it."

**Target branch moves mid-run (Scenario K):** the run is unaffected — it is a clone at a fixed commit. At integration, Conductor re-reads the target ref; if it moved, the run enters `AWAITING_REVIEW` with the divergence attached. **Conductor never rebases or merges automatically.** Automatic conflict resolution is an unbounded correctness risk for a system whose thesis is that it does not guess.

### 11.5 Reconciliation surface

As §7.7. The additions over the baseline are `.git/config`, remotes, refs, hooks and stash — precisely the surface a worktree would have shared, which is why the baseline did not think to check them.

### 11.6 Commit authority

Agents may commit **inside the run clone** — that is safe and useful, and it makes their intermediate work recoverable after a crash. Conductor performs: the final squash/annotation commit if configured, the fetch into the real repository, and nothing else. **Conductor never pushes to a remote in v1.** `git.push` exists in the policy taxonomy so that its attempted use is detected, not so that Conductor can perform it.

### 11.7 Cleanup and abandonment

Workspaces are retained until the run reaches a terminal state **and** its artifacts are captured, then retained for a configurable `keep_workspaces_days` (default 7). Orphans found at startup are **quarantined** (moved under `workspaces/quarantine/`) and reported, never deleted. `conductor workspace list` shows them with age and disk.

### 11.8 Submodules and nested repositories

**v1 refuses.** If `git submodule status` is non-empty at registration, `conductor project add` errors with an explicit message. Rationale: submodule pointer updates are a whole category of side effect with its own reconciliation semantics, and shipping a half-correct version is worse than refusing. Nested repositories inside the tree are detected at baseline and flagged; files under them are excluded from scope checks and raise a finding if modified.

Refusing loudly is the pattern Nerve already uses for `nerve affected` and `nerve trace-tests`, and it is the right one.

### 11.9 Recovery after reboot

Workspaces are on disk and self-describing: each contains `.conductor-run.json` with `run_id`, `task_id`, `base_commit`, `policy_hash`, `created_at`. **Even with a lost database, `conductor recover --scan` can enumerate workspaces, read their descriptors, inspect their git state, and rebuild a run inventory.** That property is worth the file.

---

## 12. Agent Adapter Architecture

### 12.1 Interface — narrower than the baseline's

The baseline's interface has `streamEvents`, `inspect`, `interrupt`, `terminate`, `resume`. That is a session-management API for agents that expose sessions as first-class objects. Both real adapters today are **process launchers that write JSONL to stdout**. The interface should say that.

```rust
trait AgentAdapter {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;

    /// Build the command. Does NOT spawn — Conductor owns the process.
    fn command(&self, input: &StartInput) -> Result<AgentCommand>;

    /// Parse one line of the agent's stdout into a normalized event.
    fn parse_event(&self, line: &str) -> Result<Option<AgentEvent>>;

    /// Extract the final report from the run's outputs.
    fn extract_report(&self, out: &RunOutputs) -> Result<Option<AgentReport>>;

    /// Map process exit into a normalized outcome.
    fn classify_exit(&self, code: Option<i32>, sig: Option<i32>) -> AttemptOutcome;

    /// Build a resume command, if supported.
    fn resume_command(&self, input: &ResumeInput) -> Option<AgentCommand>;
}
```

**Conductor owns spawning, killing, timeouts and streaming.** Adapters are pure translation: build argv+env, parse lines, classify exits. This makes every adapter testable against recorded JSONL fixtures with no process at all — which is what you want, because agent output is the least stable thing in the system.

### 12.2 Capabilities — only what is verified

```rust
struct Capabilities {
    conductor_assigned_session_id: bool,  // Claude: true (--session-id <uuid>)
    session_resume:                bool,  // both: true
    schema_enforced_final_output:  bool,  // both: true
    streaming_events:              bool,  // both: true (JSONL)
    os_sandbox:                    bool,  // Codex: true; Claude: false
    tool_interception_hooks:       bool,  // Claude: true; Codex: hooks exist, trust model unverified
    hermetic_config:               bool,  // Codex --ignore-user-config; Claude --setting-sources
    spend_cap:                     bool,  // Claude: --max-budget-usd; Codex: not observed
    working_dir_flag:              bool,  // Codex -C/--cd; Claude: cwd
}
```

`sandboxSelection` and `runtimeControl` from the baseline are dropped — I could not verify what they would mean for either adapter, and inventing capabilities is what the prompt forbids.

### 12.3 Claude Code — verified against `claude --help`, v2.1.228

| Need | Flag | Note |
|---|---|---|
| Non-interactive | `-p` / `--print` | |
| Event stream | `--output-format stream-json` | plus `--include-partial-messages` |
| **Hook audit trail** | `--include-hook-events` | requires stream-json; gives a durable interception record |
| Structured report | `--output-format json --json-schema <schema>` | lands in `structured_output` |
| **Session identity** | `--session-id <uuid>` | **Conductor assigns it before spawn** |
| Resume | `--resume <id>`, `--continue`, `--fork-session` | |
| Tool restriction | `--allowedTools`, `--disallowedTools` | |
| Permission baseline | `--permission-mode dontAsk` | never `bypassPermissions` |
| Hermeticity | `--setting-sources`, `--settings <file>` | Conductor injects its own hooks here |
| Spend cap | `--max-budget-usd <amount>` | maps to `billing.spend` policy |
| Scope | `--add-dir` | |

**Not available:** `--permission-prompt-tool` (absent from help), OS sandbox.
**Do not use `--bare` for real runs** — it skips hook loading and would disable interception.

`--session-id` deserves emphasis: because Conductor chooses the UUID, it can locate the session even if the process dies before emitting `system/init`. That is a genuine recovery advantage and it should be used.

### 12.4 Codex — verified against `codex exec --help`, codex-cli 0.142.0

| Need | Flag |
|---|---|
| Non-interactive | `codex exec [PROMPT]` (or stdin) |
| Event stream | `--json` (JSONL: `thread.started`, `turn.*`, `item.*`, `error`) |
| Structured report | `--output-schema <FILE>` + `-o/--output-last-message <FILE>` |
| Resume | `codex exec resume <SESSION_ID>` / `--last` |
| **OS sandbox** | `-s/--sandbox read-only\|workspace-write\|danger-full-access` |
| Working root | `-C/--cd <DIR>`, `--add-dir <DIR>` |
| Hermeticity | `--ignore-user-config`, `--ignore-rules`, `--ephemeral` |
| Model | `-m/--model` |

Session identity arrives in `thread.started` — Conductor cannot pre-assign it, so a crash before the first line leaves an unknown session. Mitigation: the run clone is the evidence; a lost session id costs the resume optimization, not correctness. This is the Recovery Principle doing its job.

### 12.5 Failure classification

| Signal | Outcome |
|---|---|
| exit 0 + report present + report parses | `EXITED` |
| exit 0, no report | `EXITED` (report is optional; reconciliation is authoritative) |
| exit ≠ 0 | `CRASHED` |
| `SIGKILL`/`SIGSEGV` | `CRASHED` |
| wall-clock budget exceeded | `TIMED_OUT` (Conductor kills: `SIGTERM`, grace, `SIGKILL`) |
| no output for `idle_timeout` | `TIMED_OUT` with `reason=stall` |
| process gone, no exit observed | `STALE` |
| auth/rate-limit error event | `CRASHED` with `kind=infrastructure` → infra retry, no budget consumed |

### 12.6 Permission integration

Claude: Conductor writes a per-run settings JSON containing a `PreToolUse` hook that invokes `conductor hook` — which evaluates the run's **policy snapshot** and returns allow/deny. This is the only place a policy decision reaches inside an agent. `--include-hook-events` makes every decision land in the event stream. §7.4's bypass caveats apply and must be documented at the same place in the code.

Codex: rely on `--sandbox workspace-write` for containment. Codex's own hook mechanism (implied by `--dangerously-bypass-hook-trust`) was not investigated deeply enough to design against; treat it as an S16+ opportunity, not a v1 dependency.

### 12.7 First real adapter: **Codex**

Ranked against the prompt's own criteria:

| Criterion | Codex | Claude Code |
|---|---|---|
| Noninteractive execution | `codex exec` | `claude -p` |
| Structured output | `--output-schema` (file) | `--json-schema` (inline) |
| Resumability | `resume <id>` / `--last` | `--resume`, **+ assignable `--session-id`** |
| Observability | JSONL | stream-json **+ hook events** |
| **OS containment** | **`--sandbox workspace-write`** | **none** |
| Permission hooks | trust model unverified | **`PreToolUse`, demonstrated working** |
| Hermetic config | `--ignore-user-config`, `--ignore-rules`, `--ephemeral` | `--setting-sources`, `--settings` |
| Spend cap | not observed | `--max-budget-usd` |

**Choose Codex first**, for one dominant reason: on a machine with no container runtime, `--sandbox workspace-write` is the **only** mechanism that can actually prevent writes outside the workspace. Building the first adapter against the agent that offers real containment means the enforcement layer (§7) is exercised from the beginning rather than stubbed. Its `--output-schema` also makes the report contract *enforced by the agent runtime* rather than hoped for and validated afterward.

Claude Code is a close second and should be S16, once the policy engine exists — because its distinguishing capability is *interception*, which is meaningless until there is a policy to intercept against. The ordering follows from the capability, not from preference.

**Caveat to measure before committing** (Slice 0): confirm empirically that `workspace-write` denies a write to `$HOME` and denies network egress. If it does not, the reason for Codex-first evaporates and the order should flip. That is a pre-registered falsification, in the style this repository's sibling already uses.

---

## 13. Prompt and Artifact Schemas

### 13.1 Principle

Every packet is **generated from durable state, is content-hashed, and is stored as an artifact.** No packet is assembled from conversation history. A packet that cannot be regenerated from the database plus the repository is a bug, because it means state exists somewhere Conductor cannot recover it from.

### 13.2 Implementation packet

```yaml
packet: implementation
packet_version: 1
run_id: r-0041
task_id: T-0012
plan_version: 3
plan_hash: blake3:9ac2…
policy_hash: blake3:41ef…

objective: "…"                     # one paragraph, from plan.yaml
context:
  milestone: M-02
  slice: S-05
  why_now: "…"

acceptance_criteria:               # each MUST bind to a verification check
  - id: AC-1
    statement: "…"
    verified_by: [typecheck, unit-tests]
  - id: AC-2
    statement: "…"
    verified_by: [migration-validate]

scope:
  allowed_globs: ["src/policy/**", "tests/policy/**"]
  forbidden_globs: [".conductor/**", "migrations/**"]

decisions:                         # ONLY those the resolver marked relevant
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
  forbidden: [git.push, git.remote.modify, deployment.execute]

evidence_links:                    # linked, never embedded
  - {kind: prior_diff, path: artifacts/r-0039/diff.patch, sha256: …}

report_schema: schemas/agent-report.v1.json
```

**Context minimization, concretely:** decisions are selected by touching the task's scope globs or its explicit `decision_refs` — not "all accepted decisions." Prior diffs are linked by path and hash. Verification logs are never embedded; failing *excerpts* are (§13.4). Target: an implementation packet under ~4 KB.

### 13.3 Agent report (schema-enforced)

```json
{
  "task_id": "T-0012",
  "status": "complete | partial | blocked",
  "summary": "≤ 500 chars",
  "files_changed": ["src/policy/eval.rs"],
  "commands_run": [{"command": "cargo test", "exit_code": 0}],
  "acceptance_criteria": [{"id": "AC-1", "claim": "met|not_met|unknown", "evidence": "…"}],
  "deviations": [{"from": "…", "reason": "…"}],
  "blockers": [],
  "unverified_claims": []
}
```

Enforced by Codex `--output-schema` / Claude `--json-schema`.

**The report is evidence, never authority.** `report.status == "complete"` with `reconcile() == NO_CHANGE` is a `CONTRADICTED` verdict and a finding. This is Scenario P and Scenario D, and the schema exists to make the contradiction machine-detectable, not to be believed.

### 13.4 Repair packet

Adds only what repair needs: the failing check IDs; the **failure fingerprint**; a bounded log excerpt (first failing assertion + 40 lines of context, never the full log); the diff of what the previous attempt changed; the attempt ordinal and remaining budget; and an explicit `do_not_retry` list of approaches already tried and failed. That last field is what stops the second attempt from being the first attempt again.

### 13.5 Continuation packet

Generated when an attempt dies. Contains everything in the implementation packet **plus observed reality**: `reconciliation_verdict`, current tree hash vs. base, the actual diff so far, which acceptance criteria already verify green at the current tree, commits made in the run clone, the partial report if any, and — explicitly — `"the previous agent's reasoning is not available; treat its intent as inferable only from the diff."*

That last line is the Recovery Principle expressed to the agent, and it is the same discipline the user applied by hand in `docs/reports/restart-recovery-report.md` when they refused to treat a dead subagent's finding as established.

### 13.6 Planning and review packets

**Planning packet** (Conductor → human → ChatGPT): vision, current plan version + diff from previous, open questions, accepted/rejected decisions with rationale, roadmap status computed from tasks, recent failures with fingerprints, policy summary, and a requested output schema so the returned plan can be validated mechanically.

**Review packet** (after a review boundary): plan version and hash; task IDs; base commit and end state; **the actual diff** (linked, with stat inline); agent claims vs. reconciliation verdict, side by side; every verification command with exit code and duration; policy evaluations and their explanations; approvals granted with scope; deviations from plan; unresolved findings; and a proposed next state.

**Review decision** (imported): `accept | repair | revise_plan | pause | stop`, plus `decisions_to_record[]`, `plan_amendments[]`, `notes`. Importing is a mutating operation and therefore goes through the control socket, not a file the agent could write (§7.1).

---

## 14. Verification and Repair

### 14.1 Profiles

The prompt's YAML (§12) is close to right. Three changes:

```yaml
verification:
  toolchain_fingerprint:              # participates in the result cache key
    - "rustc --version"
    - "cargo --version"

  required:
    - id: typecheck
      command: cargo check --all-targets
      timeout_seconds: 600
      on_timeout: inconclusive        # NOT failure
    - id: unit-tests
      command: cargo test
      timeout_seconds: 1200
      flaky_retry: 1                  # exactly one retry; disagreement ⇒ INCONCLUSIVE

  conditional:
    - when: {changed_paths: ["migrations/**"]}
      commands: [migration-validate]

  invariants:                          # cheap, always, never skipped
    - id: no-secrets
      command: conductor scan secrets
    - id: git-clean-remotes
      command: conductor scan git-invariants
```

### 14.2 Results are bound to a tree, not to a run

`(tree_hash, check_id, command_hash, toolchain_fingerprint) → outcome`.

Consequences worth naming:
- **Free caching.** Re-verifying an unchanged tree is a lookup. This matters most in repair loops, which re-run everything by default.
- **Crash-safe.** After a daemon crash, if the tree is unchanged and a valid result exists, do not re-run — that is the Scenario J answer for verification specifically.
- **A result whose tree moved under it is `VOID`, not `PASS`.** Conductor hashes the tree before and after each check; a mismatch voids the result and raises a finding. This closes a hole the baseline leaves open: an agent (or a stray watcher) that modifies files while tests run can produce a green result for a tree that never existed.

### 14.3 Outcomes — four, not two

`PASS` · `FAIL` · **`INCONCLUSIVE`** · `VOID`.

`INCONCLUSIVE` covers timeout, infrastructure error (network, missing toolchain, disk full), and flaky-retry disagreement. It is **not** a failure, because the distinction determines what happens next: `FAIL` → repair; `INCONCLUSIVE` → bounded infra retry, then human. Collapsing them is how a broken CI cache turns into three wasted agent attempts and a burned budget.

### 14.4 Logs

Full logs to `artifacts/<run>/verification/<check>-<attempt>.log`, sha256 recorded, never inlined into packets, retained per `keep_logs_days`, and **passed through the secret scanner before any of it enters a packet excerpt.**

### 14.5 Repair, with a defined progress predicate

The baseline says repair is bounded but never defines "no progress." Here it is:

```
fingerprint(failure) = blake3(
      sorted(failing_check_ids)
   || normalized(first_failing_assertion)   # paths, line numbers, addresses, timings stripped
)

progressed(prev, next) :=
      next.failing_checks ⊂ prev.failing_checks        # strictly fewer
   OR (next.fingerprint ≠ prev.fingerprint
       AND tree_hash changed)                          # different problem, real edit
```

```yaml
repair:
  max_attempts: 2
  stop_on_identical_fingerprint: true    # same fingerprint twice ⇒ stop immediately
  escalate_after: 2                      # → AWAITING_REVIEW
  new_session_on_attempt: 2              # fresh context; the stuck one is stuck
```

Three named loop-breakers, because loops have three causes:
1. **Identical fingerprint twice** → the agent is re-running into the same wall. Stop at once; do not spend attempt 2.
2. **Oscillation** — fingerprint alternates A→B→A → stop. Detected by keeping the last 4 fingerprints.
3. **Empty edit** — `NO_CHANGE` from a repair attempt → stop. The agent produced nothing.

`new_session_on_attempt: 2` is deliberate: a stuck agent's context is the problem, and resuming it re-imports the stuckness. The repair packet's `do_not_retry` list (§13.4) carries forward what matters.

### 14.6 Independent agent review — DEFER

The prompt lists "optional independent agent review" as a verification type. **Not in v1, and never gating.** Verification's entire value is that it is deterministic and cheap to trust. A nondeterministic reviewer inside it makes `COMPLETE` a probabilistic state. Post-v1 it can attach *advisory* findings to a review packet, where a human is already reading.

### 14.7 Completion criteria

A task may reach `COMPLETE` only when **all** hold:
1. Every required check `PASS` **at the current tree hash**.
2. Every conditional check triggered by the actual diff has run and passed.
3. All invariant checks pass.
4. Zero unresolved findings.
5. Every acceptance criterion binds to ≥1 passing check.
6. Reconciliation verdict ∈ {`CLEAN_COMPLETE`, `CLEAN_NO_REPORT`}.
7. Every policy-sensitive action detected has a matching, unexpired, correctly-scoped approval grant.

Note what is absent: **the agent's report is not in the list.**

---

## 15. Approval Architecture

### 15.1 The boundary first

Everything here is void without §7.1. Approval grants must be issuable **only** over the control socket, which is absent from the agent's environment and outside its filesystem scope. There is no file-based grant path. This is a surface boundary, not an identity check, and the limitation ("a human who hands an agent their own shell has removed it") is documented in `SECURITY.md`.

### 15.2 Request and grant

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
  proposed_operation: "record the dependency as authorized; resume the run"
  requested_at: 2026-08-12T14:03:00Z
  expires_at:   2026-08-13T14:03:00Z
```

```yaml
approval_grant:
  id: AG-0019
  request_id: AR-0031
  binding_hash: blake3(action || canonical(facts) || policy_hash)   # ← exact-action binding
  scope: {run: r-0041}          # or {task: …} | {project: …, action: …}
  reuse: false
  expires_at: 2026-08-12T15:03:00Z
  granted_by: krish
  granted_at: 2026-08-12T14:07:11Z
  channel: unix-socket
```

**`binding_hash` is the mechanism the prompt asks for.** A grant authorizes an operation only if the recomputed hash matches at use time. Therefore `dependency.add.runtime:foo` cannot authorize `…:bar` (different facts → different hash) and cannot authorize `deployment.execute` (different action → different hash). If the policy snapshot changed, the hash changes and the grant no longer applies — which is the correct behaviour, not an inconvenience.

### 15.3 Revocation and the in-flight problem (Scenario S)

`conductor approve revoke <id>` sets `REVOKED`. What that means depends on where the effect is:

| State at revocation | Result |
|---|---|
| Not yet consumed | Effect never happens. Run → `AWAITING_APPROVAL`. |
| `INTENDED`, effect not started | Aborted before starting. |
| `INTENDED`, effect in flight | **Cannot be cancelled.** Conductor completes or fails it, records the receipt, then halts with a finding. |
| `CONFIRMED` | Cannot be undone. Conductor records the revocation and raises a `POST_HOC_REVOCATION` finding. |

Stating that revocation cannot un-ring a bell is more useful than a `cancel()` that sometimes works.

### 15.4 Four distinct approval kinds — do not collapse them

| Kind | Authorizes | Granularity | Expires |
|---|---|---|---|
| **Plan approval** | a plan version becoming authoritative | one plan version | no |
| **Policy approval** | one policy-gated action, once | one `binding_hash` | yes (hours) |
| **Policy exception** | temporarily loosening a rule | rule + scope, within the ceiling | yes (mandatory) |
| **Review acceptance** | that completed work is accepted | one review packet | no |

Four tables' worth of semantics, three of them time-bounded, one not. Collapsing them into `approved: bool` — which is the natural drift — would let a plan approval satisfy a deployment gate. They must not share a code path.

---

## 16. Nerve Boundary

### 16.1 Inverted integration — the main change

The baseline has Conductor query Nerve and embed answers into packets. That is the weaker integration, for two reasons: Conductor does not know which questions the agent will have, and embedded answers become stale evidence inside an immutable packet.

**Primary integration: hand `nerve mcp` to the agent.**

Nerve already exposes `nerve mcp` — stdio JSON-RPC, read-only, no network, no subprocess execution, with repository-derived spans explicitly labelled as untrusted (`docs/plans/slice-08-mcp.md`). Claude Code accepts `--mcp-config`. So Conductor's Nerve integration is one config line, and the agent asks its own questions, on demand, against a read-only surface that was designed for exactly this. Nerve's own MCP threat model (T7/T8) has already been built and tested.

**Secondary integration: Conductor uses `nerve check` as a gate.** Nerve's exit codes are stable and documented: `0` success, `2` no/unhealthy index, `3` partial, `4` sound but stale. Conductor runs `nerve check --json` before offering Nerve to an agent, and:

| Exit | Conductor behaviour |
|---|---|
| 0 | attach `nerve mcp`; record index freshness in the packet |
| 3 | attach, with a `partial_index` caveat recorded |
| 4 | **do not attach**; note "index stale" in the packet; optionally run `nerve index` if the project opts in |
| 2 | do not attach; proceed without Nerve |
| absent binary | proceed without Nerve |

### 16.2 What Nerve may and may not do

**May inform:** which files a task likely touches; impact surface of a symbol; related documentation; whether a symbol has known test coverage (`nerve gaps`); cross-repository contracts.

**May never:** authorize an action; satisfy an acceptance criterion; substitute for `git status`; substitute for running tests; determine that a task is complete; be the sole basis for a `deny`.

The rule follows from Nerve's own design: it *"does not run your tests"* and *"never executes repository code."* A tool that has never executed the code cannot certify that the code works, and Conductor must not let a graph answer stand in for an exit code.

### 16.3 Fallback (Scenario M)

Nerve absent, stale, or failing → **execution continues unchanged**, with `nerve: unavailable` recorded in the run's evidence and in the packet. No retries, no blocking. Conductor must work on a machine where Nerve was never installed, and an acceptance test should assert exactly that by running the suite with the binary hidden from `PATH`.

---

## 17. CLI/API and Dashboard

### 17.1 The v1 surface — 12 commands

The prompt lists ~25. Most are ceremony. The smallest coherent surface:

```
conductor init                       # scaffold .conductor/ in the current repo
conductor doctor                     # environment, adapters, db, git, permissions

conductor plan validate [--version N]
conductor plan approve <version>     # human-only, socket-only

conductor task list [--state …]
conductor task run <task-id>         # claim, execute, verify, report  ← the core verb
conductor task show <task-id>        # state, attempts, verification, findings, diff

conductor status                     # everything currently live
conductor approve <request-id> [--scope …] [--ttl …] [--reuse]
conductor deny <request-id> [--reason …]

conductor review export [--since …]  # → review packet
conductor review import <file>       # → review decision

conductor recover [--scan]           # startup reconciliation, on demand
```

`--json` on every command. Stable exit codes, following Nerve's precedent: `0` ok · `2` no/unhealthy state · `3` partial · `4` action required (approval or review pending) · `10` usage · `70` internal.

Cut from the prompt's list, with reasons: `daemon start/stop` (auto-started on demand; `conductor doctor` reports it); `project add/list/inspect` (folded into `init` and `status` — the project is the repo you are in); `plan import` (a plan is a file you write; `validate` reads it); `plan history` (`git log .conductor/plan/`); `run start/inspect/cancel/resume` (a run is an implementation detail of a task; `task run` and `task show` cover it); `policy show` (`cat .conductor/policy.yaml`); `verification show` (part of `task show`); `artifacts` (paths are in `task show --json`).

**One command earns its place beyond the core: `conductor policy explain <action>`.** Debugging *why* something was denied is the operation most likely to be needed at 2 a.m., and it is the one that cannot be reconstructed by reading a YAML file.

### 17.2 API

A unix socket at `$XDG_RUNTIME_DIR/conductor.sock`, mode `0600`, line-delimited JSON-RPC. No TCP port in v1 — a loopback port is reachable by any process on the machine, including the agent; a `0600` socket outside the agent's sandbox scope is not. This is a direct consequence of §7.1, and it is why I would not follow Nerve's loopback-HTTP pattern here despite it being the house style: Nerve's surface is read-only, and Conductor's is not.

Hand-rolled JSON-RPC framing, no async framework dependency — Nerve made this exact call for `nerve mcp` on measured evidence (3 transitive crates vs. ~80–100), and the same reasoning applies.

### 17.3 Dashboard

After S18. Read-only first. Then, one rule, absolute:

> **Every dashboard mutation maps to an existing, tested CLI/API operation. The dashboard never gains a capability the CLI lacks.**

And: **approval must be possible from the CLI forever.** If approval ever becomes dashboard-only, the system stops working over SSH and the §7.1 boundary gets rebuilt in a browser, which is a much larger attack surface.

---

## 18. Revised End-to-End Implementation Roadmap

### 18.1 Ordering critique of the baseline

The baseline proposes: `contracts → db → git fixtures → fake agent → crash recovery → verification → repair → policy → approvals → first real agent → planning/review → Nerve → multi-project → dashboard → dogfooding`.

Four changes:

1. **Slice 0 must be measurement, not code.** Five load-bearing claims in this document are unverified (worktree config sharing, Codex sandbox containment, Codex network denial, clone cost on a large repo, SQLite claim latency). The user's own repository establishes this pattern — Nerve's slice-06 opens with *"the §2.1 gating question, to be answered empirically before any emission path is written"*, and ADR-0001 carries a *"Measured result — Slice 1"*. Building on unmeasured claims is what this project's sibling refuses to do.
2. **"Contracts" first is a trap.** Designing schemas before the spine exists means designing them from imagination. Define the *minimum* types each slice needs and let the vertical slice reveal the rest. The baseline's "contracts" phase would produce a packet schema before anything consumes one.
3. **Fake agent and crash recovery are one slice, not two.** The fake agent's entire purpose is to inject crashes; its scenarios *are* the recovery tests. Separating them means building a fake agent with no assertions to satisfy.
4. **Enforcement (S9) must precede the first real agent (S10).** The baseline has policy and approvals before the real agent, which is right, but omits the credential/environment isolation layer — the control that actually does the work (§7.3). A real agent must never run before the environment it runs in is locked down.

### 18.2 Slices

---

**S0 — Measurement and pre-registered falsification**

- **Objective:** answer five empirical questions; record results as ADR-000x with falsification triggers.
- **Why now:** §11.2, §12.7 and §4 all rest on claims I have argued but not observed. If Q2 fails, the first-adapter choice flips.
- **Dependencies:** none.
- **Scope:** throwaway scripts + `docs/decisions/`. Questions: **Q1** does a `git worktree` share `.git/config`, refs and hooks with the main repo? **Q2** does `codex exec --sandbox workspace-write` deny a write to `$HOME` and deny network egress, including for spawned children? **Q3** cost of `git clone --local` for repos at 10 MB / 500 MB / 5 GB. **Q4** SQLite `BEGIN IMMEDIATE` claim latency at 1/4/16 concurrent writers. **Q5** does a Claude `PreToolUse` hook injected via `--settings` reliably deny `Bash(git push*)`, and what is the cheapest bypass?
- **Out of scope:** all product code.
- **Files:** `docs/decisions/ADR-0001..0005`, `scripts/measure/`.
- **Tests:** none (this slice produces evidence).
- **Failure injection:** n/a.
- **Acceptance:** five ADRs, each with a measured number, a decision, and a pre-registered falsification trigger.
- **Stop point:** before any `src/`.
- **Risks:** none. This is the cheapest slice and the one most likely to be skipped.

---

**S1 — Store foundation**

- **Objective:** SQLite schema v1, migrations, transaction helpers, `conductor doctor`.
- **Why now:** everything persists; nothing else can be tested without it.
- **Dependencies:** S0/Q4.
- **Scope:** `project`, `task`, `run`, `attempt`, `event`, `workspace` tables. `BEGIN IMMEDIATE` helper. WAL, `synchronous=FULL`, `foreign_keys=ON`. Forward-only migrations with a version table. `conductor doctor` reports db, git, adapters, socket dir.
- **Out of scope:** policy, verification, approvals, packets.
- **Files:** `src/store/{mod,schema,migrate,tx}.rs`, `src/cli/doctor.rs`.
- **Tests:** migration idempotency; `integrity_check` after each; concurrent-writer contention; `synchronous=FULL` survives simulated power loss (kill mid-transaction, reopen).
- **Failure injection:** `SIGKILL` mid-transaction ×100 → db always opens and is consistent.
- **Acceptance:** 100 kill-restart cycles, zero corruption, zero partial rows.
- **Stop point:** `conductor doctor` green.
- **Risks:** schema churn ahead — keep migrations cheap to write.

---

**S2 — Workspace provider (local clone)**

- **Objective:** create, describe, reconcile and quarantine per-run workspaces.
- **Why now:** the load-bearing safety mechanism, and the one the baseline gets wrong. Everything downstream assumes it.
- **Dependencies:** S0/Q1, Q3; S1.
- **Scope:** clone at base commit; remove `origin`; set identity, `core.hooksPath=/dev/null`; write `.conductor-run.json`; baseline capture (§11.1); `reconcile()` producing §10.8 verdicts; quarantine of orphans. Fixture repos: clean, dirty, detached, with-submodule, with-nested-repo, large.
- **Out of scope:** agents, verification, policy.
- **Files:** `src/workspace/{clone,baseline,reconcile,quarantine}.rs`, `tests/fixtures/`.
- **Tests:** every §10.8 verdict from a hand-constructed git state; config/ref/hook isolation asserted directly (a test that mutates config *inside* the clone and asserts the source repo is byte-identical); submodule repo is refused at registration.
- **Failure injection:** delete the workspace mid-run; corrupt `.git`; leave `index.lock`; move the source repo.
- **Acceptance:** an adversarial script inside the clone that sets a remote, deletes a branch, writes a hook and runs `gc` leaves the **source repository byte-identical** (asserted by hashing `.git/config`, `git show-ref`, and the hooks dir).
- **Stop point:** isolation proven.
- **Risks:** clone cost on large repos — mitigated by S0/Q3 and the worktree opt-in.

---

**S3 — Fake agent, supervision, and crash recovery** *(merged)*

- **Objective:** spawn/supervise/classify a subprocess; recover every failure mode from evidence alone.
- **Why now:** this is Conductor's spine. Real agents add nothing testable that a fake one does not.
- **Dependencies:** S1, S2.
- **Scope:** process supervisor (spawn, JSONL read, idle+wall timeouts, `SIGTERM`→grace→`SIGKILL`); leases with `lease_epoch` fencing; heartbeat conditional on child liveness; attempt state machine; startup recovery (§10.6). Fake agent driven by a scenario file covering all 14 prompt-§33 scenarios.
- **Out of scope:** verification, policy, repair, real adapters, daemon.
- **Files:** `src/supervisor/`, `src/recovery/`, `src/agents/fake.rs`.
- **Tests:** one per scenario; each asserts persisted state, attempt outcome, and reconciliation verdict.
- **Failure injection:** `SIGKILL` the agent at 8 points; `SIGKILL` **Conductor** at 12 points; stall; malformed JSONL; missing report; duplicate spawn.
- **Acceptance:** for every (scenario × kill-point) pair, restart converges to the correct state **without human input**, and no work is silently lost. Fencing test: a stalled worker that wakes after lease expiry cannot write.
- **Stop point:** the whole matrix green.
- **Risks:** highest-complexity slice. If it slips, everything slips — which is why it is third rather than eighth.

---

**S4 — Verification runner**

- **Objective:** run checks, bind results to tree hashes, classify four outcomes.
- **Dependencies:** S2, S3.
- **Scope:** profile loading; required/conditional/invariant checks; timeouts → `INCONCLUSIVE`; tree-hash binding and `VOID` detection; content-addressed cache; log capture with secret scanning; toolchain fingerprint.
- **Out of scope:** repair, policy.
- **Files:** `src/verify/{profile,runner,cache,classify}.rs`.
- **Tests:** pass/fail/timeout/infra-error; cache hit on identical tree; **`VOID` when the tree changes mid-check** (a fixture that writes a file from a background process while a check runs); conditional triggering by changed paths.
- **Failure injection:** kill mid-check; fill the disk; remove the toolchain between runs.
- **Acceptance:** no check result is ever attributed to a tree it did not observe.
- **Stop point:** cache correctness proven.

---

**S5 — First vertical: task → agent → reconcile → verify → commit**

- **Objective:** one fake-agent task from `PENDING` to `COMPLETE`, end to end, with a real commit.
- **Why now:** proves the spine before policy, approvals or packets add surface. This is the "boring end-to-end slice" the baseline defers too long.
- **Dependencies:** S1–S4.
- **Scope:** task state machine; minimal task spec file (not yet the full plan ledger); Conductor-owned commit via the `side_effect` ledger; fetch of the run branch into the source repo; `conductor task run/show/list`.
- **Out of scope:** policy, approvals, repair, real agents, plan versioning.
- **Files:** `src/task/`, `src/effects/`, `src/cli/task.rs`.
- **Tests:** the happy path; `RUNNING→COMPLETE` rejected without reconciliation; false-success report → `CONTRADICTED`.
- **Failure injection:** kill **between** intent and effect, and between effect and confirm, for both commit and fetch.
- **Acceptance:** kill at any of 6 points during commit/fetch → restart produces **exactly one** commit and one ref update. Verified by counting commits.
- **Stop point:** the spine works.

---

**S6 — Bounded repair**

- **Objective:** failed verification → bounded repair with real loop detection.
- **Dependencies:** S4, S5.
- **Scope:** failure fingerprinting; `progressed()`; three loop-breakers; repair packet; budget accounting; new session on attempt 2; escalation to `AWAITING_REVIEW`.
- **Out of scope:** policy-driven repair, agent-chosen strategy.
- **Files:** `src/repair/`.
- **Tests:** identical fingerprint → stop at attempt 1; oscillation → stop; empty edit → stop; genuine progress → continue; budget exhaustion → review.
- **Failure injection:** fake agent that always produces the same failure; one that oscillates; one that changes nothing.
- **Acceptance:** **no configuration of the fake agent can produce more than `max_attempts` agent invocations.** Asserted by counting spawns.
- **Stop point:** loops provably bounded.

---

**S7 — Policy engine**

- **Objective:** typed actions, two-stage evaluation, snapshots, explain.
- **Why now:** sensitive actions now exist (S5 commits, S6 retries) and the spine reveals which facts are needed.
- **Dependencies:** S5.
- **Scope:** global+project YAML; ceiling/join evaluation (§6.2); deterministic fact extractors; snapshot + BLAKE3 hash; run-lifetime snapshot pinning; `conductor policy explain`; `unknown → deny`.
- **Out of scope:** approvals (S8), enforcement (S9), model-assisted facts.
- **Files:** `src/policy/{model,load,evaluate,facts,explain}.rs`.
- **Tests:** precedence matrix as a table test; project cannot loosen past a locked ceiling; unknown action denies; snapshot pinning across a mid-run policy edit; explain output names non-matching rules and why.
- **Failure injection:** malformed policy; conflicting rules; policy file deleted mid-run (snapshot must still resolve).
- **Acceptance:** every cell of the precedence matrix asserted; hash stable across two serializations of the same policy (byte-identical, per Nerve's determinism discipline).
- **Stop point:** evaluation explainable.

---

**S8 — Approvals**

- **Objective:** durable, exactly-scoped, expiring approvals over a socket the agent cannot reach.
- **Dependencies:** S7.
- **Scope:** unix socket at `0600`; request/grant lifecycle; `binding_hash`; TTL; one-shot vs. reuse; revocation semantics (§15.3); persistence across restart; four distinct approval kinds.
- **Out of scope:** dashboard, notifications.
- **Files:** `src/approval/`, `src/ipc/socket.rs`.
- **Tests:** grant for action A does not satisfy action B; grant for `dep:foo` does not satisfy `dep:bar`; expiry; restart during a wait restores it; revocation in each of the four §15.3 states; **a source-scan test asserting no approval-granting code path is reachable from the workspace shim binary** (the shape of Nerve's `layering.rs`).
- **Failure injection:** kill during an approval wait; kill between grant and consume; socket file deleted.
- **Acceptance:** approval state survives 50 kill-restart cycles; no grant is ever consumed twice; the layering test fails if someone wires approval into the shim.
- **Stop point:** approvals are durable and bounded.

---

**S9 — Enforcement and post-run audit**

- **Objective:** make the environment, not the prompt, the boundary.
- **Why now:** **hard gate — this must land before any real agent runs.**
- **Dependencies:** S2, S7.
- **Scope:** env allowlist; per-run `HOME`; `GIT_ASKPASS` that fails; `SSH_AUTH_SOCK` unset; secret scanner; full post-run audit surface (§7.7); findings that never auto-resolve; `SECURITY.md` stating v1 limitations (§7.6).
- **Out of scope:** containers, network filtering.
- **Files:** `src/enforce/{env,audit,secrets}.rs`, `SECURITY.md`.
- **Tests:** a fake agent that attempts push, remote mutation, config edit, hook install, secret exfiltration, and out-of-scope writes — each detected, each a finding; env allowlist asserted by dumping the child's environment and diffing against expected.
- **Failure injection:** agent that writes a secret into a file; one that sets a remote; one that installs a hook.
- **Acceptance:** every prompt-§10 sensitive operation is either **prevented** (with the mechanism named) or **detected** (with the evidence named), and the classification is written into `SECURITY.md` per item. **No item may be listed as prevented without a passing test.**
- **Stop point:** the honesty table is complete and true.

---

**S10 — First real adapter: Codex**

- **Objective:** replace the fake agent with `codex exec` behind the same interface.
- **Dependencies:** S3, S9. Confirmed by S0/Q2.
- **Scope:** `AgentAdapter` for Codex; JSONL event mapping; `--output-schema` report; `--sandbox workspace-write`; `--ignore-user-config`/`--ignore-rules`; `resume`; exit classification.
- **Out of scope:** Claude adapter, hooks.
- **Files:** `src/agents/codex.rs`, `tests/fixtures/codex-jsonl/`.
- **Tests:** adapter parses recorded JSONL fixtures with **no process spawned**; malformed lines tolerated; schema-invalid report → `CONTRADICTED` path.
- **Failure injection:** kill Codex mid-run; auth failure; truncated JSONL; schema violation.
- **Acceptance:** the entire S3 crash matrix passes with Codex substituted for the fake agent. If any scenario needs adapter-specific handling, that is a design smell to fix in the interface, not in the adapter.
- **Stop point:** one real slice of real work completes on a fixture repo.
- **Risks:** first nondeterminism. Keep the fake agent as the primary CI harness forever — real-agent tests are a separate, non-blocking suite.

---

**S11 — Plan ledger and decisions**

- **Objective:** repo-tracked, versioned, immutable plans; append-only decisions.
- **Why now:** the spine works; now make the input authoritative.
- **Dependencies:** S5.
- **Scope:** `.conductor/plan/vN/`; `plan validate` (§2.4); `plan approve` (socket-only); content hashing; task materialization; supersession; decisions with `ACCEPTED|REJECTED|SUPERSEDED`; `db`-loss test.
- **Out of scope:** packets, review bridge.
- **Files:** `src/plan/`, `src/decision/`.
- **Tests:** approved plan is immutable (edit → hard error); revision creates a version and supersedes tasks; **deleting `conductor.db` and rebuilding loses no plan or decision**; in-flight runs keep their old plan version (Scenario O).
- **Acceptance:** the db-loss test passes.

---

**S12 — Packets and reports**

- **Objective:** generate every packet from durable state.
- **Dependencies:** S11, S10.
- **Scope:** implementation, repair, continuation packets; report schema; context minimization; evidence linking; determinism.
- **Tests:** **the same state produces a byte-identical packet twice** (Nerve's export-determinism discipline); packet size budget enforced; continuation packet regenerable after a total process restart.
- **Acceptance:** an agent handed only a continuation packet completes a task interrupted mid-way, on a fixture, with no session resume.

---

**S13 — Review bridge**

- **Objective:** the ChatGPT loop, mechanized.
- **Dependencies:** S12.
- **Scope:** `review export` / `review import`; the five decision outcomes; review cadence config; boundary detection (milestone end, repeated failure, policy violation, ambiguous recovery, plan deviation).
- **Tests:** each outcome routes correctly; `revise_plan` creates a version and supersedes; import is socket-only.
- **Acceptance:** a full loop — export → hand-edited decision → import → resume — with no manual state editing.

---

**S14 — Daemon, concurrency, multi-project**

- **Objective:** survive terminal close; run 2+ projects concurrently.
- **Why now:** deferred until recovery semantics are proven (§5.3).
- **Scope:** `conductor daemon`; auto-start on demand; per-project locking; concurrent-run limits; stale-socket handling.
- **Tests:** close terminal → run continues; two projects concurrently with no cross-contamination (Scenario N); daemon kill → recovery.
- **Acceptance:** Scenario N and Scenario L pass with the daemon.

---

**S15 — Claude Code adapter with hook interception**

- **Scope:** adapter; Conductor-generated settings with `PreToolUse` → `conductor hook`; `--include-hook-events` audit trail; `--session-id` pre-assignment; `--max-budget-usd`; `--setting-sources` hermeticity.
- **Tests:** hook denies a forbidden Bash pattern (S0/Q5 evidence); denial appears in the event stream; **documented bypasses asserted as known-failing tests**, so nobody later mistakes them for coverage.
- **Acceptance:** S3 crash matrix passes with Claude; `SECURITY.md` updated with the Claude-specific "detect only" rows.

---

**S16 — Nerve integration** · attach `nerve mcp` via `--mcp-config`; `nerve check` gate; suite passes with the binary hidden from `PATH`.

**S17 — Dogfooding** · Conductor manages one real Conductor slice, with review at every task, `git.push` forbidden, and a human accepting every result.

**S18 — Dashboard** · read-only first; mutations only via tested API operations.

### 18.3 Hard gates

- **S9 before S10.** No real agent before the environment is locked down.
- **S7 before S8.** No approvals before there is a policy to approve against.
- **S3 before everything after it.** If recovery is not proven, nothing built on it is trustworthy.
- **S0 before S2's design is fixed.** If Q1 refutes worktree config-sharing, §11.2's argument weakens and should be revisited honestly.

---

## 19. End-to-End Acceptance Suite

Every row is a test. "Retry?" means an automatic agent attempt; "Human?" means execution halts for a person.

| # | Scenario | Injected | Expected persisted state | Automatic behaviour | Retry? | Human? | Final |
|---|---|---|---|---|---|---|---|
| 1 | Success | — | attempt `EXITED`, `CLEAN_COMPLETE`, all `PASS` | commit, fetch, advance | no | no | `COMPLETE` |
| 2 | Crash before edits | kill at t=1s | `CRASHED`, `NO_CHANGE` | new attempt, same packet | yes | no | `COMPLETE` |
| 3 | Crash after edits | kill after writes | `CRASHED`, `CLEAN_NO_REPORT` | verify current tree; continuation packet | yes | no | `COMPLETE` |
| 4 | Missing report | exit 0, no report | `EXITED`, `CLEAN_NO_REPORT` | verification decides | no | no | `COMPLETE` |
| 5 | Malformed report | invalid JSON | `EXITED`, finding `REPORT_UNPARSEABLE` | verification decides; finding stays | no | no | `COMPLETE` + finding |
| 6 | False success | "complete", tree unchanged | `CONTRADICTED` | halt | no | **yes** | `AWAITING_REVIEW` |
| 7 | Verification failure | test fails | `FAIL` | repair packet | yes ≤2 | if unfixed | `COMPLETE` or `AWAITING_REVIEW` |
| 8 | Verification timeout | hang > timeout | `INCONCLUSIVE` | infra retry ×1, no budget spent | infra only | after 2 | `AWAITING_REVIEW` |
| 9 | Repeated identical failure | same fingerprint | attempt 2 not started | **stop at once** | no | **yes** | `AWAITING_REVIEW` |
| 10 | Daemon crash mid-run | kill Conductor | lease expires | adopt or reconcile on restart | no | no | resumes |
| 11 | Reboot with live workspaces | reboot | leases expired, workspaces on disk | scan descriptors, reconcile each | no | no | resumes |
| 12 | Crash during approval wait | kill in `AWAITING_APPROVAL` | request `REQUESTED` | wait restored, TTL preserved | no | yes (as before) | resumes on grant |
| 13 | Dependency policy violation | agent adds a dep | `POLICY_SENSITIVE`, request created | halt | no | **yes** | `AWAITING_APPROVAL` |
| 14 | Git remote mutation | `git remote set-url` in the clone | config diff vs. baseline | **contained** — source repo unaffected; finding | no | **yes** | `AWAITING_REVIEW` |
| 15 | Target branch moved | user commits to `main` | divergence at integration | no rebase, no merge | no | **yes** | `AWAITING_REVIEW` |
| 16 | Dirty user repo | uncommitted changes at start | recorded in baseline | proceed (clone from commit) | no | no | `COMPLETE`, user tree untouched |
| 17 | Abandoned worktree/clone | orphan on disk | orphan detected | **quarantine**, never delete | no | no | reported in `status` |
| 18 | Nerve outage | binary hidden | `nerve: unavailable` | proceed unchanged | no | no | `COMPLETE` |
| 19 | Concurrent projects | 2 runs, 2 repos | independent rows and clones | no cross-contamination | no | no | both `COMPLETE` |
| 20 | Plan revision mid-flight | approve v4 during a v3 run | run keeps `plan_version=3` | finish under v3; new tasks under v4 | no | at review | `COMPLETE` under v3 |
| 21 | Duplicate side effect | kill between effect and confirm | `side_effect` `INTENDED` | re-check precondition; do not re-run | no | only if ambiguous | exactly one commit |
| 22 | Policy change during run | edit policy mid-run | run keeps `policy_hash` | old snapshot; **pause if strictly tighter** | no | if tighter | `COMPLETE` or `AWAITING_APPROVAL` |
| 23 | Verification passes, policy violated | tests green + forbidden change | `POLICY_SENSITIVE` | **policy wins over green tests** | no | **yes** | `AWAITING_APPROVAL` |
| 24 | Approval revoked mid-effect | revoke during `INTENDED` | grant `REVOKED`, effect recorded | complete/fail the effect, then halt | no | **yes** | `AWAITING_REVIEW` |
| 25 | Tree mutated during verification | background write | result `VOID` | re-run at the new tree; finding | yes (verify only) | if repeated | `COMPLETE` or review |
| 26 | Stale worker wakes late | pause past lease, resume | fencing epoch stale | **all writes rejected** | no | no | successor unaffected |

Rows 14, 21, 23, 25 and 26 are the ones that most distinguish this design from the baseline.

---

## 20. Risks and Open Decisions

### Must resolve before writing code

| Risk | Sev | Lik | Detect | Mitigation cost |
|---|---|---|---|---|
| **Approval reachable by the agent (§7.1)** | Critical | High if unaddressed | Low — looks fine until abused | Low — socket + env + layering test |
| **Worktree config/ref/hook sharing (§11.2)** | High | Certain if worktrees used | Medium | Low — clone instead |
| **Runtime choice** | High | — | — | Very high to reverse |
| **Stack choice** | Medium | — | — | Very high to reverse |
| **Codex sandbox actually contains (S0/Q2)** | High | Unknown | High (measurable) | Low — measure it |

### Resolve during early slices

- Verification cache-key completeness (env vars? `$PATH`? OS version?) — start conservative, widen on evidence.
- Repair budget defaults — 2 is a guess; instrument and revisit.
- Fact extractors for non-Rust/JS ecosystems — add on demand.
- Clone-cost threshold for the worktree opt-in — from S0/Q3.
- Whether `--setting-sources` fully isolates ambient hooks — measure in S15.

### Intentionally defer

- Multi-machine anything. Dashboard. Additional adapters beyond two. Container isolation (revisit when a runtime is installed). Network egress control. Submodules. Automatic merge/rebase. Cost tracking beyond `--max-budget-usd`. Notifications. Plan authoring assistance.

### Explicitly not decisions yet

Log retention defaults, packet size limits, artifact GC policy, CLI colour scheme. Deciding these now would be inventing constraints before the data exists.

---

## 21. Comparison Against the Baseline Plan

**Strongest parts.** Typed policy actions (§9) — the single best idea, and correct. The insistence that verification and Git beat agent self-report. Fake-agent-first. The explicit refusal to claim exactly-once. Recognizing that policy ≠ security (§10) — the framing is right even where the follow-through is not. The failure-scenario catalogue (§34), which is more complete than most production systems have. CLI-first, dashboard-later.

**Weakest parts.** (1) **Worktrees presented as isolation** — file isolation mistaken for repository isolation. (2) **The approval channel is reachable by the agent it constrains** — every approval semantic rests on an authorization the agent could forge. (3) **"Materialized state" implies the database is the source of truth**, contradicting the product's own thesis. (4) **The `WorkflowRuntime` abstraction** — one implementation, designed for a backend it argues against. (5) **"No progress" in repair is undefined**, which is the difference between a bounded loop and a budget fire.

**Missing.** Credential/environment isolation as a first-class control. Fencing tokens for stale workers. Tree-hash binding of verification results. `INCONCLUSIVE` as a distinct outcome. Exact-action binding via hash. Where plans and decisions live (repo vs. database) and what survives database loss. Submodule refusal. Determinism requirements for packets and policy hashes.

**Overengineered.** The `WorkflowRuntime` interface. Materialized projections. `Checkpoint`, `Failure`, `Continuation` as tables. A 25-command CLI. Agent selection. Milestone/slice status as stored fields. Fourteen task states.

**Underengineered.** Approval enforcement. Git isolation. Repair loop detection. The definition of "exactly-once." Verification validity windows. Policy precedence for locked rules. What happens when the database is lost.

**Decisions I changed.** Worktree → clone. Projections → plain state. Runtime interface → pure functions. Daemon-first → foreground-first. Claude-first → Codex-first. Plans in DB → plans in repo. Timeout=failure → timeout=`INCONCLUSIVE`. Fourteen task states → nine.

**Decisions I retained.** Custom runtime. SQLite. Append-only evidence log. Typed policy actions. Git-authoritative reconciliation. Deterministic verification. Bounded repair. Focused packets. Fake-agent-first. Optional Nerve. CLI-first. Human-mediated planning.

**New ideas introduced.** The plan ledger lives in the repository and SQLite is an index over it (§8.1). Verification results content-addressed by tree hash, giving caching, crash-safety and `VOID` detection (§14.2). `binding_hash` for exact-action approval (§15.2). `lease_epoch` fencing (§10.2). Failure fingerprints with a three-way loop-breaker (§14.5). Nerve integration inverted — `nerve mcp` to the agent, `nerve check` as a Conductor gate (§16). Credential absence as the primary enforcement mechanism (§7.3). The rule that an effect Conductor cannot verify afterwards is an effect Conductor may not own (§10.7). Self-describing workspace descriptors enabling recovery from total database loss (§11.9).

---

## 22. Comparison Rubric — 100 points

| # | Dimension | Pts | What full marks require |
|---|---|---:|---|
| 1 | Product boundary & scope discipline | 8 | Human/automation line justified by which inputs have oracles; non-goals enforced by design, not intent |
| 2 | Global/project policy semantics | 10 | Precedence as an algebra; locked rules as a ceiling; scoped exceptions with expiry; snapshots + hashes; explainable negatives; fail-closed on unknown |
| 3 | Durable recovery & execution guarantees | 12 | Named transaction boundaries; fencing; startup reconciliation from **disk**, not from a log; guarantee stated in tiers; no hidden-state dependency |
| 4 | Git safety & workspace isolation | 12 | Config/ref/hook isolation addressed explicitly; dirty tree, branch movement, orphans, submodules; source repo provably unaffected by a hostile run |
| 5 | Verification authority | 9 | Results bound to the tree observed; `INCONCLUSIVE` distinct from `FAIL`; conditional checks; completion criteria that exclude the agent's report |
| 6 | Repair correctness & loop prevention | 6 | Formal progress predicate; ≥2 independent loop-breakers; provable upper bound on invocations |
| 7 | Approvals & security honesty | 12 | Approval channel unreachable from the agent; exact-action binding; revocation semantics per state; prevented-vs-detected table with a test behind every "prevented" |
| 8 | Plan versioning & decision ledger | 7 | Immutable approved plans; append-only decisions; runs pinned to a version; survives database loss |
| 9 | Agent abstraction & first adapter | 6 | Capabilities verified against installed binaries; no invented flags; first adapter justified by a capability, not preference |
| 10 | Human/ChatGPT bridge | 5 | Packets generated from durable state; deterministic; import is a mutating, gated operation |
| 11 | Implementation sequencing | 8 | Measurement before design commitment; vertical spine early; hard gates named; slices independently verifiable |
| 12 | Local-first usability | 3 | Works offline, no container runtime, no external services, single binary |
| 13 | Avoidance of premature platform complexity | 2 | No generic scheduler, DSL, or DAG engine; named containment rules |
| | **Total** | **100** | |

Scoring: 0 = absent · 25% = mentioned · 50% = specified · 75% = specified with failure modes · 100% = specified with failure modes **and** an acceptance test that would catch its absence.

---

## 23. The Ten Most Important Decisions, Ranked

1. **The approval channel must be unreachable from the agent's environment.** Without it, every safety guarantee is a suggestion. (§7.1)
2. **Per-run local clone, not worktree.** The difference between "the agent broke its sandbox" and "the agent broke your repository." (§11.2)
3. **Git and the filesystem are the source of truth; the agent's report is evidence.** Everything else follows from this. (§10.8)
4. **Build the runtime; do not adopt Temporal or Hatchet.** Durable execution is not reconciliation, and neither removes Conductor's hard parts. (§4)
5. **Credential and environment absence is the primary enforcement mechanism.** Capabilities you never grant cannot be misused. (§7.3)
6. **Verification results are bound to the tree hash they observed.** Otherwise "passing" is a claim about a moment, not a state. (§14.2)
7. **Conductor may only own effects whose occurrence it can verify afterwards.** This is what keeps deployment out of v1 by construction rather than by policy. (§10.7)
8. **The plan and decision ledger lives in the repository.** Conductor's most valuable data must outlive its database. (§8.1)
9. **Policy precedence is a ceiling plus a join, and unknown actions deny.** (§6.2)
10. **Fake agent and crash recovery are one slice, third.** If recovery is not proven early, everything built on it is unfalsifiable. (§18.2/S3)

---

## 24. Five Legitimate Areas of Disagreement

**1. Rust vs. TypeScript.**
*A:* Rust — exhaustive state machines, single binary, measured velocity. *B:* TypeScript — faster schema churn, native Agent SDK, larger contributor pool.
*My choice:* Rust, for this developer specifically (§5.2).
*Changes my mind:* if the first three slices show schema churn dominating (say, >30% of commits are serde/type plumbing with no behaviour change), or if Conductor needs in-process `canUseTool` for a capability hooks cannot provide.

**2. Clone vs. worktree.**
*A:* clone — config/ref/hook isolation. *B:* worktree — cheaper, and post-run audit catches config changes anyway.
*My choice:* clone. Detection is not containment, and a `pre-commit` hook written during a run fires under the *human's* hands later.
*Changes my mind:* S0/Q3 showing clone cost > 10 s on the user's actual repositories, with no acceptable mitigation. Then: worktree plus `core.hooksPath=/dev/null` plus config-diff-on-every-write, with the exposure documented at the config site.

**3. Daemon-first vs. foreground-first.**
*A:* foreground — trivial crash testing, less early surface. *B:* daemon — the real topology; building it late means retrofitting concurrency.
*My choice:* foreground first (§5.3). Lease-based claiming is designed in from S1, so the daemon is an entry point, not a rewrite.
*Changes my mind:* if lease and concurrency semantics turn out to differ meaningfully between the two, which would mean I designed S1 wrong.

**4. Event journal vs. full event sourcing.**
*A:* journal as evidence, plain mutable state. *B:* full event sourcing with projections.
*My choice:* A (§8.2). The authoritative state is outside the database.
*Changes my mind:* if "how did this run reach this state?" becomes a frequent, hard question that the event log cannot answer — i.e. if I find myself repeatedly reconstructing state by hand from events.

**5. Codex-first vs. Claude-first.**
*A:* Codex — real OS sandbox, schema-enforced output. *B:* Claude — hooks, assignable session ID, spend cap, and the user's primary tool.
*My choice:* Codex (§12.7), because containment is the capability v1 most lacks.
*Changes my mind:* S0/Q2 showing `workspace-write` does not actually contain writes or network. Then Claude-first, and the containment story becomes "detection only," stated plainly.

---

## 25. Overengineering Traps

| Trap | How it starts | Containment rule |
|---|---|---|
| **Temporal clone** | "we need replay for debugging" → workflow versioning → determinism sandboxing | **The `event` table is append-only and is never replayed to produce state.** If replay is ever needed, that is trigger §4.6(5). |
| **Hatchet clone** | "we need priorities" → fair scheduling → backpressure → worker pools | **One claim query, FIFO within priority. If a second scheduling dimension appears, write an ADR before writing code.** |
| **Generic scheduler** | "cron for nightly runs" | **No timers except lease expiry and approval TTL (§10.5). Nightly runs are `launchd` calling `conductor task run`.** |
| **Event-sourcing framework** | "aggregates would be cleaner" → projections → snapshots → rebuild tooling | **Materialized projections are deleted from the design (§8.2). Reintroducing them requires refuting §8.2 in writing.** |
| **CI platform** | "we already run tests" → matrices → caching → artifacts → parallelism | **Verification runs configured commands, in one workspace, sequentially. No matrix. No parallelism across checks. If you want a matrix, you want CI.** |
| **Agent marketplace** | "adapters should be plugins" → registry → sandboxing → versioning | **Adapters are compiled-in. Two of them. A third requires evidence that two are insufficient.** |
| **Security sandbox project** | "we should really contain the agent" → seatbelt profiles → namespaces → a container runtime | **Conductor uses containment its adapters already provide (Codex) and otherwise removes capabilities (§7.3). Conductor does not build isolation primitives.** |

The general rule, and the one worth writing on the wall: **every subsystem must be traceable to a row in §19.** If a feature does not make an acceptance-suite row pass, it is not part of v1.

---

## 26. Underengineering Traps

| Area | The tempting simplification | Why it is dangerous | Minimum acceptable |
|---|---|---|---|
| **Crash recovery** | "resume the agent session" | Session resume is an optimization; the session may be gone, and correctness would then depend on hidden state | Full reconstruction from Git + filesystem + descriptors, tested with the session deliberately destroyed |
| **Transaction boundaries** | separate statements for claim, lease, event | Crash between them yields a claimed run with no lease, or an event for a claim that did not happen | One `BEGIN IMMEDIATE` per state change, with the event inside it (§10.2) |
| **Git isolation** | "worktrees are fine" | Config, refs, hooks and objects are shared; a hook written in a run fires under the human later | Clone, `origin` removed, `core.hooksPath=/dev/null`, byte-identity test on the source repo |
| **Duplicate effects** | "retry the commit; git is idempotent" | It is not — a retried commit produces a second commit with a different hash and timestamp | Precondition-checked `side_effect` ledger; `AMBIGUOUS` halts (§10.7) |
| **Policy evaluation** | "most restrictive wins" | Under-specifies locked rules, so locking silently does nothing | Ceiling + join, with the precedence matrix as a table test (§6.2) |
| **Approval scope** | `approved: bool` | One approval authorizes everything; the four kinds collapse | `binding_hash` over action + facts + policy hash; four separate kinds (§15) |
| **Plan versions** | "just edit the roadmap" | Runs reference a plan that no longer exists; reports become unreproducible | Immutable approved versions, content-hashed, runs pinned (§8.3) |
| **Verification** | "tests passed, we're done" | Passed *when*, on *what tree*, with *what toolchain*? | Tree-hash binding; `VOID` on mid-run mutation (§14.2) |
| **Stale attempts** | "if the lease expired, take over" | The old worker may still be alive and about to write | `lease_epoch` fencing; all writes carry the epoch (§10.2) |
| **Secret handling** | "we won't print secrets" | Logs, diffs and packet excerpts all leak by default | Scanner on every path into an artifact or packet, tested with planted secrets |

---

## 27. Direct Debate Response

### What I Strongly Agree With

Typed policy actions — the strongest idea in the proposal, and the thing that makes any of this enforceable. Git and verification as authoritative over agent self-report. The refusal to claim exactly-once. The explicit separation of policy intent from enforcement in §10 — the framing is exactly right even where I think the follow-through falls short. Fake-agent-first. Human-mediated strategic planning, for the reason I gave in §2.4: a plan is the only input with no oracle. CLI-first, dashboard-later. The failure-scenario catalogue. And the instruction not to build Hatchet or Temporal feature-for-feature — that instinct is correct, and §4 is me agreeing with it for load-bearing reasons rather than by assertion.

### What I Agree With but Would Modify

**Event journal:** keep it as an append-only evidence log; delete "materialized projections." The phrase implies the database is the source of truth, which contradicts the plan's own thesis two sections earlier.

**Verification:** add tree-hash binding and `INCONCLUSIVE`. A result that does not name the tree it observed is a claim about a moment.

**Repair:** define "no progress" formally. Fingerprint + subset-shrink + oscillation detection, with a provable upper bound on invocations.

**Policy inheritance:** the hierarchy is right; "most restrictive wins" is not, because it makes locked rules peers with project rules. Ceiling + join.

**Approvals:** durable and scoped is right; bind them to a hash of (action, facts, policy) so scope is enforced by construction rather than by a comparison someone has to remember to write.

**Task states:** fourteen is too many. Nine, with `COMMITTING` replaced by the idempotency ledger and `FAILED`/`ABANDONED` folded into review and cancellation.

**Daemon:** yes, but not first.

**Nerve:** the boundary is right; the integration is backwards. Hand `nerve mcp` to the agent; use `nerve check` as your own gate.

### What I Disagree With

**1. Worktrees as the isolation boundary.** They share `.git/config`, refs, hooks and the object store. An agent running `git remote set-url` in a worktree mutates the user's real repository, and a `pre-commit` hook it writes will fire under the user's own hands later. This is the plan's most consequential technical error. Use a per-run local clone with `origin` removed.

**2. The approval channel is reachable by the agent it constrains.** `conductor approve` on the same CLI, same machine, same shell the agent holds. The user's own Nerve repository already documents this exact class of defect and its only honest fix — *"the honest control is not an identity check. It is a surface boundary, and it is testable"* — and Conductor should not rediscover it by being exploited. Socket outside the agent's scope; no file-based grants; a workspace-facing shim from which the approval code path is provably absent.

**3. The `WorkflowRuntime` abstraction.** An interface with one implementation, whose purpose is to make adopting a backend we have argued against easier later. It leaks `claim`/`heartbeat`/`lease` into domain code and is the most likely single path to accidentally building Temporal. Keep the domain decisions pure instead; that is better portability insurance and it is testable today.

**4. "Exactly-once" left undefined.** The plan is rightly suspicious but never states the actual guarantee. Three tiers: at-least-once attempts, at-most-once Conductor-owned effects via precondition checks, and **no guarantee at all** for agent-caused effects.

**5. Prompt instructions listed alongside real controls.** §10.A treats "policy intent" as a layer. It is documentation. It should be labelled as worth zero enforcement so nobody later mistakes a well-written packet for a boundary.

### What I Would Remove

Materialized projections. The `WorkflowRuntime` interface. Agent selection. `Checkpoint`, `Failure`, `Continuation` as tables. `Milestone`/`Slice` as tables with stored status. Thirteen of the twenty-five CLI commands. Independent agent review as a verification type in v1. Five of the fourteen task states.

### What I Would Add

Credential and environment isolation as the *primary* control, not the fourth. `lease_epoch` fencing. Tree-hash binding of verification results. `binding_hash` for approvals. Failure fingerprints. Plans and decisions as repository files, with a "delete the database, lose no plan" test. Self-describing workspace descriptors so recovery works with no database at all. Submodule refusal. Determinism requirements for packets and policy hashes. A `SECURITY.md` whose prevented-vs-detected table has a passing test behind every "prevented." And Slice 0: measure five claims before committing to designs that depend on them.

### Where the Custom Runtime Could Become a Mistake

Three specific ways, all detectable early:

1. **Scope creep into scheduling.** The first priority queue, the first fairness heuristic, the first DAG. §25's containment rules exist for this.
2. **Schema evolution under long-suspended runs.** A run paused across a schema migration, with in-flight state that no longer type-checks. Rare at v1 durations; genuinely ugly at multi-week ones. Trigger §4.6(4).
3. **Concurrency bugs that are hard to reproduce.** Lease races, fencing gaps, `SQLITE_BUSY` under contention. The mitigation is that S1 and S3 make these the *primary* tested surface, and trigger §4.6(5) measures whether it is working.

If Conductor's own git log shows runtime bug-fixes exceeding 15% of commits over 30 days, the custom-runtime bet has lost and should be conceded rather than defended.

### Where Hatchet Could Be Better Than the Custom Runtime

If Conductor becomes a **team** product with a shared control plane. Hatchet's Postgres-backed durable tasks, dashboard, retry semantics and worker management are genuinely better than anything worth hand-rolling once multiple engineers share state and multiple machines run agents. Its dashboard alone would obsolete a meaningful chunk of §17.3. The moment Conductor stops being local-first, Hatchet becomes a serious option — its self-hosting footprint (API server, gRPC engine, Postgres, optional RabbitMQ, dashboard) stops being absurd once it is amortized across a team rather than imposed on one laptop with no container runtime.

### Where Temporal Could Be Better Than Both

Two cases. First, **very long-lived workflows** — a run suspended for weeks across daemon and OS upgrades. Temporal's versioning and replay machinery solves schema-evolution-under-suspension, which is the ugliest problem in hand-rolled durability. Second, **complex orchestration topologies** — fan-out across many repositories with joins, compensation, child workflows. Conductor v1 has a sequential task queue; if it ever grows a real dependency graph with parallel branches and rollback, hand-rolling that is a mistake, and Temporal is the mature answer. Its operational cost (Docker Compose, Postgres, Elasticsearch) is a real price, but at that complexity it is the cheaper of two prices.

### Why My Final Runtime Recommendation Wins for V1

Because of the ratio. Adopting either framework costs a container runtime this machine does not have, a Postgres instance, a control plane to operate and upgrade, and a programming model Conductor's domain logic must be shaped around — and it removes **two** of Conductor's ten hard problems (§4.1). The other eight — repository isolation, Git-truth reconciliation, verification authority, policy algebra, approval binding, agent supervision, evidence-based recovery, side-effect verification — get built either way.

And the deepest reason is structural rather than economic: **durable execution is not reconciliation.** Temporal guarantees your workflow reaches the same state given the same history. Conductor needs to determine the true state given whatever actually happened on disk — where the user may have edited the branch, the host may have rebooted, and the agent may have lied. A perfectly replayed workflow still has to go read `git status`. The expensive part is not made cheaper; it is made less visible.

Meanwhile the minimum owned runtime is small and, importantly, *bounded*: one claim query with fencing, a heartbeat, a startup scan, and an idempotency ledger. Roughly 1,500 lines, entirely testable with `kill -9`, on a foundation this developer has demonstrably shipped 130k lines of Rust against in ten days.

### What Evidence Would Change My Mind

- **On the runtime:** execution moving to more than one machine; >3 operators; >50 concurrent runs; runs suspended >30 days; or runtime bug-fixes exceeding 15% of commits over any 30-day window.
- **On clones:** S0/Q3 showing clone cost >10 s on the user's real repositories with no mitigation.
- **On the stack:** the first three slices showing schema plumbing dominating commits with no behaviour change.
- **On Codex-first:** S0/Q2 showing `--sandbox workspace-write` does not actually deny writes outside the workspace or network egress.
- **On the human boundary:** six months of review packets where the human's decision is `accept` >95% of the time — which would be evidence that review is theatre and the boundary should move.
- **On event sourcing:** finding myself repeatedly reconstructing state by hand from the event log because the state tables cannot answer "how did we get here."

### What the Baseline Must Address Before We Converge

1. **How is `conductor approve` protected from the agent it constrains?** Nerve already answered this class of question; the answer is a surface boundary, not an identity check. Until this is specified, the approval system's semantics do not matter.
2. **Does the plan accept that worktrees share config, refs, hooks and objects — and if it keeps worktrees, what specifically prevents an agent-written `pre-commit` hook from firing under the human later?**
3. **What is the exact execution guarantee, in tiers?** Not "we avoid duplicates" — what is guaranteed for Conductor's own effects, and what is explicitly *not* guaranteed for the agent's.
4. **Define "no progress" in repair, formally**, with a provable upper bound on agent invocations.
5. **What survives losing `conductor.db`?** If the answer includes plans or accepted decisions, the storage split is wrong.
6. **How does policy precedence handle a locked global rule versus a more restrictive project rule?** If the answer is "most restrictive wins," locking does nothing and should be removed rather than documented.
7. **What is verification a claim *about*?** If a result is not bound to a tree hash, what stops a green result from describing a tree that never existed?
8. **Which prompt-§10 operations are prevented and which are only detected, itemized — and for each "prevented," which mechanism and which test?**
9. **Which adapter is first, and which specific verified capability decides it?** Not preference — a flag that exists in the installed binary.
10. **What gets measured before the design is committed?** If the answer is nothing, the plan is being decided by argument in a repository whose sibling decides things by experiment.

---

## Appendix — Evidence Index

| Claim | Source |
|---|---|
| Repository is empty: 1 commit, 11-byte README | `gh api repos/Krish-Verma/conductor/git/trees/main?recursive=1` |
| Nerve: 157 commits, 10 days, 129,751 lines Rust, 54 test files | `git log`/`find`/`wc` in `/Users/krishverma/Documents/Nerve` |
| Nerve's agent-vs-human indistinguishability finding | `Nerve/docs/plans/slice-14-human-confirmed-memory.md` §1 |
| Nerve's hand-executed recovery procedure | `Nerve/docs/reports/restart-recovery-report.md` |
| Nerve stack and dependency discipline | `Nerve/docs/decisions/ADR-0001`, `ADR-0004`, `docs/plans/nerve-master-build-plan.md` §4 |
| Nerve CLI/MCP surface, exit codes, refusals | `Nerve/README.md`; `docs/plans/slice-08-mcp.md` |
| Claude Code flags and absence of `--permission-prompt-tool` | `claude --help`, v2.1.228, this machine |
| Claude Code structured output, sessions, hook events | `docs.claude.com/en/docs/claude-code/headless` |
| PreToolUse hooks deny tool calls | Observed directly in this session |
| Codex sandbox modes, `--output-schema`, resume, `--ephemeral` | `codex exec --help`, codex-cli 0.142.0; `developers.openai.com/codex/noninteractive` |
| Temporal self-host: Postgres + Elasticsearch via Compose | `docs.temporal.io/self-hosted-guide/deployment` |
| Temporal dev server loses state without `--db-filename` | `docs.temporal.io/cli/server` |
| Hatchet self-host: API server, engine, Postgres, RabbitMQ, dashboard; Lite needs Docker | `docs.hatchet.run/self-hosting`, `/self-hosting/hatchet-lite` |
| No container runtime on this machine | `which -a docker podman colima` → all absent |
| Rust 1.97.1, Node 24.15.0, SQLite 3.51.0, git 2.50.1, macOS 26.6 arm64 | measured |
| `git worktree` shares config/refs/hooks/objects | `git worktree --help`, git 2.50.1 — **flagged for empirical confirmation in Slice 0/Q1** |
