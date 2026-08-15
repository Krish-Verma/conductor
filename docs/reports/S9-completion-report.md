# S9 — Enforcement and post-run audit

**Status:** COMPLETE
**Starting commit:** `4db3ee9` (`6afdd3d` + the test-attribution harness)
**Ending commit:** *(this commit)*
**Stop point:** *"The honesty table is complete and true."* — `SECURITY.md`

---

## 1. Objective

> **Make the environment, not the prompt, the boundary.** (Part 8, S9)

⛔ **This was the hard gate before S10.** No real coding agent may execute through
Conductor until S9 passes, and none has: every test in this slice uses the fake
agent, deterministic host commands, or disposable git fixtures.

S9's work turned out **not** to be deciding anything new. S7 decided eligibility,
S8 decided approvals, S4 built the secret scanner. All three were pure functions
with good coverage and **no call site**. S9 is the slice that made them reachable
— and reaching them surfaced four defects that no amount of unit coverage could
have found, because each one lives in the gap *between* components.

---

## 2. What S9 found

Four defects, three of them in shipped code, listed worst-first.

### 2.1 A named credential mechanism did not exist (ADR-0011)

`GIT_ASKPASS=/bin/false`, shipped since S5. **`/bin/false` does not exist on
macOS** — `false` is at `/usr/bin/false`. Git reported `cannot exec '/bin/false'`
and then refused to prompt because `GIT_TERMINAL_PROMPT=0`, so nothing ever
broke and nothing was noticed.

It failed *safe*. But `SECURITY.md` was about to name this mechanism, and a
control that is documented, believed and absent is precisely the false-safety
claim S9 exists to eliminate. Conductor now writes its own askpass program into
the per-run `HOME` at mode `0500`, and a test asserts the file is there.

### 2.2 A credential path the environment allowlist cannot close (ADR-0011)

A **system** gitconfig is located by absolute path, so it survives both
`env_clear()` and a redirected `HOME`. On this host Xcode's git ships one
declaring `credential.helper=osxkeychain`.

An agent running `git remote add origin https://…` then `git push` would have git
consult the keychain — **no environment variable involved**, so every item in
§4.9's list was irrelevant to it. Layer 6 is the *primary* control by the master
plan's own ranking, and it had a hole that the allowlist model structurally could
not see.

On this host the keychain holds no `github.com` credential, so it was not live.
That is a property of the operator's machine, not of Conductor.
`GIT_CONFIG_NOSYSTEM=1` closes it.

### 2.3 The per-run `HOME` and `TMPDIR` pointed at nothing

S5 built the environment map without creating the directories it named. A
variable naming a non-existent directory is not isolation: depending on the tool
it is an immediate failure or — worse — a silent fallback to the real `/tmp`,
which is the containment hole the variable was supposed to close.

Fixed by construction rather than by remembering to `mkdir`: `RunEnvironment` has
no constructor except `prepare`, which does the I/O.

### 2.4 The secret scanner missed the canonical `.env` line

`AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE` scanned **completely clean**. The
tokenizer split on whitespace only, so the whole string was one token: it does
not start with `AKIA`, and `AWS_ACCESS_KEY_ID` is not one of `SENSITIVE_NAMES`.

This is *exactly* the shape §4.9's limitation 2 predicts — "secret staging is
possible" — and §11.2's "tested with planted secrets" was satisfied by a corpus
built from the detectors rather than from what an agent would actually write.

Raised by the audit subagent, **independently reproduced** before being accepted,
and found to be worse than reported: the most canonical form of the most common
cloud credential was invisible.

---

## 3. Two things the state machine could not express (ADR-0012)

Wiring the two gates asked a question neither S7 nor S8 had to answer: **where
does the run go when the answer is no?**

| Edge | Why it was missing | Why the alternative is wrong |
|---|---|---|
| `READY → BLOCKED` | §5.2 draws `READY ──claim+eligibility──► RUNNING`: two gates named, only the both-pass outcome drawn. Row 30's `BLOCKED` was unreachable from anywhere. | `RUNNING → BLOCKED` was rejected: the gate runs before the claim, which keeps §4.8's "every exit from `RUNNING` passes through reconciliation" literally true for a run that never launched an agent. |
| `AWAITING_APPROVAL → RECONCILING` | The diagram's `(granted)` arrow points at `READY`. | **`READY` destroys the approved work.** `ensure_workspace` re-captures the baseline from a workspace that already holds the approved change, so the next attempt reconciles as `NO_CHANGE` and the approval authorises nothing. `RECONCILING` rejoins §4.7's recovery path, which compares against the **stored baseline artifact**. |

Both writes are unfenced and state-guarded, because a run in either state holds
no lease — the `WHERE state = …` clause is the same concurrency mechanism the
claim itself uses.

---

## 4. A completion criterion that could never fire (ADR-0013)

Running row 12 to completion for the first time produced:

```
Stopped { state: AwaitingReview,
          reason: "ReconciliationVerdict: the reconciliation verdict is
                   POLICY_SENSITIVE, not CLEAN_COMPLETE or CLEAN_NO_REPORT" }
```

The grant was authorized, the binding matched, the grant was consumed — and
§4.5's **criterion 6** refused before **criterion 7** was ever consulted.

The reason is structural. A policy-sensitive action is policy-sensitive because a
sensitive path changed, and **approval does not un-modify the file**. So the
verdict is still `POLICY_SENSITIVE` after the human grants, criterion 6 refuses
every time, and every run criterion 7 could speak about is already rejected.
**Criterion 7 was unreachable by construction, and had been since S4.**

It also made rows 12 and 13 vacuous in the other direction: "resumes on grant"
would resume a run that then refuses to complete no matter what the human said.

Criterion 6 now reads as excluding the verdicts *nobody has resolved*, with an
authorized `POLICY_SENSITIVE` resolved by criterion 7. Enforced in the type
system: the evidence variant carries the authorizing grant as a **required
field**, so "authorized" cannot be claimed by a caller with nothing to name.

---

## 5. Previously Deferred Acceptance Rows

The master plan held four rows at `NOT RUN` and said scoring them `PASS` on unit
coverage "would be exactly the 'a similarly named test exists' error the sweep
forbids". Each is now scored from end-to-end evidence through
`vertical::run_task` — the same entry point production uses.

| Row | Previous status | New test / evidence | Final |
|---|---|---|---|
| **13** — Dependency policy violation → request created, `AWAITING_APPROVAL` | `NOT RUN` — "nothing in the run path creates one" | `enforce_approval::a_policy_sensitive_run_creates_a_durable_approval_request`. **Nothing in the test writes a request**; the run must create it. Asserts exactly one `REQUESTED` row, bound to the run, subject `PolicyAction { DependencyAddRuntime }`, pinned to the run's own policy hash, naming the rule that gated it, with a mandatory TTL. | **PASS** |
| **12** — Crash during approval wait → resumes on grant | Half enforced: "TTL and `REQUESTED` survive restart — proven; 'resumes on grant' — not reachable" | `enforce_approval::a_granted_request_resumes_the_run_without_a_second_agent_attempt`. Run reaches `COMPLETE`; **attempt count unchanged** across the resume; grant is `CONSUMED`; and `Cargo.toml` — the approved change — is present in the integrated commit **read back from git**. | **PASS** |
| **25** — Approval revoked mid-effect | Mechanism-only: "no real run reaches revocation" | Both halves. Before consumption: `a_grant_revoked_before_it_is_spent_does_not_resume_the_run` — run stays `AWAITING_APPROVAL`, grant `REVOKED`. After consumption: `revoking_after_consumption_does_not_pretend_the_effect_was_undone` — grant stays `CONSUMED`, run stays `COMPLETE`, commit still present. Conductor cannot undo an effect and does not pretend to. | **PASS** |
| **30** — Ineligible execution mode → attempt never starts, `BLOCKED` | `NOT RUN` — "the decision is proven, the refusal is not yet reachable from a real launch" | `enforce_eligibility` — 8 tests through `run_task`. Asserts **zero attempt rows**, **no workspace on disk**, run and task both `BLOCKED`, and a `CRITICAL` unresolved finding naming the dimension, the requirement and the measured value. | **PASS** |

Row 12's restart half was already proven at S8 (50 × `SIGKILL`); S9 adds the
reachable resume. Row 25's four-state revocation matrix was already proven at S8;
S9 adds the run that reaches it.

---

## 6. Non-vacuity evidence

Every load-bearing control was mutated and the mutation confirmed to fail a named
test. **Both directions** are covered wherever a one-sided test could pass under a
broken implementation — the S8 lesson, applied throughout.

| Mechanism | Mutation | Caught by |
|---|---|---|
| Eligibility gate — refuses | `gate()` returns `Ok(None)` always | 6 refusal tests in `enforce_eligibility` |
| Eligibility gate — permits | `gate()` returns `Refused` always | `the_same_task_launches_once_the_host_measures_enough`, `a_task_that_requires_nothing_launches_on_an_unprobed_host` |
| Credential isolation | Inherit one `GH_TOKEN` (allowlist adjusted so the internal assert stays quiet) | `a_planted_credential_does_not_reach_the_child_and_the_control_proves_it_could` |
| Environment allowlist | Same | `the_child_environment_is_exactly_the_allowlist` |
| System-gitconfig suppression | Drop `GIT_CONFIG_NOSYSTEM` | `the_system_gitconfig_cannot_be_read` |
| Askpass mechanism | Revert to S5's `/bin/false` | `the_askpass_program_exists_and_always_fails` |
| Per-run `TMPDIR` | Skip its `create_dir_all` | `the_per_run_home_and_tmpdir_exist_on_disk_and_the_child_can_write_to_tmpdir` |
| Secret-scan **call site** | Replace with an empty vec | `a_credential_the_agent_committed_becomes_a_finding_on_the_real_run_path`, while the negative control still passes |
| Secret detection in diffs | `audit_diff_for_secrets` → `Vec::new()` | 7 tests in `enforce_audit` |
| No-secret-in-findings | `AuditFinding::new` stores the raw detail | `a_finding_cannot_be_built_with_the_secret_still_in_it` |
| Redact-before-truncate | Constructor redaction only | `a_long_line_is_redacted_before_it_is_truncated_not_after` — this mutant **initially survived both suites**; two redaction layers meant neither was individually tested |
| Temp-delta detection | `audit_temp_delta` → `Vec::new()` | 8 tests |
| `/tmp` attribution | Treat every `/tmp` entry as attributable | `a_system_tmp_entry_the_window_cannot_account_for…` |
| False-positive floor | `is_credential_shaped_literal` → `true` | `an_ordinary_code_diff_raises_nothing` |

The two most instructive: the **redact-before-truncate** mutant survived because
the constructor's redaction pass runs *after* truncation and therefore cannot see
a bisected secret — two layers that looked redundant were not equivalent. And the
**secret-scan call site** mutant is the whole S9 thesis in miniature: the scanner
had 24 passing tests and was called from nowhere.

---

## 7. Files changed

**New**
```
crates/conductor-run/src/enforce/{mod,env,launch,policy_gate,audit}.rs
crates/conductor-run/tests/{enforce_env,enforce_eligibility,enforce_approval,enforce_audit}.rs
SECURITY.md
docs/decisions/ADR-00{11,12,13}-*.md
scripts/run-tests.sh                       (committed separately, 4db3ee9)
```

**Modified**
```
crates/conductor-core/src/task.rs          §5.2 corrections 5 and 6
crates/conductor-core/src/completion.rs    criteria 6 and 7 reachable
crates/conductor-store/src/schema.rs       SCHEMA_V7, version 7
crates/conductor-store/src/{migrate,task,lease,lib,error}.rs
crates/conductor-run/src/worker.rs         env prepare, temp watch, audit, policy route
crates/conductor-run/src/recovery.rs       same routing function as the attempt path
crates/conductor-run/src/vertical.rs       eligibility gate, resume_on_grant, policy evidence
crates/conductor-run/src/verify/secrets.rs separator-aware tokenizing
crates/conductor-agent/src/scenario.rs     `secret-in-diff` fixture
crates/conductor-cli/src/task.rs           real probe key from the detected host
CLAUDE.md, docs/architecture/CONDUCTOR-MASTER-PLAN.md
```

**Deliberately not created:** `enforce/secrets.rs`. S4 already built the scanner,
with its published `NOT_DETECTED` list; a second one would be a second answer to
"is this text safe to show" and the two would drift.

---

## 8. Verification

```
scripts/run-tests.sh
  suites:  79
  passed:  822        (baseline at S8: 766)
  failed:  0
  ignored: 0

cargo fmt --all --check                                   clean
cargo clippy --all-targets --all-features -- -D warnings   clean
```

Every run in this slice used `scripts/run-tests.sh`, which captures suite
tallies **and the fully-qualified name of every failure** in one invocation. It
earned its keep immediately: a mid-slice compile break was reported as an
explicit `ANOMALY: cargo exited 101 but no test name was captured` rather than as
the unattributable count that motivated it.

**The S8 flake did not recur** across six full runs in this slice. It remains
unidentified; the instrumentation is now in place to name it on its next
occurrence, and that is the honest status — not "fixed".

---

## 9. Skills and subagents

**Skills inspected:** `superpowers:{using-superpowers, test-driven-development,
verification-before-completion, subagent-driven-development,
systematic-debugging, brainstorming, executing-plans, requesting-code-review}`.

**Used:** `test-driven-development` — every behaviour in this slice was written
test-first and watched fail for the right reason, including three cases where
the first RED was for the *wrong* reason and was corrected before implementing.

**Subagents:** one, for `enforce/audit.rs` and its tests, with an exclusive file
scope and an explicit prohibition on touching the concurrently-edited run path.
It reported a defect in shipped S4 code (§2.4); that claim was **independently
reproduced** before being accepted, and the fix was made by the orchestrator in
`verify/secrets.rs`, which was outside the subagent's scope. The security-critical
run-path wiring was kept in-house rather than delegated.

---

## 10. Architecture drift audit

Searched for accidental implementation of rejected ideas. **None found.** No
`WorkflowRuntime`, no DAG engine, scheduler or DSL, no event replay as
authoritative state, no worktree or hardlinked clone, no automatic merge/rebase,
no Nerve dependency, no `RepositoryEvidenceProvider`, no unbounded repair, no
strict unknown-field rejection on external agent events.

One thing worth naming: `enforce/` gained two files the plan did not foresee
(`launch.rs`, `policy_gate.rs`). Both are **call sites**, not new subsystems —
neither re-decides anything S7 or S8 decided. `policy_gate::route_reconciliation`
is deliberately the *single* routing function shared by the attempt path and the
recovery path, because two routing paths that could disagree about policy is
exactly the drift this audit looks for. It is also load-bearing: a granted run
returns through recovery, so a policy check living only in the attempt path would
never see the grant.

---

## 11. Known limitations

Recorded here and in `SECURITY.md` §5–7, not hidden:

- **Reads are not contained.** The per-run `HOME` defeats credential
  *discovery*, not reading a path you already know (M12).
- **`/tmp` attribution is weak by nature.** Same uid ≠ same process; timestamps
  are settable; a create-then-delete inside the window leaves nothing. No
  system-`/tmp` finding is ever blocking, and unattributable entries are reported
  as uncertain rather than asserted as agent activity.
- **The secret scanner is shape-based.** `NOT_DETECTED` enumerates the blind
  spots. Its claim that entropy analysis was "S9's" has been corrected to say it
  is unimplemented and unscheduled — naming a slice that will fix a gap, and then
  not fixing it, is how a limitation becomes a promise.
- **§4.3's binding rule is still not wired.** `unattended_requirements` needs the
  set of actions a task may perform, and no such declaration exists on a task at
  S9. Inventing one would be guessing at the schema S11 owns. It is decided and
  not reachable, and must not be scored as enforced. **S11 owns wiring it.**
- **Layers 4, 5 and 7 are unavailable.** Hooks (S15), OS sandbox and network
  control (S10) are not wired, so everything S9 claims rests on layers 2, 3, 6
  and 8.

---

## 12. Master-plan amendments

| Section | Change |
|---|---|
| §4.9 | Askpass is Conductor-written, not a host path; `GIT_CONFIG_NOSYSTEM` added, with the reason |
| §4.5 criterion 6 | Qualified to admit an authorized `POLICY_SENSITIVE`, or criterion 7 is unreachable |
| §5.2 | Corrections 5 and 6 added to the existing list of four |
| Part 8 S9 | File list corrected; `enforce/secrets.rs` explicitly not created |
| Part 9 | The rows 12/13/25/30 deferral notes marked **DISCHARGED**, kept rather than deleted |
| CLAUDE.md | "S0 done, S1 not started" was stale through S8; now S0–S9 |

---

## 13. Security implications

`SECURITY.md` is new and is the deliverable this slice's stop point names. Every
`PREVENTED` row carries a mechanism, a test, and a positive control; every test
named in it was verified to exist. Rows that are not prevented say so plainly —
including the ones that are uncomfortable, such as absolute-path credential
reads and Conductor's inability to undo an effect.

---

## 14. Push / parity

```
commit       abb76f0  feat: S9 — the gates were decided, and nothing called them
pushed       6afdd3d..abb76f0  main -> main
local HEAD   abb76f0fe1ed72db766351ee64c9cc8b17b8ef8e
origin/main  abb76f0fe1ed72db766351ee64c9cc8b17b8ef8e   (verified after a fresh fetch)
working tree clean
```

Recorded in a follow-up rather than by amending: `abb76f0` is already on the
remote, and rewriting published history is forbidden by CLAUDE.md.

---

## 15. Next slice

**S10 — First real adapter: Codex.** The hard gate is passed, so a real agent may
now execute through Conductor. S10 must inspect the actually-installed Codex
version and CLI help rather than assuming flags, keep the fake agent as the
primary CI harness forever, and run the S3 crash matrix with Codex substituted.

**S9 COMPLETE — CONTINUING AUTOMATICALLY**
