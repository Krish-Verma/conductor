# Conductor — Convergence Pass 01

**Date:** 2026-08-12
**Scope:** resolve the ten open architectural questions. No implementation.
**Method:** repository inspection + four executed experiments. Measurements below are from this machine, this session.

---

## 1. Current Conductor Repository Truth

Inspected via `gh api`, not from conversation memory.

| Property | Value |
|---|---|
| HEAD | `6b830311f21d6bd25d28e9ecf3513b5628affb06` |
| Refs | `refs/heads/main` only — no other branches, no tags |
| Commits | 1 — "Initial commit", 2026-08-03T18:45:28Z |
| Tree at HEAD | one blob: `README.md`, 11 bytes, content `# conductor` |
| `pushed_at` | 2026-08-03T18:45:28Z — **unchanged since the previous pass** |
| repo `size` | 0 |
| Code search for `runs` | `total_count: 0` |
| Local dir | one file: `CONDUCTOR-ARCHITECTURE-REVIEW.md` (153,847 bytes, written by me last turn) |

- **Implementation exists:** no.
- **Architecture docs in the repo:** no. The review document is local and uncommitted.
- **Database schema:** none.
- **Migrations:** none — no migration directory, no migration file, no schema file of any kind.
- **`runs` / `Runs` table:** **does not exist.** There is no SQL, no ORM, no schema, and no code in this repository at all. There is exactly one commit containing exactly one 11-byte Markdown file.
- **Still effectively greenfield:** yes, unchanged from the previous pass.

### 1.1 The "Runs table" sentence

**Verdict: B — context contamination. And a further correction: the sentence did not come from me.**

My previous response ended with a Sources list of six documentation URLs. The string "And thanks for getting the new Runs table applied" does not appear anywhere in my previous output — not at the end, not in the body, not in the architecture document I wrote. I did not write it, and I have no record of a `Runs` table in any Conductor reasoning.

So there are two separate things to discard, and it is worth keeping them separate:

1. **The claim that a Runs table exists.** False. Refuted by the repository above.
2. **The claim that I asserted it.** Also false, and worth naming, because if the premise had been accepted uncritically, the correct fix ("Claude leaked state, distrust its prior reasoning") would have been applied to the wrong target and the actual source of the contamination would have gone unexamined.

Plausible origin: another project in this workspace, or a summarization/handoff artifact between tools. `~/Documents` contains ISAAC, Kayna, LOGD, etoystore, neurogrip and Nerve; several are database-backed. Not worth chasing further — but worth noting that the leak entered *this conversation from outside it*, so watching my output for leakage would not have caught it.

**Action taken:** no `Runs` table, no schema, no migration, and no prior applied database state exists in any Conductor reasoning going forward. Nothing in the previous architecture document depended on one — the data model in §8.3 of that document is a proposal, explicitly unbuilt.

**Standing rule adopted:** any claim about Conductor's *existing* state gets verified against the repository before it is reasoned from, regardless of who asserted it, including me.

---

## 2. Nerve / Conductor Separation Correction

The separation is accepted without argument. Conductor and Nerve are different products; Nerve is a repository-intelligence system in the CodeGraph/GitNexus category; it may at most become one optional evidence provider.

**Where the previous review over-relied on Nerve.** Honestly: more than it should have. Nerve was the only substantial engineering artifact available, Conductor was empty, and I used the nearest available evidence rather than the correct evidence. Two failures resulted: I imported architectural *conclusions* (language, exit codes, MCP integration) where I should have imported at most *workflow observations*, and I placed Nerve inside the Conductor architecture diagram, which visually merged two products.

### 2.1 Reclassification

| # | Prior Nerve-derived claim | Verdict | Conductor-independent justification (or removal) |
|---|---|---|---|
| 1 | Rust, justified by Nerve's velocity | **REMOVE** | Velocity evidence deleted entirely. Decision re-derived from scratch in §6. Conclusion happens to survive; the reasoning is wholly replaced. |
| 2 | Plans stored in Git | **MODIFY** | Conclusion kept, reasoning replaced. Independent basis: an approved plan must survive loss of `conductor.db`, must be reviewable as a diff, must travel with the repository to another machine, and must be readable by a human without Conductor installed. Those are Conductor requirements derived in §7. That another project also keeps plans in files is a coincidence, not an argument. |
| 3 | Decisions stored in Git | **MODIFY** | Same basis as (2), plus: decisions are inputs to prompt packets and must be reconstructible without execution state. |
| 4 | CLI conventions (`--json` on every command) | **STILL VALID INDEPENDENTLY** | Conductor's CLI is consumed by scripts, by the future dashboard, and by packet generation. A machine-readable surface is a product requirement, not a style preference. |
| 5 | Exit-code conventions (0/2/3/4/10/70) | **REMOVE** | Those were Nerve's semantics for an indexing tool and do not fit Conductor. Replaced with a Conductor-specific set derived from Conductor's own outcomes, using `sysexits.h` conventions — an independent standard. See §2.2. |
| 6 | Recovery architecture | **MODIFY** | Nerve's recovery report was an illustration, not evidence. Independent basis: an agent's reasoning is unrecoverable after process death; the repository and filesystem are on disk and readable; therefore recovery must reconstruct from disk. This follows from Conductor's own execution model with no external reference. |
| 7 | Approval-surface architecture | **STILL VALID INDEPENDENTLY — and now measured** | The underlying truth (a same-user process is indistinguishable from a human at a local CLI) is a general property of Unix, not a Nerve finding. It is now replaced entirely by direct measurement of Conductor's own execution environment in §4. Upgraded from borrowed insight to experimental result. |
| 8 | Nerve MCP integration (`nerve mcp` to the agent, `nerve check` gate) | **REMOVE from core** | Core Conductor must not reference Nerve. Deferred to an optional adapter, post-v1, behind a generic provider interface. See §2.3 and §13. |
| 9 | Security patterns borrowed from Nerve (source-scan "code path is absent" test) | **STILL VALID INDEPENDENTLY** | Asserting that a code path is unreachable from a surface is a generic technique with no product ownership. Conductor needs it for its own reason: proving the approval verb is absent from any binary reachable by an agent. |
| 10 | ADR / pre-registered falsification practice | **MODIFY** | Kept, justified independently: this pass alone contained two experiments whose first design was wrong (§3, §4). A project making containment claims about a sandbox it has not tested needs falsifiable records. That is a property of Conductor's risk profile, not an inherited habit. |
| 11 | Determinism discipline (byte-identical serialization) | **STILL VALID INDEPENDENTLY** | Required mechanically: approval binding hashes and policy snapshot hashes are worthless if serialization is nondeterministic. Derived from §19's exact-action binding requirement. |
| 12 | SQLite | **MODIFY** | Kept; reasoning replaced. Independent basis: single-writer embedded store, transactional claim semantics, zero install, one file to back up, and it is the only serialization point Conductor needs. Not because another project chose it. |

### 2.2 Replacement exit codes (Conductor-derived)

Built from Conductor's own outcome classes, using `sysexits.h` for the standard slots:

```
0   success
1   generic failure
2   no project / not initialized / store unhealthy
3   action required — approval or review pending  (scriptable: "human needed")
4   policy denied
5   verification failed
64  usage error        (EX_USAGE)
70  internal error     (EX_SOFTWARE)
```

Code `3` earns a dedicated slot because "Conductor stopped and needs a human" is the single most common non-success outcome and must be distinguishable from failure by a wrapper script.

### 2.3 Final evidence-provider boundary

Core Conductor depends on exactly one provider and knows of no others:

```
                    ┌──────────────────────────────┐
                    │      Conductor Core          │
                    │  (no knowledge of any        │
                    │   specific provider vendor)  │
                    └──────────────┬───────────────┘
                                   │  trait RepositoryEvidenceProvider
                    ┌──────────────▼───────────────┐
                    │  GitFilesystemProvider       │  ← the ONLY provider in v1.
                    │  git + fs. Always present.   │    Not optional. Not swappable.
                    └──────────────────────────────┘

  ── everything below is post-v1 and does not exist yet ──
                    ┌──────────────────────────────┐
                    │  (optional) third-party      │
                    │  providers, incl. Nerve      │
                    └──────────────────────────────┘
```

Binding rules:

- The trait is **not written until a second provider is actually built.** Until then `GitFilesystemProvider` is a concrete module, not an implementation of anything. A one-implementation trait is a pure function wearing a costume (§9).
- Core contains no string `nerve`, no Nerve exit codes, no Nerve paths, no Nerve CLI invocation, no assumption about Nerve's language or layout.
- Conductor v1 completes, tests, dogfoods and ships with Nerve absent from the machine. An acceptance test runs the full suite with any provider binary removed from `PATH`.
- If a Nerve adapter is ever built it lives in a separate optional crate, is off by default, and its failure is never an execution failure.

**Diagram correction:** the previous document placed Nerve inside the Conductor architecture picture. That was wrong and is removed (§12).

---

## 3. Git Isolation Experiment Results

Executed. Script: `scratchpad/git-isolation-experiment.sh`. All numbers below are measured, macOS 26.6, APFS, git 2.50.1.

### 3.1 Experiment A — default local clone

Source: 70 files of incompressible data, one pack (3.6 MB) plus loose objects from a later commit.

```
source loose  51/9b6e3f…   inode=209190344  nlink=2
clone  loose  51/9b6e3f…   inode=209190344  nlink=2   >>> SAME INODE
source pack   pack-0ddcc…  inode=209190239  nlink=2   size=3,637,791
clone  pack   pack-0ddcc…  inode=209190239  nlink=2   >>> SAME INODE
```

**Both loose objects and pack files are hardlinked.** Object files are mode `-r--r--r--`, but the same user owns them, so `chmod u+w` from inside the clone succeeded immediately.

**Adversarial in-place mutation, executed from inside the clone** (16 bytes written at offset 8 with `dd conv=notrunc`):

```
BEFORE:  source `git fsck` exit=0

AFTER:   error: inflate: data stream error (invalid code lengths set)
         error: unable to unpack header of .git/objects/51/9b6e3f…
         error: 519b6e3f…: object corrupt or missing
         fatal: loose object 519b6e3f… is corrupt
         source `git fsck` exit=128
         source `git cat-file -p HEAD`: FAILED
```

**The source repository — the user's real repository — was corrupted by a write performed inside the clone.** HEAD became unreadable.

Deletion is safe: removing an object file in the clone left the source copy intact (`unlink` only decrements the link count). The danger is exclusively **in-place mutation of a shared inode**.

### 3.2 Experiment B — `--no-hardlinks`

```
src2 pack     inode=209191099  nlink=1
clone-b pack  inode=209191286  nlink=1   >>> DIFFERENT INODE
loose objects                            >>> DIFFERENT INODE

after corrupting clone-b's pack in place:
  src2 `git fsck` exit=0
  src2 `cat-file --batch-all-objects`: all objects readable
  >>> SOURCE UNAFFECTED
```

**Isolated.**

### 3.3 Experiment C — `--no-local`

Tested. Uses the transport path, no filesystem sharing. Correct but strictly slower than `--no-hardlinks` with no additional isolation benefit for a local source. Not the default.

### 3.4 Cost — measured at realistic scale

Source: an independent copy of a real 22 MB `.git` (155 Rust files, 157 commits). The copy was made with `--no-hardlinks` first, specifically so the real repository was never hardlinked into by this experiment.

| Variant | Wall time | Incremental disk |
|---|---:|---:|
| default (hardlink), `--no-checkout` | **1.47 s** | 3.6 MB |
| `--no-hardlinks`, `--no-checkout` | **0.59 s** | 23.3 MB |
| `--no-local`, `--no-checkout` | 0.94 s | ~22 MB |
| `--no-hardlinks`, **with checkout** | **0.76 s** | ~40 MB |

Small repo (4.2 MB `.git`): default 0.05 s, `--no-hardlinks` 0.04 s, `--no-local` 0.08 s.

**The result inverts the assumption in the previous document.** I had written that safety would cost latency and that large repositories might need a worktree opt-in. Measured, `--no-hardlinks` is **2.5× faster** than the hardlinking default at 22 MB. Hypothesis for why (not verified, and not load-bearing): hardlinking requires one `link()` syscall per object file, while a bulk copy on APFS can use `clonefile`, which is O(1) per file — so the "optimization" is slower precisely because it is per-object. Whatever the mechanism, the measurement stands.

### 3.5 Final decision

```bash
git clone --no-hardlinks --no-checkout "$SOURCE" "$WORKSPACE"
git -C "$WORKSPACE" checkout -b "conductor/$TASK_ID/$RUN_ID" "$BASE_COMMIT"
git -C "$WORKSPACE" remote remove origin
git -C "$WORKSPACE" config core.hooksPath /dev/null
git -C "$WORKSPACE" config user.name  "Conductor Agent"
git -C "$WORKSPACE" config user.email "conductor@localhost"
git -C "$WORKSPACE" config commit.gpgsign false
```

**`--no-hardlinks` is the secure default and there is no optimized mode.** I proposed one in the previous pass; the measurement removes the reason for it. Shipping a faster-but-unsafe mode that is in fact *slower* would be indefensible. Disk cost is ~23 MB per concurrent run at this repo size — at four concurrent runs, under 100 MB, which is not a constraint worth trading repository integrity for.

Residual shared state after this sequence: none in `.git`. The clone has its own config, refs, reflog, hooks path, object store and index. The remaining shared surface is the filesystem outside the workspace, which is §4's problem, not git's.

**Revisit trigger:** if a registered repository's clone exceeds 10 s, revisit — and revisit toward `--reference` with `--dissociate`, or a cached base clone refreshed by fetch, **not** toward hardlinks.

---

## 4. Approval-Security Reassessment

The critique is correct and my previous position was too strong. A `0600` unix socket does not distinguish a human from a same-user subprocess, and removing an environment variable is obscurity. I asserted a boundary I had not measured. Below is the measurement.

### 4.1 What was measured

Executed against `codex sandbox -c sandbox_mode=workspace-write` — Codex's own sandbox runner, invoked directly, **with no model in the loop**, so every result is deterministic and repeatable. Round 1 of this experiment was invalid twice over (workspace was under `/tmp`, which the policy permits; and the AF_UNIX test failed on `sun_path` length, not on the sandbox). Both were corrected and re-run.

**AF_UNIX control-surface reachability — three socket locations, all with a live listener:**

| Socket path | Result |
|---|---|
| `/tmp/cndA.sock` | `DENIED: PermissionError [Errno 1] Operation not permitted` |
| `$HOME/cndB.sock` | `DENIED: PermissionError [Errno 1] Operation not permitted` |
| `$HOME/.conductor-cndC.sock` | `DENIED: PermissionError [Errno 1] Operation not permitted` |

**Positive control, proving the deny is real and not an artifact of a broken test:**

```
codex sandbox -c sandbox_mode=workspace-write --allow-unix-socket /tmp/cndD.sock -- python3 connect.py
  → exit=0   CONNECTED: b'ALLOWED_VIA_FLAG\n'
```

The identical connect succeeds when the path is explicitly allowlisted and fails when it is not. **AF_UNIX connect is default-deny under seatbelt, kernel-enforced, and Conductor's control socket is unreachable from a Codex-sandboxed agent.** This is not obscurity — the agent knew the exact path in every denied case.

**Filesystem writes:**

| Target | Result |
|---|---|
| workspace (cwd) | ALLOWED |
| sibling dir under `$HOME` | **PREVENTED** |
| `$HOME` root | **PREVENTED** |
| `$HOME/Documents` | **PREVENTED** |
| `/Users/Shared` | **PREVENTED** |
| `~/.codex` (its own auth dir) | **PREVENTED** |
| `~/.ssh` | **PREVENTED** |
| `~/.claude` | **PREVENTED** |
| `/tmp` | **ALLOWED** |
| `$TMPDIR` | **ALLOWED** |
| nested `sh -c` child writing outside | **PREVENTED** (restriction is inherited) |

**Filesystem reads — unrestricted:**

| Target | Result |
|---|---|
| planted secret outside workspace | read successfully |
| `~/.codex/auth.json` | read successfully — returned `{"auth_mode":"chatgpt","OPENAI_AP…` |
| `~/.ssh/known_hosts` | read successfully (11 lines) |

**Network:**

| Test | Result |
|---|---|
| TCP connect `1.1.1.1:443` | `PermissionError [Errno 1] Operation not permitted` |
| DNS resolve `github.com` | fails — `nodename nor servname provided` |
| `curl https://example.com` | `curl: (6) Could not resolve host` |

**Other:** exit codes propagate faithfully (`exit 42` → 42). Default `codex sandbox` mode with no override denied a write to its own cwd, so **read-only is the default and `workspace-write` must be opted into.**

### 4.2 What this means for approval integrity

**"Human-only approval" is a property of the execution mode, not of Conductor.** Stated per mode:

**Codex, `--sandbox workspace-write`:** the control surface is genuinely unreachable. An agent that knows the socket path, runs as the same user, and calls `connect()` directly is refused by the kernel. Approval integrity here is **enforced**. Additionally the agent cannot write to `$HOME`, so it cannot squat or replace the socket file if Conductor places it under `$HOME/.conductor/` — which is now a requirement, not a preference.

**Claude Code, unwrapped:** no sandbox exists. The agent can `find / -name '*.sock'`, connect to anything the user can, read `conductor.db`, and read Conductor's config. Environment scrubbing and `0600` are obscurity. Approval integrity here is **absent** — not weak, absent. Any design that pretends otherwise is a false security claim.

**The material finding: containment is a property of the launcher, not the agent.** `codex sandbox` successfully wrapped a foreign binary — `codex sandbox -c sandbox_mode=workspace-write -- claude --version` returned `2.1.228 (Claude Code)`, exit 0. And `/usr/bin/sandbox-exec` exists on this machine. So Conductor is not forced to accept an adapter's native tier; it can **supply** containment. That decouples "which agent" from "what enforcement" and is the cleanest way to make Claude Code eligible for sensitive work later.

### 4.3 How human-only approval is actually enforced

Three tiers, stated honestly, with no tier described as stronger than it is:

| Tier | Mechanism | Integrity |
|---|---|---|
| **A. Sandboxed** | seatbelt AF_UNIX default-deny; socket under `$HOME/.conductor/` (unwritable by the agent) | **Enforced.** Measured, with positive control. |
| **B. Unsandboxed + operator nonce** | grant requires a nonce Conductor prints **only** to the controlling terminal; only `hash(nonce)` is persisted, so reading `conductor.db` does not yield it | **Raises cost substantially; not a kernel boundary.** An agent would have to target the operator's TTY specifically. |
| **C. Unsandboxed, no nonce** | socket permissions and env scrubbing only | **Not a boundary. Approvals are advisory.** |

**Binding rule:** a task whose policy can produce an approval gate **may not run unattended** in a mode below tier A. This is enforced by the eligibility check in §5, not by documentation.

**Limitations, stated plainly:**
- Under tier A, the agent can still **read** any file the user can, including `~/.codex/auth.json` and `~/.ssh`. Exfiltration requires a second stage, because network is denied — but `/tmp` is writable, so staging secrets for later pickup is possible and only **detectable**, not preventable.
- `/tmp` and `$TMPDIR` being writable is a real hole (see §11.2 for the proposed fix).
- All of the above is measured for macOS 26.6 with codex-cli 0.142.0. It is not a claim about other platforms or versions, which is why §5 requires the capability vector to be **measured on the host at runtime**, never hardcoded.

---

## 5. Execution-Security Capability Model

Minimal. Four gating dimensions, four levels. Deliberately smaller than the six proposed.

### 5.1 What was cut and why

- **`repository_isolation` removed as a dimension.** After §3 it is a constant Conductor guarantees itself via `--no-hardlinks` clone, identically for every adapter. A dimension whose value never varies is not a dimension; it is an invariant. Asserted by the §3 byte-identity test instead.
- **`tool_interception` kept but marked non-gating.** Hooks are auditing and accident-prevention, with known bypasses (`sh -c`, script-then-execute, alternate spellings). Allowing a policy to *require* it would let a non-boundary satisfy a boundary requirement, which is the exact confusion this model exists to prevent. Recorded for observability; never satisfies an `execution_requirements` clause.

### 5.2 The model

```rust
enum Enforcement {
    Hard,        // kernel-enforced deny, verified by probe with a positive control
    Restricted,  // enforced, with a known and enumerated exception set
    AuditOnly,   // not prevented; reliably detected after the fact
    None,        // neither prevented nor detected
}

struct ExecutionCapabilities {
    filesystem_write:  Enforcement,  // can it write outside the workspace?
    network_egress:    Enforcement,  // can it reach the network?
    control_surface:   Enforcement,  // can it reach Conductor's approval socket?
    credential_read:   Enforcement,  // can it read credentials on this host?
    // informational only, never gates:
    tool_interception: Enforcement,
    exceptions:        Vec<PathBuf>, // populated for Restricted, e.g. ["/tmp", "$TMPDIR"]
}
```

**These values are measured, not declared.** `conductor doctor --containment` runs the §4 probe suite against each configured (adapter × launcher) pair on the actual host and caches the result keyed by `(adapter_version, launcher_version, os_version)`. A stale or absent probe result forces the vector to `None` across the board — fail closed. This is the single most important property of the model: sandbox behaviour varies by OS and CLI version, and a hardcoded table would silently become a lie after an upgrade.

### 5.3 Measured classification

| | FakeAgent | **Codex** `exec --sandbox workspace-write` | **Claude Code** (bare) | Claude wrapped in a sandbox launcher |
|---|---|---|---|---|
| `filesystem_write` | n/a | **Restricted** — exceptions `/tmp`, `$TMPDIR` | **None** | *unverified* — expected Restricted |
| `network_egress` | n/a | **Hard** | **None** | *unverified* — expected Hard |
| `control_surface` | n/a | **Hard** (measured, positive control) | **None** | *unverified* — expected Hard |
| `credential_read` | n/a | **None** — reads unrestricted | **None** | *unverified* — expected None |
| `tool_interception` | n/a | not investigated | **Restricted** (`PreToolUse`, bypassable) | Restricted |

FakeAgent is Conductor's own code and not an adversary; it is exempt, and its harness asserts whatever the scenario requires. Recording it as `Hard` would be a category error.

The wrapped-Claude column is **explicitly unverified**. `codex sandbox` wrapping `claude --version` succeeded, which establishes the mechanism works for a foreign binary; it does not establish that a full Claude run functions correctly under it (Claude needs network for the API, and `network_egress: Hard` would break it outright — so a Conductor-authored `sandbox-exec` profile permitting the API endpoint but nothing else is the likely shape). That is a research task, not a v1 commitment.

### 5.4 Eligibility check — the whole subsystem

```yaml
# .conductor/project.yaml  (or per-task override)
execution_requirements:
  filesystem_write: restricted   # minimum acceptable
  control_surface:  hard
```

```
before launching an attempt:
    caps = probe_cache.get(adapter, launcher, host)        // None if stale/absent
    if any required dimension > caps dimension:
        refuse to launch unattended
        emit reason naming the dimension, the requirement, and the measured value
        offer: attended mode | different adapter | a sandbox launcher
```

That is roughly fifty lines and one table. It is not a scheduler, it does not rank adapters, and it does not choose between eligible options — if more than one adapter qualifies, the configured one is used. Any growth beyond "compare a vector, refuse or proceed" is scope creep.

---

## 6. Independent Language Decision

Nerve's implementation language and velocity are removed from evidence entirely. Re-derived from Conductor's own properties.

**The strongest Conductor-specific argument was not in the previous analysis at all**, and it emerges from a premise in this prompt: *substantial implementation will be agent-assisted.*

That cuts against TypeScript rather than for it. The usual case for TypeScript is human iteration speed — fewer keystrokes, less ceremony, faster edit-run loops. When an agent writes most of the code, keystroke cost largely stops being a human cost. What does *not* transfer to the agent is verification: someone still has to establish that the generated code is correct. A compiler that mechanically rejects a missing state transition, an unhandled error, or a moved value is an automated reviewer that never tires and never approves out of fatigue.

Conductor's entire product thesis is *do not trust an agent's self-report; verify mechanically against ground truth.* Building that system in the weaker of two available verification regimes, because the agent writing it would type less, is self-contradictory at the level of the product.

Then the ordinary comparison, Conductor-specific:

**Where Rust genuinely pays here.** Conductor's core is six state machines with "invalid transition" as an explicit deliverable — exhaustive `match` turns adding a state into a compile error at every site, whereas TypeScript's equivalent is opt-in per switch and silently degrades when someone forgets. Child-process lifecycle is Conductor's most leak-prone surface (spawn, stream, timeout, `SIGTERM`→`SIGKILL`, reap, orphan-detect, on every exit path including panics); ownership and `Drop` make "always reaped" structural, while Node's `child_process` accumulates zombies and dangling handles in long-running daemons in ways that surface as mysterious non-exit. And Conductor is the tool you run when the environment is broken, so a single static binary that does not depend on a working runtime install is worth more than usual.

**Where Rust genuinely hurts here, stated fairly.** Agent CLIs are unstable protocols — the Claude Code docs alone cite behavioural changes at v2.1.163, .182, .205, .211, .219, .221, .223. Tolerating unknown JSONL fields is free in TypeScript and requires deliberate serde design in Rust. Compile times will slow the agent's edit-verify loop; at scale this is 30–60 s per `cargo check`, which is a real tax on an agent-driven workflow and the most likely source of regret. Mitigations: workspace crates so checks are incremental, `cargo check` rather than `cargo build` in the loop, and treating adapter parsing as an explicitly permissive layer (`#[serde(flatten)]` catch-alls, never `deny_unknown_fields` on agent input).

The tie-breaker is that Rust's costs concentrate in one place — the adapter parsing layer, which is small, isolated, and testable against recorded fixtures with no process running — while Rust's benefits concentrate in the state machines, process supervision and transaction boundaries, which is where an error is silent, durable and expensive.

```text
FINAL LANGUAGE RECOMMENDATION: RUST
```

**Top five reasons, all Conductor-specific:**

1. **The code is largely agent-written, so mechanical verification is worth more, not less.** A product whose thesis is "verify, don't trust the agent" should be built where the compiler enforces the most.
2. **Six state machines with an explicit no-invalid-transition requirement.** Exhaustive matching converts that requirement from a test you must remember to write into a compile error you cannot ignore.
3. **Child-process lifecycle is the highest-leak surface in the system.** Ownership and `Drop` make "always killed, always reaped, on every path" structural rather than disciplinary.
4. **Conductor is the recovery tool.** It must run when the environment is broken; a single static binary with no runtime dependency is a functional requirement, not packaging taste.
5. **Costs are isolated and benefits are diffuse.** Protocol churn hurts in one small, fixture-testable layer; correctness pays across supervision, transactions, policy and recovery.

**Falsification trigger, pre-registered:** if after slices S1–S5 more than 30% of commits are type/serde plumbing with no behaviour change, or if median `cargo check` in the agent loop exceeds 90 s, the decision is wrong and should be reversed while reversal is still cheap.

---

## 7. Storage Split Decision

### Git repository — `<repo>/.conductor/` (committed, authoritative)

```
.conductor/
├── project.yaml            identity, adapter, scope defaults, review cadence,
│                           execution_requirements
├── policy.yaml             project policy rules
├── verification.yaml       check profiles, toolchain fingerprint commands
├── plans/
│   └── v3/
│       ├── plan.yaml       milestones, slices, tasks, acceptance criteria,
│       │                   scope globs, verification bindings
│       └── APPROVED        plan content hash, approver, timestamp, policy hash
└── decisions/
    └── D-0007-clone-not-worktree.md
```

Authoritative for: **what we agreed to do, and what we are allowed to do.** Project intent, approved plans, durable decisions, policy, verification definitions.

### SQLite — `~/.local/share/conductor/conductor.db` (local, disposable)

Authoritative for: **what actually happened, and what is happening now.** Tasks, runs, attempts, leases, fencing epochs, event journal, verification executions and their tree bindings, approval requests and grants, findings, side-effect intents and receipts, adapter/session metadata, containment probe cache.

### Local artifact storage — `~/.local/share/conductor/artifacts/<run-id>/`

Content-addressed, never committed, gitignored by `conductor init`: generated packets, agent JSONL streams, raw verification logs, diffs, generated reports, review packets.

### 7.1 The ten questions, answered

1. **Must survive `conductor.db` deletion:** approved plans, decisions, project policy, verification definitions, project identity. All in git. Confirmed as an invariant with an acceptance test.
2. **Must survive moving to another machine:** the same set, plus git history. Execution state must *not* travel — a lease or workspace path from another machine is actively misleading.
3. **Needs git history/review:** plans, decisions, policy, verification config. These are the things a human argues about in a diff.
4. **Local/transient:** everything in SQLite and artifacts.
5. **May contain sensitive content:** agent JSONL streams (file contents, possibly secrets), verification logs, diffs, generated prompts. Never committed; secret-scanned before entering any packet; retention-bounded.
6. **Should be content-addressed:** packets, reports, diffs, policy snapshots, verification logs, plan content. Everything whose identity is its content.
7. **Should approval history survive DB loss?** **Partially, and deliberately so.** Full request/grant history is execution history and dies with the DB. But an approval that authorized a *lasting* effect leaves a durable trace: **Conductor-owned commits carry git trailers.**

   ```
   Conductor-Run: r-0041
   Conductor-Plan: v3@blake3:9ac2…
   Conductor-Policy: blake3:41ef…
   Conductor-Approval: AG-0019 binding=blake3:7d31…
   Conductor-Verification: blake3:5b8e…
   ```

   Git history then carries the audit trail for anything consequential, surviving total local loss and travelling with the repository. Ephemeral approvals that authorized nothing lasting are not worth durable storage.
8. **Should execution history survive DB loss?** No, and it should not be engineered to. Losing "attempt 2 timed out on 2026-08-09" costs nothing. Engineering durability for it would push toward committing execution state, which pollutes the repository and leaks agent output into git.
9. **Plan approval in Git, SQLite, or both?** **Git is authoritative; SQLite indexes it; a mismatch is a hard error, never a resync.**

   This has a security consequence I did not address in the previous pass and which must be handled: **`.conductor/` lives inside the repository, so an agent can write `.conductor/plans/v3/APPROVED` inside its own clone.** Three controls, all required:
   - `.conductor/**` is in the always-forbidden write scope; any change to it arriving on a run branch is rejected at reconciliation, unconditionally, with a finding. Conductor never fetches such a change.
   - Conductor reads plan approval **only** from the registered repository's own working tree, never from a run branch.
   - The DB records the approval independently at grant time; if the file and the DB disagree, execution halts rather than picking a winner.
10. **Recovery after total local state loss:** re-register the project; read `.conductor/` for plans, decisions, policy, verification; rebuild the task list from the approved plan; scan `workspaces/` for `.conductor-run.json` descriptors and reconcile each against git; read Conductor trailers in git history to reconstruct which runs produced which commits under which approvals. Execution history is gone; project truth and the audit trail for consequential actions are intact.

### 7.2 What is lost if `conductor.db` disappears

Lost: run/attempt history, timings, event journal, verification result cache (recomputable), pending approval requests, findings not yet resolved, lease state, containment probe cache (re-probeable).

Not lost: every approved plan, every decision, all policy, all verification definitions, project identity, and — via commit trailers — which run, plan version, policy snapshot and approval produced every Conductor-authored commit.

**The invariant holds and I keep it**, on the independent grounds above rather than by analogy to any other project.

---

## 8. Human/Machine Representation Decision

**Plans, policy, verification config → canonical YAML. Decisions → Markdown with YAML frontmatter. Nothing is generated into a second committed file.**

Reasoning per artifact, from what each actually is:

- **A plan is a data structure** — milestones containing slices containing tasks, each with acceptance criteria bound to named verification checks, scope globs and dependencies. Its prose is confined to `rationale:` and `objective:` fields. Markdown-with-frontmatter would put the load-bearing structure in the frontmatter and the decoration in the body, i.e. exactly inverted. YAML is canonical.
- **Policy and verification config are configuration.** No argument needed; YAML.
- **A decision is an argument.** The metadata (`id`, `status`, `supersedes`, `date`) is small and fixed; the value is prose that a human reads and a planning packet quotes. Markdown body with YAML frontmatter is canonical, and the frontmatter is schema-validated.

**No generated duplicates.** The temptation is to render plans to Markdown for readability and commit both. That creates two representations that silently disagree the first time someone edits the wrong one. `conductor plan show` renders on demand, to stdout, never to a tracked file.

**Hashing — hash semantics, not bytes.** The plan hash is computed over the *parsed and canonically re-serialized* content: keys sorted, LF endings, no trailing whitespace, no timestamps. Consequences, both intended:

- Reformatting or re-indenting a plan does **not** invalidate its approval.
- Changing any field, anywhere, **does** invalidate it.
- YAML comments are excluded from the hash, and therefore **must not carry meaning.** Anything that matters goes in a `rationale:` field. `conductor plan validate` warns on comments longer than a threshold, because a long comment is usually load-bearing prose in the wrong place.

**Stable IDs:** `M-01`, `S-05`, `T-0012`, `D-0007`, assigned once and never reused. A plan revision that keeps a task's meaning keeps its ID; a task whose meaning changes gets a new ID and the old one is `SUPERSEDED`. `plan validate` refuses duplicate IDs, dangling `verified_by` references, forward dependencies, scope globs matching no path, and verification IDs absent from `verification.yaml`.

---

## 9. Runtime Boundary Decision

Confirmed, with one refinement.

**Retained:** pure domain functions with no I/O; narrow interfaces only at real I/O seams; **no `WorkflowRuntime`.** The principle "abstract known domain seams, not hypothetical future infrastructure" is right and I adopt it.

**Refinement — four of the eight proposed seams are not seams.**

A trait earns existence when it has either (a) more than one real implementation, or (b) I/O that tests need to fake. Measured against that:

| Proposed | Verdict |
|---|---|
| `PolicyEvaluator` | **Not a trait.** Pure function, no I/O, one implementation. `evaluate(snapshot, action, facts) -> Effect + Explanation`. Making it a trait adds indirection and removes nothing. |
| `RunStore`, `ApprovalStore`, `ArtifactStore` | **Not three traits.** One `Store` over one SQLite database in one transaction domain. Splitting them invites a write that spans two "stores" and therefore two transactions, which is precisely the bug class §10 of the architecture document exists to prevent. Split only if a second backing store ever appears. |
| `RepositoryEvidenceProvider` | **Not yet.** One implementation (git + fs) and no second one planned in v1. Concrete module now; trait when a second provider is actually built (§2.3). |
| `WorkspaceProvider` | **Real seam.** Tests need a fake; clone-vs-alternative may genuinely vary. |
| `AgentAdapter` | **Real seam.** FakeAgent, Codex, Claude — three implementations, and the fake is load-bearing for the entire test suite. |
| `Verifier` | **Borderline; defer.** One implementation (spawn a command). Tests can use fixture commands rather than a fake. Introduce only if a second execution strategy appears. |

So v1 has **two traits — `WorkspaceProvider` and `AgentAdapter` — plus one concrete `Store`,** and everything else is functions. The rest emerge from implementation slices or never.

**The pure core, restated as the actual portability insurance:**

```rust
next_action(state: &TaskState, evidence: &Evidence) -> Decision
reconcile(baseline: &Baseline, observed: &Observed) -> Reconciliation
evaluate(snapshot: &PolicySnapshot, action: &Action, facts: &Facts) -> (Effect, Explanation)
classify(attempt: &AttemptEvidence) -> AttemptOutcome
progressed(prev: &Failure, next: &Failure) -> bool
eligible(required: &Requirements, measured: &ExecutionCapabilities) -> Eligibility
```

Total functions, no I/O, exhaustively testable without a process, a database or a repository. If the execution substrate ever changes, these are unaffected — which is stronger insurance than an interface guessing at what the substrate will need.

---

## 10. Final Convergence Table

| Decision | Previous Claude recommendation | ChatGPT critique | Final recommendation | Confidence | Evidence |
|---|---|---|---|---|---|
| Conductor/Nerve relationship | Nerve integrated fairly deeply; `nerve mcp` handed to agent; `nerve check` gate; placed inside core diagram | Over-reliance; separate products; Nerve at most an optional provider | **Separate products.** Core knows only `GitFilesystemProvider`. Nerve becomes an optional post-v1 adapter, never a dependency. Removed from the architecture diagram | High | §2; accepted product context |
| Custom runtime | Build it | Agrees | **Build it.** Minimum owned runtime only | High | §4 of prior doc; unchanged |
| Hatchet / Temporal | Reject for v1 | Agrees; don't reopen | **Rejected for v1.** Triggers retained | High | Self-host footprints; no container runtime on host |
| Language | Rust, partly justified by another project's velocity | Remove that evidence; re-derive | **Rust**, re-derived. Decisive new argument: agent-written code raises the value of mechanical verification | Medium-High | §6; falsification trigger pre-registered |
| SQLite | Keep | Agrees | **Keep.** WAL, `synchronous=FULL`, `BEGIN IMMEDIATE` | High | §7 |
| Event journal | Append-only evidence log | Agrees | **Keep as evidence log** | High | §17 of prompt |
| Materialized projections | Delete | Agrees | **Deleted.** Plain mutable state, reconciled against git | High | Ground truth is external |
| Worktree vs clone | Per-run local clone | Agrees; but hardlinks untested | **Per-run clone** — confirmed, and now for a measured reason | High | §3.1: clone corrupted the source repo |
| Hardlinks | Accepted default hardlinking; proposed a worktree opt-in for large repos | Suspected unsafe; proposed `--no-hardlinks` | **`--no-hardlinks` mandatory. No optimized mode.** ChatGPT's hypothesis confirmed and strengthened: it is also 2.5× *faster* | High | §3.1–3.4, measured |
| Credential isolation | Primary control | Agrees | **Primary control**, plus: reads are *not* contained even under sandbox — stated as a limitation | High | §4.1: `auth.json` readable |
| Approval surface | Socket + env removal described as a boundary | `0600` doesn't distinguish same-user; env removal is obscurity | **Correct — I overclaimed.** Now: three explicit tiers; enforced *only* under a sandboxed launcher, proven by measurement with positive control | High | §4.1–4.3 |
| Adapter security tiers | Informal | Wants explicit capability model | **Four gating dimensions × four levels, measured at runtime, fail-closed.** `repository_isolation` cut (invariant); `tool_interception` non-gating | High | §5 |
| First real adapter | Codex, pending containment tests | Keep falsifiable | **Codex confirmed.** Containment measured: network Hard, control surface Hard, filesystem Restricted | High | §4.1 |
| Policy precedence | Locked ceiling + `max` join; unknown denies | No objection | **Unchanged** | High | Prior doc §6.2 |
| Plan storage | Git, justified by analogy | Re-justify independently | **Git**, re-justified from db-loss survival, diff review, portability. Plus new control: `.conductor/**` rejected from run branches | High | §7.1(9) |
| Decision storage | Git | Same | **Git**, Markdown + validated frontmatter | High | §8 |
| Verification tree binding | Bind to tree hash | Keep | **Keep** | High | Prior doc §14.2 |
| Verification outcomes | PASS/FAIL/INCONCLUSIVE/VOID | Keep | **Keep** | High | — |
| Repair-loop detection | Fingerprint + subset + oscillation | Keep | **Keep**, provable invocation bound | High | — |
| Lease fencing | `lease_epoch` | Keep | **Keep** | High | — |
| Side-effect idempotency | Intent → precondition → act → receipt | Keep | **Keep.** Plus git trailers as durable audit | High | §7.1(7) |
| `WorkflowRuntime` abstraction | Remove | Agrees | **Removed.** Further: 4 of 8 proposed seams also cut | High | §9 |
| Daemon timing | Foreground first | Agrees; reconfirm once | **Reconfirmed.** Daemon only after fake-agent recovery, leases, fencing, startup reconciliation and side-effect recovery all pass | High | §13 |
| Nerve integration timing | Mid-roadmap (S16) | Must be optional and late | **Post-v1**, after dogfooding. Never a prerequisite for running, testing, or completing v1 | High | §2.3, §13 |
| Dashboard timing | Last, read-only first, no dashboard-only mutation | Agrees | **Unchanged** | High | — |

---

## 11. Remaining Genuine Disagreements

Five. Everything else converged.

### 11.1 Is Claude Code eligible for unattended execution in v1 at all?

**Question:** measured bare, Claude Code is `None` on all four gating dimensions. Does it ship in v1 as an unattended adapter?

**Claude position:** No. Bare Claude Code should be eligible only for tasks whose policy cannot produce an approval gate and which touch no policy-sensitive surface — in practice, a small set. Making it broadly eligible would mean either weakening `execution_requirements` until it passes (defeating §5) or accepting advisory-only approvals for real work.

**ChatGPT position:** Claude Code is adapter 2 with useful capabilities (hooks, structured output, session IDs, spend caps), tier determined by measured reality.

**Not actually far apart** — the disagreement is only whether "adapter 2 exists" implies "adapter 2 runs sensitive tasks unattended." It does not.

**Evidence required:** whether a Conductor-authored `sandbox-exec` profile (or `codex sandbox`) can host a full Claude run while permitting only the Anthropic API endpoint. `network_egress: Hard` would break Claude outright, so the profile needs a hole exactly the size of one endpoint — and whether seatbelt can express that precisely enough is unknown to me.

**Recommended temporary choice:** ship Claude Code as adapter 2 with its measured tier (`None` across the board), let it run attended and for non-sensitive tasks, and treat "sandboxed Claude" as a separate, later, measured slice. Do not weaken `execution_requirements` to accommodate it.

### 11.2 `/tmp` and `$TMPDIR` are writable under Codex's sandbox

**Question:** is that acceptable for v1?

**Claude position:** Not as-is. It permits secret staging (read `~/.ssh` → write `/tmp`, pick up later) and cross-run interference between concurrent Conductor runs sharing `/tmp`. The cheap fix is to set `TMPDIR` per run to a directory *inside* the workspace, so ordinary temp usage stays contained; `/tmp` proper remains writable and becomes an audited surface — post-run, Conductor diffs `/tmp` for files created during the attempt window and raises a finding.

**ChatGPT position:** not yet stated.

**Evidence required:** whether Codex's `workspace-write` policy honours an inherited `TMPDIR` or hardcodes the system temp paths into the seatbelt profile. Measurable in one test.

**Recommended temporary choice:** set per-run `TMPDIR` inside the workspace; add `/tmp` delta scanning to the post-run audit; document the residual hole in `SECURITY.md`.

### 11.3 Unrestricted credential *reads* under the sandbox

**Question:** the agent can read `~/.codex/auth.json` and `~/.ssh` even under `workspace-write`. Acceptable for v1?

**Claude position:** Acceptable but must be stated, not glossed. Network egress is `Hard`, so reading a secret does not by itself exfiltrate it; the realistic risk is staging plus a later human-triggered egress. The mitigations that actually work are a per-run `HOME` (so `~/.ssh` and `~/.aws` are not at the expected paths) and secret scanning of every artifact and diff. Neither prevents a determined read of an absolute path.

**ChatGPT position:** not yet stated.

**Evidence required:** whether a per-run `HOME` is compatible with the adapters' own auth (Codex reads `CODEX_HOME`, which may or may not follow `HOME`).

**Recommended temporary choice:** per-run `HOME`, adapter auth pinned via its own dedicated variable to a path outside the redirected `HOME`, secret scanning on all artifacts, and an explicit `SECURITY.md` row reading "credential read: not prevented."

### 11.4 Operator nonce for unsandboxed approvals — v1 or deferred?

**Question:** ship tier B (§4.3) in v1, or only tiers A and C?

**Claude position:** Ship the *mechanism* in v1 because it is small (print a nonce to the TTY, store only its hash, require it on grant), but keep it **off by default**, enabled only when `control_surface < Hard` and a task nonetheless needs an approval. Building it later means retrofitting the approval grant path, which is exactly the code where retrofits cause security bugs.

**ChatGPT position:** not yet stated.

**Evidence required:** none architectural; this is a cost/benefit judgement.

**Recommended temporary choice:** build the mechanism in the approval slice, default off, activated by the eligibility check.

### 11.5 Does `plan validate` refuse, or warn, on a semantically-empty plan?

**Question:** `plan validate` refuses dangling references, duplicate IDs and forward dependencies. Should it also refuse a plan whose acceptance criteria are unfalsifiable prose ("works correctly") not bound to any check?

**Claude position:** Refuse. An acceptance criterion with no `verified_by` binding is the mechanism by which a task reaches `COMPLETE` on an agent's word — the exact failure the product exists to prevent. Every criterion must name at least one check.

**ChatGPT position:** not yet stated; the prompt's plan schema does not require the binding.

**Evidence required:** none; it is a policy choice about strictness.

**Recommended temporary choice:** refuse. It is trivially satisfiable (bind to a check, or mark the criterion `manual: true` and force a review boundary) and it closes the hole by construction.

---

## 12. Architecture Document Delta

Changes to `CONDUCTOR-ARCHITECTURE-REVIEW.md`. Unchanged sections are not listed.

```
Section: §1.3 (Nerve as primary evidence)
Old decision: Nerve's plans/ADRs/velocity used as the empirical foundation for several recommendations.
New decision: Demote to a single paragraph of workflow context, explicitly labelled as
              non-architectural. Every conclusion re-justified from Conductor's own requirements.
Reason: Different product. Borrowed conclusions are not evidence. §2.
```
```
Section: §5.1 (architecture diagram)
Old decision: Nerve shown inside the Conductor architecture as "Evidence(Nerve, optional)".
New decision: Removed. Diagram shows GitFilesystemProvider only; third-party providers appear
              in a separate post-v1 figure.
Reason: The diagram visually merged two products. §2.3.
```
```
Section: §5.2 (language)
Old decision: Rust, with "measured velocity" from another repository as reason #2.
New decision: Rust, with five Conductor-specific reasons; velocity evidence deleted; new primary
              argument (agent-written code raises the value of mechanical verification);
              falsification trigger added.
Reason: §6.
```
```
Section: §7.1 (approval boundary)
Old decision: "Socket outside the agent's scope + env removal" presented as the boundary.
New decision: Rewritten as three explicit tiers. Enforced ONLY under a sandboxed launcher,
              with the measurement and its positive control cited. Unsandboxed = advisory,
              stated plainly. Nerve citation removed.
Reason: I overclaimed. 0600 does not distinguish same-user processes. §4.
```
```
Section: §7.2 / §7.5 (threat layers, sandbox)
Old decision: Codex sandbox "must be measured, not assumed".
New decision: Replaced with the measured matrix: filesystem Restricted (exceptions /tmp, $TMPDIR),
              network Hard, AF_UNIX Hard, credential reads None. Positive control documented.
Reason: §4.1. The pre-registered measurement was performed.
```
```
Section: NEW §7.9 (containment is a property of the launcher)
New decision: Add. `codex sandbox` wraps foreign binaries; /usr/bin/sandbox-exec exists.
              Conductor may supply containment rather than inherit an adapter's tier.
Reason: Decouples enforcement from adapter choice; the path to making Claude eligible later. §4.2.
```
```
Section: NEW §7.10 (execution-security capability model)
New decision: Add. Four gating dimensions, four levels, measured at runtime by
              `conductor doctor --containment`, cached by (adapter, launcher, os) version,
              fail-closed when stale. Plus the eligibility check.
Reason: §5. Conductor must not treat weaker modes as equivalent to stronger ones.
```
```
Section: §8.1 (storage split)
Old decision: Plans in repo, justified partly by analogy.
New decision: Same split, re-justified from db-loss survival / diff review / portability.
              ADD: `.conductor/**` is an always-forbidden write scope; changes to it arriving
              on a run branch are rejected at reconciliation; plan approval is read only from
              the registered repo, never from a run branch; DB/file mismatch halts.
Reason: §7.1(9). `.conductor/` is inside the repository and therefore inside the agent's workspace.
```
```
Section: NEW §8.6 (git trailers as durable audit)
New decision: Add. Conductor-owned commits carry Conductor-Run / Plan / Policy / Approval /
              Verification trailers.
Reason: Makes the audit trail for consequential actions survive total local state loss. §7.1(7).
```
```
Section: §11.2 (clone strategy)
Old decision: `git clone` (default, hardlinked), with hardlink risk noted as theoretical and a
              worktree opt-in proposed for large repositories.
New decision: `git clone --no-hardlinks --no-checkout` mandatory. No optimized mode.
              Measured corruption demonstration and timing table inserted.
Reason: §3. Default clone let the run corrupt the source repository; --no-hardlinks is 2.5× faster.
```
```
Section: §12.2 (capabilities struct)
Old decision: Capability flags describing adapter features.
New decision: Split in two — functional capabilities (resume, schema output, session id) stay;
              security capabilities move to the measured ExecutionCapabilities model.
Reason: Conflating "can resume" with "contains writes" hides the distinction that matters. §5.
```
```
Section: §16 (Nerve boundary)
Old decision: A full section specifying `nerve mcp` passthrough and a `nerve check` gate.
New decision: Replaced by a short generic section: RepositoryEvidenceProvider, one
              implementation (git+fs), trait deferred until a second provider exists.
              All Nerve specifics deleted from core.
Reason: §2.3. Non-negotiable product separation.
```
```
Section: §17.1 (exit codes)
Old decision: Exit codes adopted from another project (0/2/3/4/10/70).
New decision: Conductor-derived set using sysexits conventions, with a dedicated code 3 for
              "action required".
Reason: §2.2.
```
```
Section: §9 (state machines) — no change to states; one addition
New decision: Add the eligibility gate as a precondition on READY → RUNNING: an attempt may not
              start if measured capabilities do not satisfy execution_requirements.
Reason: §5.4.
```

---

## 13. Revised Implementation-Ordering Delta

Only slices whose order or scope changes.

**S0 — Measurement slice: mostly DONE, and it shrinks.**
Q1 (worktree config sharing) is **no longer decision-relevant** — clones won on both safety and speed, so worktrees are not a candidate. It should still be recorded as an unverified claim in the doc rather than silently kept as fact. Q2 (Codex containment) is **answered** (§4.1). Q3 (clone cost) is **answered** (§3.4). Q5 (Claude hook denial) remains open. Q4 (SQLite claim latency) remains open. Net: S0 drops from five questions to two, and its findings feed directly into S2 and the new S2.5.

**S2 — Git isolation slice: scope shrinks from "decide and measure" to "implement the decided thing."**
The strategy is settled. What remains is the byte-identity acceptance test — a hostile script inside the clone that sets a remote, deletes a branch, writes a hook, runs `gc`, and mutates object files, after which the source repo's `.git/config`, `show-ref` output and object readability must be unchanged. That test is now known to be non-vacuous, because the default clone fails it.

**NEW S2.5 — Containment probe harness.** Did not exist in the previous roadmap and must.
`conductor doctor --containment` runs the §4 probe suite against each configured (adapter × launcher) on the actual host, caches by version triple, and fails closed when stale. This must exist **before** the first real adapter, because §5's eligibility gate is meaningless without measured input, and because sandbox behaviour will change under CLI upgrades. It is small — the probe suite already exists as a script from this pass.

**S3 — Fake agent + recovery: unchanged in position, one addition.** The fake agent gains a scenario that attempts control-socket connection, so the approval boundary is tested by the harness rather than only by the sandbox.

**S4 — Verification: unchanged.**

**S7 — Policy: unchanged in position.** Gains `execution_requirements` as a policy field.

**S8/S9 — Approval + enforcement: unchanged in position (both still before the first real adapter), scope grows slightly.** S8 adds the operator-nonce mechanism (default off, §11.4). S9 adds per-run `TMPDIR`, `/tmp` delta scanning, and the `SECURITY.md` honesty table now populated with measured values rather than placeholders.

**S10 — First real adapter (Codex): confirmed, and its precondition is now satisfiable.** Depends on S2.5.

**S11 — Plan ledger: gains the `.conductor/**` rejection rule** and the "approval read only from the registered repo" invariant (§7.1(9)). This is a security control, not a convenience, so it lands with the ledger rather than later.

**S13 — Review bridge: unchanged.**

**S14 — Daemon: unchanged, reconfirmed.** Still gated behind fake-agent recovery, leases, fencing, startup reconciliation and side-effect recovery all passing.

**S15 — Claude Code adapter: scope narrows.** Ships at its measured tier (`None` across the board), eligible only for tasks that pass the §5 gate at that tier. "Sandboxed Claude" becomes a separate, later, measured slice — not part of S15.

**Nerve: removed from the numbered roadmap entirely.**
Formerly S16. Now: post-v1, after dogfooding, behind a `RepositoryEvidenceProvider` trait that does not exist until a second provider does. Explicitly **not** a prerequisite for running Conductor, testing Conductor, using Codex or Claude through Conductor, dogfooding Conductor, or completing v1. An acceptance test asserts the full suite passes with no third-party provider binary installed.

**Dashboard: unchanged, last.**

**Net ordering:**

```
S0  measurement (2 remaining questions)
S1  store foundation
S2  workspace isolation (--no-hardlinks clone)
S2.5 containment probe harness            ← NEW
S3  fake agent + crash recovery
S4  verification
S5  first vertical (task → agent → reconcile → verify → commit)
S6  bounded repair
S7  policy (+ execution_requirements)
S8  approvals (+ nonce mechanism, default off)
S9  enforcement + post-run audit          ← HARD GATE before S10
S10 Codex adapter
S11 plan ledger (+ .conductor/** rejection)
S12 packets
S13 review bridge
S14 daemon
S15 Claude adapter (measured tier only)
S16 dogfooding
--- v1 complete ---
post-v1: sandboxed-Claude research · optional evidence providers · dashboard
```

---

## 14. Implementation Readiness Verdict

Every question posed in this pass has an answer grounded in either a measurement taken this session or an explicit, falsifiable decision. The three claims that were previously asserted without evidence — hardlink isolation, Codex containment, and approval-channel enforcement — have all been tested; one confirmed a hypothesis, one refuted my own position, and one produced a result strong enough to change the security model. The five remaining disagreements (§11) each have a recommended temporary choice, and none of them blocks slices S0–S9; the earliest any of them binds is S10, by which point S2.5 will have produced the measured input needed to settle them.

```
READY FOR MASTER PLAN FINALIZATION
```

Carry into the master plan as open items, not blockers: §11.1 through §11.5, plus the two surviving S0 questions (Claude hook denial reliability; SQLite claim latency under concurrency).

---

## Appendix — Experiment Artifacts

| Script | Purpose | Key result |
|---|---|---|
| `scratchpad/git-isolation-experiment.sh` | clone hardlink behaviour, adversarial mutation, cost | default clone corrupted the source repo; `--no-hardlinks` isolated and faster |
| `scratchpad/codex-containment.sh` | containment round 1 | **invalid** — workspace under `/tmp`; AF_UNIX path >104 chars |
| `scratchpad/codex-containment-2.sh` | containment round 2, corrected | filesystem Restricted, network Hard, AF_UNIX Hard w/ positive control, reads unrestricted |

Round 1 is retained deliberately. Both of its flaws produced *false permissive* results — it reported escapes that had not occurred and a socket denial that was a test bug. An experiment that fails toward "the sandbox is weaker than it is" is the safer failure direction, but it is exactly the kind of result that would have been quoted as fact if it had not been re-run.
