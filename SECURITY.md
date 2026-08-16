# Conductor — Security Model

**Scope of this document.** What Conductor prevents, what it merely restricts,
what it only detects, and what it does not address at all. Written at S9, whose
stop point is *"the honesty table is complete and true"*.

**The rule this document is built on:** master plan §4.9 —

> **No item may be listed as prevented without a passing test.**

Every `PREVENTED` row below therefore names the mechanism, the test, and the
**positive control** that proves the test can fail. A test that only shows the
bad thing not happening would pass on a machine where the bad thing was never
possible; a control that makes it happen is what turns absence of evidence into
evidence of absence.

---

## 1. Vocabulary

| Term | Meaning |
|---|---|
| **PREVENTED** | A named mechanism makes it impossible. Adversarial test + positive control, both passing. |
| **RESTRICTED** | Enforced, with a known and enumerated exception set. The exceptions are listed. |
| **DETECTED** | Not prevented. Reliably observed afterwards, and the observation is a durable finding. |
| **NOT PREVENTED** | Neither prevented nor reliably detected. Stated so nobody assumes otherwise. |
| **UNKNOWN** | Not measured. Never inferred from a similar measurement. |

A claim is scoped to a **platform and version**. Sandbox behaviour changes with
OS and CLI releases, which is why Conductor measures rather than hardcodes
(§4.2), and why this document dates its measurements.

---

## 2. Measurement scope

Everything below was measured on:

```
host      Darwin 25.6.0 (macOS), arm64
git       2.51.0 (Apple/Xcode-provided)
rust      1.97.1
date      2026-08-15
slice     S9
```

**Execution configurations that exist at S9:** `FakeAgent` under no launcher.
Codex (S10) and Claude Code (S15) are not wired into the run path yet; their
measured containment values are recorded in master plan §4.2 from S0/S2.5 probe
runs and are **not** repeated here as if they were reachable today. Section 7
states what is `UNKNOWN` for that reason.

---

## 3. The layer model, and which layer actually carries the weight

Master plan §4.9 ranks eight layers. The ranking is the design, and it is
uncomfortable on purpose:

| Layer | Prevents | Status at S9 |
|---|---|---|
| 1. Prompt instructions | **nothing** | not a control; worth ~0 |
| 2. Deterministic policy | Conductor's own actions | active |
| 3. Conductor-owned effects | the agent performing push/deploy *through Conductor* | active |
| 4. Agent permission hooks | specific tool calls by pattern; **known bypasses** | not wired (S15) |
| 5. OS sandbox | writes outside workspace; network; AF_UNIX | not wired (S10) |
| 6. **Credential absence** | **push, deploy, cloud API, DB access** | **active — the primary control** |
| 7. Network control | egress | not wired (S10) |
| 8. Post-run audit | nothing | active; detects almost everything |

**Layer 6 is the primary control, not the fourth.** An agent with no push
credential cannot push regardless of what it types, what it is told, or whether
any hook fires. Layers 4, 5 and 7 are unavailable at S9, so anything claimed
below rests on 2, 3, 6 and 8.

---

## 4. Truth table

### 4.1 Credentials

| Operation | Status | Mechanism | Test | Positive control |
|---|---|---|---|---|
| Inherit an environment credential (`AWS_SECRET_ACCESS_KEY`, `GH_TOKEN`, `GITHUB_TOKEN`, `DATABASE_URL`, `ANTHROPIC_API_KEY`, `SSH_AUTH_SOCK`, `NETRC`, …) | **PREVENTED** | The child environment is **built from an allowlist**, not filtered; `supervise::spawn` calls `env_clear()` first. Adding a variable requires naming it in `AGENT_ENV_KEYS`. | `enforce_env::a_planted_credential_does_not_reach_the_child_and_the_control_proves_it_could` — plants eight disposable canaries and asserts none crosses. | **Yes.** The same measurement with `env_clear` omitted must observe every canary, or the test declares itself meaningless. |
| Receive an unlisted variable of any kind | **PREVENTED** | Same. | `enforce_env::the_child_environment_is_exactly_the_allowlist` — **set equality**, not containment, against what a child actually observes. | Mutation: inheriting a single `GH_TOKEN` fails this test. |
| Discover a credential under `$HOME` (`~/.aws`, `~/.config/gh`, `~/.netrc`, `~/.ssh`, `~/.git-credentials`) | **PREVENTED** | Per-run `HOME` inside the workspace, created by `enforce::env::prepare`. | `enforce_env::home_relative_credential_lookup_finds_nothing` — a child resolves each path through its own `$HOME`. | Implicit: the paths exist under the real `$HOME` on any developer machine, and the test asserts `absent` through the redirected one. |
| Have `git` prompt for a credential | **PREVENTED** | `GIT_TERMINAL_PROMPT=0`. | `enforce_env::git_cannot_prompt_for_a_credential` — a real `git fetch` against an unroutable URL. | The fetch is asserted to fail *without* a prompt, and the stderr is inspected for the prompt text. |
| Have `git` obtain a credential via askpass | **PREVENTED** | `GIT_ASKPASS` → **a program Conductor writes itself** into the per-run `HOME` at mode `0500`: `exit 1`, prints nothing. Rewritten on every `prepare`, so an agent that replaced it does not keep the replacement. | `enforce_env::the_askpass_program_exists_and_always_fails` — asserts `.is_file()`, non-zero exit, and empty stdout. | Mutation: reverting to S5's `/bin/false` fails the test. **This is not hypothetical** — `/bin/false` does not exist on macOS and shipped that way from S5 to S9 (ADR-0011). |
| Have `git` obtain a credential from a **system** gitconfig | **PREVENTED** | `GIT_CONFIG_NOSYSTEM=1`, plus `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` → `/dev/null`. | `enforce_env::the_system_gitconfig_cannot_be_read` — asserts no `credential.helper` is reachable. | **Yes, and it is the reason the row exists.** Dropping `GIT_CONFIG_NOSYSTEM` must make the setting reappear. On this host Xcode's git ships `credential.helper=osxkeychain` at an absolute path that survives both `env_clear` and a redirected `HOME` (ADR-0011). If a host ships none, the test says so out loud rather than passing silently. |
| **Read a credential by absolute path** (`~/.ssh/id_rsa`, `~/.codex/auth.json`) | **NOT PREVENTED** | — | — | M12. The per-run `HOME` defeats *discovery*, not *reading a path you already know*. Under a sandboxed launcher (S10) network denial means reading ≠ exfiltrating, but staging for later pickup is possible. At S9 there is no sandbox at all. |
| Regain a credential through a **login shell** started by the agent | **PREVENTED** *(measured 2026-08-15)* | The per-run `HOME`. Codex runs its shell commands as `/bin/zsh -lc …` — a **login** shell, which sources profile files. Those are `$HOME/.zprofile` and `$HOME/.zshrc`, and the per-run `HOME` has neither. | Measured directly: a login shell launched under the S9 environment, with a canary planted in the parent, reported **0** variables matching `^(AWS_|GH_|GITHUB_|SSH_AUTH)`. | The canary is present in the parent process, so the measurement distinguishes "the shell found nothing" from "there was nothing to find". |
| — *caveat to the row above* | **NOT PREVENTED** | — | — | `/etc/zprofile` and `/etc/zshrc` are absolute and **are** sourced. On macOS they run `path_helper`, which **rewrites `PATH`** — so the `PATH` Codex's shell commands actually run with is wider than the one Conductor supplied. `PATH` is a search path, not a credential, so this is recorded as a measured caveat rather than a hole; it does mean "allowlisted environment" is not "the environment I specified" once a login shell is involved. |

### 4.2 Repository

| Operation | Status | Mechanism | Test |
|---|---|---|---|
| Damage the operator's repository | **PREVENTED** | `git clone --no-hardlinks --no-checkout` (ADR-0001). The run works in a clone whose object store shares nothing with the source. | `conductor-git::isolation` — including an in-place write to a clone `.git` object, after which the **source** `git fsck` exits 0 (acceptance row 15). |
| Push to any remote | **PREVENTED** | The run clone has **no `origin`** — `git remote remove origin` at clone time. There is nothing to push to before credentials are discussed. | `enforce_env` + `conductor-git::isolation`. Combined with the four credential rows above. |
| Add a remote and push to it | **RESTRICTED → DETECTED** | Adding a remote is not prevented. Obtaining a credential for it is (rows above). The addition is a `POLICY_SENSITIVE` verdict. | `scenarios::a_forbidden_git_change_is_caught_even_though_the_tree_is_unchanged` (row 14) — the tree is byte-identical to baseline, which is why a tree-first classifier would miss it. |
| Modify local git config, refs, hooks, stash, submodules | **DETECTED** | §4.8's reconciled surface diffs all of them against the captured baseline. Findings **never auto-resolve**. | `conductor-git::reconcile` suite. |
| Write outside the workspace | **NOT PREVENTED** at S9 | — | Under a non-sandboxed launcher Conductor cannot prevent writes anywhere the user can write. S10's measured `filesystem_write: Restricted` changes this for Codex only. Detection is partial: §4.8 sees the workspace, not the whole disk. |

### 4.3 Policy, approval and eligibility

| Operation | Status | Mechanism | Test | Positive control |
|---|---|---|---|---|
| Complete a policy-sensitive change with no human approval | **PREVENTED** | The policy gate runs on every `POLICY_SENSITIVE` verdict, from **both** the attempt path and the recovery path (one function, so they cannot disagree). `require_approval` creates a durable request and the run halts in `AWAITING_APPROVAL`. | `enforce_approval::a_policy_sensitive_run_creates_a_durable_approval_request` (row 13). Nothing in the test creates the request — the run must. | **Yes.** `a_run_whose_policy_allows_the_change_creates_no_request` — same fixture, same verdict, one rule changed. A build that asked for approval on every sensitive verdict fails it. |
| Proceed on a grant issued for a different action, run, or policy | **PREVENTED** | `approval::authorize` **recomputes** the binding from the decision in hand and compares; the stored binding is never trusted. | `enforce_approval::a_grant_for_a_different_action_does_not_resume_the_run` | **Yes.** `a_granted_request_resumes_the_run_without_a_second_agent_attempt` proves the *correct* grant does complete the run, with `Cargo.toml` present in the integrated commit read back from git. |
| Proceed on an expired grant | **PREVENTED** | TTL checked at authorize **and** again at consume, adjacent to the effect. | `enforce_approval::an_expired_grant_does_not_resume_the_run` | As above. |
| Proceed on a revoked grant | **PREVENTED** | Revocation before consumption; re-authorized at the gate on the way through. | `enforce_approval::a_grant_revoked_before_it_is_spent_does_not_resume_the_run` | As above. |
| Spend one grant twice | **PREVENTED** | `consume` names `state = 'GRANTED'` in the `UPDATE`, inside `BEGIN IMMEDIATE`. | S8's 50 × `SIGKILL` durability test; `enforce_approval` asserts `CONSUMED` after resume. | |
| **Undo an effect that already happened** | **NOT PREVENTED — and never claimed** | — | `enforce_approval::revoking_after_consumption_does_not_pretend_the_effect_was_undone` asserts the grant stays `CONSUMED`, the run stays `COMPLETE`, and the commit is still there. | Conductor can refuse to be the one that performs an effect, and detect afterwards. It cannot roll one back. |
| Launch unattended when measured capabilities are below the task's requirement | **PREVENTED** | `eligibility::check` at the launch call site, **before the claim**. Refusal is durable: `BLOCKED` + a `CRITICAL` finding naming the dimension. | `enforce_eligibility` — 16 tests through `vertical::run_task`, asserting zero attempt rows and no workspace on disk. | **Yes.** `the_same_task_launches_once_the_host_measures_enough`. Mutations both ways: a gate that always permits fails 6 tests; a gate that always refuses fails the controls. |
| Run unattended below tier A a task whose policy can produce an approval gate (§4.3's binding rule) | **PREVENTED** | S11 wired `approval::gate::unattended_requirements` into the same call site: `gate` reads `task.declared_actions`, resolves the policy the run is **pinned** to, and merges the resulting `control_surface: hard` with the task's own vector, taking the stronger demand. | `enforce_eligibility::a_declared_action_the_policy_can_gate_may_not_run_unattended_below_tier_a` — no attempt row, no workspace, `BLOCKED` + a `CRITICAL` finding naming `control_surface`. | **Yes, two.** `the_same_gateable_task_launches_on_a_host_measured_hard` (same fixture, one seeded probe row) and `a_declared_action_the_policy_cannot_gate_launches_on_a_host_below_tier_a` (same host, one policy rule changed). Mutation: deleting the `merge(unattended_requirements(…))` call fails 4 named tests and leaves both controls green. |
| Disable §4.3's binding rule by corrupting what it reads | **PREVENTED** | Unreadable `declared_actions`, an undecodable pinned policy snapshot, and a task with no active run are each a refusal — the rule is undecided, and undecided is never "no gate is possible". `NULL` (never materialized) is the one case that keeps pre-S11 behaviour, and it is a different column value from `'[]'`. | `enforce_eligibility::declared_actions_that_are_not_a_json_string_array_refuse_rather_than_gating_nothing`; `…::a_materialized_task_is_refused_when_the_runs_policy_cannot_be_read_and_a_never_materialized_one_is_not`; `…::a_materialized_task_with_no_active_run_has_no_policy_to_ask_and_is_refused` | **Yes.** The same test pins `NULL` against `'[]'` on an identical undecodable snapshot: the never-materialized task launches, the materialized one is refused. A build that collapsed the two fails it in one direction or the other. |
| Satisfy a gating requirement with a **hook** | **PREVENTED, structurally** | `tool_interception` is not a variant of `GatingDimension`, has no `PartialOrd`, and the YAML loader refuses the name. Three type-level barriers, not a runtime check. | `policy::eligibility` `compile_fail` doctests. | Hooks have known bypasses (`sh -c`, script-then-execute, alternate spellings); letting a non-boundary satisfy a boundary requirement is the exact confusion the model exists to prevent. |
| Disable the gate with a typo | **PREVENTED** | A requirements column that is present but yields no requirement is treated as **unreadable**, not as "nothing gated". | `enforce_eligibility::a_requirement_that_parses_to_nothing_refuses_rather_than_gating_nothing` | Absent (`NULL`) and present-but-meaningless are deliberately different answers. |

### 4.4 The control surface

| Operation | Status | Mechanism | Test |
|---|---|---|---|
| Reach the approval socket from a workspace-facing binary | **PREVENTED** | No approval-granting code path is reachable from any workspace-facing binary. Enforced by a source-scan test over the actual dependency census. | `layering.rs`, with a TEETH test proving the scan catches a deliberately wired violation. |
| An agent connecting to `conductor.sock` | **DETECTED** at S9 | Under a sandboxed launcher (S10) the connect is denied at `Hard` (M10, M11). At S9 there is no sandbox, so the connection succeeds and noticing is all Conductor can do. | `scenarios::an_attempt_to_reach_the_control_socket_raises_a_critical_finding` (row 28) — `CRITICAL`, never auto-resolves. |
| A `0600` socket proving a human is at the other end | **NOT PREVENTED — explicitly disclaimed** | — | ADR-0002. A `0600` socket does not distinguish a human from a same-user subprocess, and removing an environment variable is obscurity. Approval integrity is a property of the **execution mode**, which is measured, not asserted. |

### 4.5 Secrets and temp surfaces

| Operation | Status | Mechanism | Test |
|---|---|---|---|
| Leave a shaped credential in the working tree | **DETECTED** | Secret scan over the full patch text — staged, unstaged, **and untracked files rendered as added-file hunks** — wired into every attempt. `CRITICAL` for a shaped credential on an added line. | `enforce_approval::a_credential_the_agent_committed_becomes_a_finding_on_the_real_run_path` proves the **call site** exists, through `run_task`; `enforce_audit` proves the scanner. Both are needed: a scanner nobody calls detects nothing. |
| — *the same, negative control* | | | `enforce_approval::an_ordinary_change_on_the_real_run_path_raises_no_secret_finding`. Mutation: emptying the call site fails the detection test while this one still passes. |
| Stage a credential in the per-run `TMPDIR` | **DETECTED** | Per-run `TMPDIR` inside the workspace is scanned after the attempt. | `enforce_audit::a_secret_staged_in_the_run_tmpdir_is_found_and_never_echoed` |
| Write to system `/tmp` | **NOT PREVENTED; WEAKLY DETECTED** | Per-run `TMPDIR` redirects *ordinary* temp usage (M7 mitigation). System `/tmp` is snapshotted before and after the attempt window. | `enforce_audit` — and see §5, which states exactly what the attribution cannot prove. |
| A secret the scanner's shape rules do not match | **NOT PREVENTED** | — | `verify::secrets::NOT_DETECTED` enumerates the blind spots, and a test asserts the list is real rather than decorative. |

---

## 5. What the `/tmp` attribution cannot prove

Reproduced verbatim from the audit implementation, because a detection claim
without its limits becomes an assumption:

1. **Same uid is not same process.** Conductor itself, the test harness, the
   shell, cargo and every other process running as this user share the system
   temp directory. Ownership narrows the field to "this account"; it never
   narrows it to "the agent".
2. **Timestamps are not testimony.** Creation time is unavailable on some
   filesystems (the audit falls back to mtime), and mtime is settable by the
   process that owns the file.
3. **Absence of a delta is not absence of activity.** A file created and deleted
   inside the window leaves nothing; a write into an existing file changes no
   name; the system-temp scan is top-level only.
4. **`/tmp` is not the only residual path.** macOS's per-user `/var/folders/…`,
   `/dev/shm`, and any other world-writable directory are not watched.
5. **Nothing is known about the content of a system-temp entry.** Conductor
   records the name and refuses to read the file — reading other processes' temp
   files to audit an agent would be a worse privacy posture than the thing being
   audited.
6. **Even in the per-run `TMPDIR`, attribution stops at the run.** It does not
   identify which child process wrote a file.
7. **Secret detection is shape-based.** Every entry in
   `verify::secrets::NOT_DETECTED` applies unchanged. A clean audit is not
   evidence that no secret was written.
8. **File reads are bounded** to the first 64 KiB per file, decoded lossily as
   UTF-8.
9. **The diff scan sees the tree as it ends, not everything that happened.** A
   file written and then reverted inside the attempt leaves nothing to scan.
   Untracked files *are* covered (each is rendered as an added-file hunk), but
   anything `.gitignore` or `.git/info/exclude` suppresses is not — deliberately,
   since that is what keeps the per-run `HOME` and `TMPDIR` from being scanned
   twice, and they are covered by the temp audit instead.
10. **Untracked file reads are bounded** to 64 KiB and to valid UTF-8; a binary
    file is skipped rather than decoded lossily into noise.

Consequently no system-`/tmp` finding is ever blocking, and findings the window
cannot account for are reported at a lower severity with wording that says the
attribution is uncertain — rather than being dropped, and rather than being
asserted as agent activity.

---

## 6. Stated limitations (master plan §4.9, unchanged)

1. Under a non-sandboxed launcher, Conductor cannot prevent writes anywhere the
   user can write. Detection only.
2. Under Codex, `/tmp` remains writable (M7) — secret staging is possible;
   mitigated by per-run `TMPDIR` and `/tmp` delta auditing, **not prevented**.
3. **Reads are not contained even under the sandbox** (M12).
4. Conductor cannot distinguish a human at the socket from a process that
   reached it.
5. Conductor cannot undo an external side effect.
6. **Prompt injection from repository content is unmitigated.** Packets label
   repository-derived spans as untrusted; labelling is mitigation, not
   prevention.

---

## 7. UNKNOWN

Stated rather than inferred:

- **Codex containment through a Conductor run.** §4.2 records measured values
  from the S0/S2.5 probe suite, but no Codex process has been launched through
  the run path (S10). The measurement is of a launcher against an arbitrary
  payload, not of Conductor driving Codex.
- **Claude Code containment through a Conductor run.** Same, and additionally:
  the fourth column of §4.2's table measures what a *launcher* does, and does
  **not** show that Claude Code functions under it — a real Claude run needs
  network egress, which the same sandbox denies at `Hard`.
- **`tool_interception` for any adapter.** The probe harness cannot measure it:
  a hook only fires inside a live agent session. Measured out-of-band by S0's
  live probe (ADR-0003) and reported as *unmeasured*, never inferred. Safe
  precisely because it never gates.
- **Behaviour on any host other than the one in §2.** Every row above is scoped
  to that measurement.
- **Whether the operator's keychain holds a credential.** Checked once on this
  host and found empty for `github.com`; the mitigation in §4.1 does not depend
  on that result, and must not.

---

## 8. Reporting

Conductor is pre-v1 and not deployed. If you find a defect in a claim above, the
useful report names **the row, the mechanism, and the test that should have
caught it** — a `PREVENTED` row whose test cannot fail is the most serious
category of bug in this document.
