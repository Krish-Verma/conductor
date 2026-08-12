# ADR-0002 — Execution containment is measured per (adapter × launcher), never declared

**Status:** ACCEPTED
**Date:** 2026-08-12
**Slice:** pre-S0 (measured during architecture convergence, before this repository existed)

---

## Question

What does an agent execution mode actually prevent, as opposed to what its
documentation suggests it prevents?

## Why the answer matters to Conductor

Conductor gates unattended execution of sensitive tasks on an execution mode's
capabilities. If those capabilities are wrong — or become wrong after a CLI upgrade —
Conductor authorizes work it should have refused. The approval boundary in particular
is worthless if the agent can reach the control socket.

## Experiment / evidence

`scripts/measure/codex_containment.sh` (round 2; round 1 retained as a methodology
record — see below). Runs concrete commands under `codex sandbox` directly, with **no
model invocation**, so results are deterministic and free.

Full measurements: master plan **Part 0, rows M6–M15**. Not restated here.

## Observed result

Summarized:

- Codex `workspace-write` **denies writes** to `$HOME`, `$HOME/Documents`, sibling
  directories, `/Users/Shared`, `~/.ssh`, `~/.codex` and `~/.claude`; **permits** writes
  to the workspace, `/tmp` and `$TMPDIR`. Child processes inherit the restriction.
- It **denies network egress** — TCP connect, DNS and `curl` all fail.
- It **denies AF_UNIX connect by default**, at every path tested, *with the path known
  to the caller*. Proven non-vacuous by a positive control: the identical connect
  **succeeds** under `--allow-unix-socket`.
- It **does not restrict reads**. `~/.codex/auth.json` and `~/.ssh/known_hosts` were
  read successfully.
- `codex sandbox` wraps arbitrary foreign binaries, not only Codex.
- Bare Claude Code provides none of the above; `/usr/bin/sandbox-exec` exists as an
  alternative launcher.

## Decision

1. **Capabilities are measured on the host, never hardcoded.** A probe suite runs per
   `(adapter, adapter_version, launcher, launcher_version, os_version)` and caches the
   result. **A stale or absent probe forces every dimension to `None` — fail closed.**
   A hardcoded capability table silently becomes a lie after a CLI upgrade.
2. **Containment is a property of the launcher, not the agent.** Because `codex sandbox`
   wraps foreign binaries and `sandbox-exec` exists, Conductor may *supply* containment
   rather than inherit an adapter's native tier. The capability model is therefore keyed
   on (adapter × launcher).
3. **Codex is the first real adapter**, because on a machine with no container runtime
   it is the only mode that measurably prevents writes outside the workspace, denies
   network, and denies the control surface.
4. **The control socket lives under `$HOME/.conductor/`** — a location a sandboxed agent
   can neither write (so it cannot squat or replace the socket) nor connect to.
5. **Approval integrity is stated per execution mode**, not absolutely. Tier A
   (sandboxed) is enforced; tier C (unsandboxed, no nonce) is explicitly *not a boundary*.

## What this DOES prove

- For codex-cli 0.142.0 on macOS 26.6, the four gating dimensions have the values
  recorded in Part 0, with a positive control for the AF_UNIX result.
- Sandbox restrictions are inherited by child processes for the shapes tested.
- The mechanism for supplying containment to a foreign binary exists and runs.

## What this DOES NOT prove

- **It is version- and platform-specific.** It is a claim about this machine, these two
  CLI versions, and macOS seatbelt. That is precisely why decision (1) requires
  re-measurement rather than trusting this record.
- It does not prove the write-denial set is complete; `/tmp` and `$TMPDIR` remain
  writable, so secret staging is possible and is only *detectable*.
- It does not prove reads are containable at all under this launcher — measurement says
  they are not.
- It does not prove a sandboxed **Claude** run works. Wrapping `claude --version`
  succeeded; a full run needs network, which the sandbox denies, so a usable profile
  would need a hole exactly the size of one API endpoint. Untested.

## Methodology record: an invalid first round

The first containment round produced **false permissive** results and was discarded:
the "outside the workspace" directory was itself under `/tmp` (which the policy
permits), and the AF_UNIX test failed on `sun_path` length rather than on the sandbox.
It reported escapes that had not occurred and a socket denial that was a test bug.

It is recorded rather than deleted because both flaws failed in the direction of
"the sandbox is weaker than it is" — the safer direction, but exactly the kind of
result that gets quoted as fact if it is not re-run. Any future containment probe must
carry a positive control for the same reason.

## Pre-registered falsification / revisit trigger

- Any adapter or OS upgrade — handled automatically by the version-keyed probe cache.
- A container runtime becoming available on the target machine, which would add a
  stronger launcher and change the tier table.
- Evidence that sandbox restrictions are *not* inherited by some child-process shape.
- A measured way to contain reads, which would change `credential_read` from `None`.

## Impacted master-plan sections

Part 0 (M6–M15) · §4.2 · §4.3 · §4.9 · §6.2 · §6.3 · slice S2.5 · acceptance-suite rows 28, 30.
