# S10 — First real adapter: Codex

**Status:** COMPLETE
**Starting commit:** `c02ac60`
**Ending commit:** *(this commit)*
**Stop point:** *"One real slice of real work completes on a fixture repo."* — reached.

---

## 1. Objective

> Replace the fake agent with `codex exec` behind the same interface.

S9 was the hard gate and it passed, so a real coding agent was permitted to
execute through Conductor for the first time. One did, and completed a real task
with a real commit.

**Real Codex invocations across the whole slice: 5** (3 paid). The cap was 6.
Every real-agent test is `#[ignore]`d, so `cargo test` never spends money — the
master plan's rule is that the fake agent stays the primary CI harness *forever*.

---

## 2. Measured against codex-cli 0.142.0 — the plan was mostly right, in four places wrong

Nothing was assumed from the master plan's text; `codex exec --help` was read on
the installed binary first.

| Master plan said | Measured | Action |
|---|---|---|
| "`-C` requires `--permissions-profile`; use cwd instead" | `-C/--cd <DIR>` is unconditional; `--permissions-profile` **does not exist** on 0.142.0 | §6.2 corrected |
| `--ephemeral` listed under hermeticity | It discards the session, so it is incompatible with `resume`, which the same slice scopes | Dropped; §6.2 corrected |
| `--output-schema` gives a structured final report | It shapes **every** `agent_message` — the recorded run emits five, and the first four claim `PARTIAL` | §6.2 corrected; report is `--output-last-message`'s file, last message as fallback |
| *(silent)* | `files_touched` carries **absolute** paths | §6.2 corrected; adapter normalises, and a genuinely-outside path is left alone as evidence |

And one that is not about flags: **Codex blocks indefinitely reading `stdin`**
when stdin is not a TTY. Conductor is immune only because `supervise::spawn`
uses `Stdio::null()`, chosen for unrelated reasons — so the immunity was
accidental. It is now recorded as a requirement, and the adapter refuses to
build a command with an empty prompt rather than producing an invocation that
would hang forever.

---

## 3. An interface that could not say what the agent did

Codex's `file_change` item carries an **array** of paths. `parse_event` returned
`Option<AgentEvent>` — one line, at most one event — so the adapter reported the
first path and silently dropped the rest. Every multi-file edit understated what
the agent did.

Nothing failed. §4.8 reconciles against git, which sees all of them, so no test
would ever have caught it. That is exactly the situation S10's own instruction
is written for:

> If any scenario needs adapter-specific handling, that is a design smell to fix
> in the interface, not the adapter.

The trait now returns `Vec<AgentEvent>`, across all three implementors and both
supervisor call sites. The adapter emits one event per change, in order, with
each change's kind preserved. The honest *limitation* test that documented the
old behaviour was replaced by a *capability* test that asserts all three changes
arrive — cementing the limitation would have been worse than the limitation.

---

## 4. `resume` was unreachable, and the fake agent hid it

S10's scope names `resume`. It could not happen.

- `attempt.agent_session_id` was written from exactly one place: the session
  Conductor **assigned before the run** (`worker.rs`).
- `repair::driver::previous_session` reads that same column to decide what a
  retry may resume.
- Codex's `conductor_assigned_session_id` is `false`. §6.2: *"Session identity
  arrives in `thread.started`, so it cannot be pre-assigned."* The announced id
  reached `run_one_attempt` in `supervised.events` and **went nowhere**.

So the column was always `NULL` for Codex, `previous_session` always `None`, and
`CodexAgent::resume_command` was unreachable through the product.

The fake agent is the other kind of adapter — Conductor assigns its session and
it hands the same one back — so every test written before S10 asked a question
that could not fail. `tests/agent_session.rs` is the fix, and the reason it did
not exist earlier.

**A surviving mutant improved the design.** The first version used
`COALESCE(agent_session_id, ?2)` and documented it as "never clears". Removing
the `COALESCE` failed no test — because the caller only writes when there *is*
an announcement, so the clearing case is unreachable by construction. The
`COALESCE` was defending against nothing while the doc comment claimed a
guarantee the code was not providing. It was removed, and the case that actually
exists was tested instead: when Conductor assigned one session and the agent
announced another, **the announcement wins** — the assignment was a request, and
only the agent knows what it created. Reinstating the `COALESCE` now fails that
test.

---

## 5. §4.9's "the adapter's own auth variable" does not fit Codex

Measured, without reading any credential value:

```
~/.codex/auth.json  →  auth_mode: "chatgpt",  OPENAI_API_KEY: null
                       (the credential is a token pair in `tokens`)

HOME=<per-run>                       codex login status → Not logged in
HOME=<per-run> CODEX_HOME=~/.codex   codex login status → Logged in using ChatGPT
```

What Codex needs is `CODEX_HOME` — a **directory pointer, not a secret**. §4.9's
clause was written for the API-key case and does not cover it.

Pointing it at the operator's real `~/.codex` is wrong in two directions, and
the second is the serious one:

1. It hands the agent `config.toml`, every profile file, and the entire session
   history — none of which is a credential.
2. **Codex writes into `CODEX_HOME`.** A contained run would leave session
   rollouts in the operator's home: outside the workspace, outside §4.8's
   reconciled surface, outside the per-run `TMPDIR` audit. The containment story
   would have a hole shaped exactly like its own foundation.

`enforce::env::materialize_credential_home` is the answer: a per-run directory
**inside the workspace**, `0700`, containing **only** the files the adapter
names, each `0600`, excluded from git so a credential can never become a commit,
dying with the workspace and inside the audit surface. It fails closed on a
missing file rather than producing an empty home that fails later as an opaque
`401`.

Tested with a **synthetic** credential and a source directory that also contains
`config.toml` and a `sessions/` history — both asserted absent from the result. A
test that copied a live credential to prove copying works would have proved it by
doing the thing it guards against.

The allowlist was **not** weakened. The path travels by name through
`agent_env_extra`, the existing mechanism, and the allowlist test shows it as an
extra key rather than letting it in silently.

---

## 6. Stop point — reached

`a_real_codex_completes_one_slice_of_real_work_on_a_fixture_repo` drives
`vertical::run_task` with the real `codex` binary on a disposable fixture repo:
`PENDING → READY → RUNNING → RECONCILING → VERIFYING → COMPLETE`.

Asserted by reading **git**, not by trusting a return value:

- exactly **one** commit above base on `conductor/T-0012/r-0041`
- `git show --name-only` is exactly `lib.rs`
- `git show <sha>:lib.rs` contains `double` **and** still contains `base` — the
  agent extended the file rather than replacing it
- verdict `CLEAN_COMPLETE`; run and task both `COMPLETE`

One earlier invocation is worth recording: it *also* reached `COMPLETE` with a
real commit and failed only the test's assertion, because the fixture had both
`lib.rs` and `src/lib.rs` and Codex chose the other one. That is a defensible
reading of an ambiguous instruction, so the **fixture** was disambiguated rather
than the assertion loosened. Conductor was correct in both runs.

---

## 7. Crash matrix with Codex substituted

The master plan's Verify line asks for the entire S3 crash matrix.

**Real Codex (2 points):**
- `after-outcome-recorded` — real agent, real work, Conductor `SIGKILL`ed before
  it read the repository. Recovery converged; the real edit survived byte-for-byte.
- an unauthenticated run — a real process that reaches the model's door and is
  refused; routes to `REPAIRING` with an empty workspace.

**Recorded Codex bytes through the real adapter, real supervisor, real
reconciliation and real recovery (everything else):** all **13** `RunPoint`s —
asserted against `RunPoint::ALL`, so a point dropped from the enum fails the test
rather than silently shrinking the matrix — plus all 8 agent-kill points, stall,
torn JSONL, auth error, missing report, schema-violating report, multi-file
change, and an escaped-path report.

The replay process is **not a second fake agent**: it speaks `codex exec`'s argv
and emits bytes recorded from the real one. It also **refuses to start** unless
the argv carries `--sandbox workspace-write`, `--ignore-user-config`,
`--ignore-rules`, `--json`, `--cd`, `--output-last-message`, a readable and
parsing `--output-schema`, and a non-empty prompt. That guard is the only thing
in the system checking that the *caller* writes the schema file, and an adapter
that silently dropped `--sandbox` would pass every parsing test in
`conductor-agent/tests/codex.rs`.

Recordings were used wherever a real agent adds nothing: the first seven kill
points never spawn a process at all, and beyond them the question is whether
§4.7 converges — which the database and the repository decide, not the model.

**No adapter-specific handling was needed anywhere in core.** The Codex worker
binary is S3's worker with the adapter line changed. There is no
`if adapter == "codex"`, and a grep for one is part of the drift audit below.

---

## 8. Non-vacuity evidence

| Mechanism | Mutation | Caught by |
|---|---|---|
| Report selection | take the **first** agent message | `the_report_is_the_last_agent_message_not_the_first_schema_shaped_one` (`Partial` vs `Complete`) |
| Path normalisation | invert the `is_absolute` test | `absolute_paths_in_files_touched_are_made_workspace_relative` + 5 others |
| Path normalisation | naive `starts_with` instead of component-wise | `a_path_genuinely_outside_the_workspace_is_left_alone_because_it_is_evidence` — `/workspace-other/lib.rs` became `-other/lib.rs` |
| Malformed-line tolerance | `unwrap_or(Value::Null)` | `a_truncated_final_line_does_not_consume_the_lines_before_it` |
| Sandbox flag | `workspace-write` → `danger-full-access` | `the_sandbox_is_workspace_write_never_danger_full_access`, `resume_uses_the_thread_id_and_keeps_every_containment_flag` |
| Session id field | `thread_id` → `session_id` | `thread_started_carries_the_session_id_codex_assigned_itself` |
| Unknown event tolerance | unknown type → `Err` | `an_unknown_future_event_type_is_ignored_not_rejected` |
| **Session persistence** | discard the announced session | `a_session_the_agent_announces_is_recorded_on_the_attempt` |
| **Session precedence** | reinstate `COALESCE` | `an_announced_session_wins_over_the_one_conductor_assigned` |

The last two are the ones worth reading: the first is the defect S10 found in
core, and the second is a mutant that **survived** the initial suite and was
killed by deleting dead code rather than by adding a test to cover it.

---

## 9. Verification

```
scripts/run-tests.sh
  suites:  <filled at commit>
  passed:  <filled at commit>
  failed:  0

cargo fmt --all --check                                    clean
cargo clippy --all-targets --all-features -- -D warnings    clean

real-agent suite (not run by default):
  cargo test -p conductor-run --test codex_integration -- --ignored  → 3 passed
```

---

## 10. Architecture drift audit

Searched implementation (not comments) for every rejected concept. Clean:
no `WorkflowRuntime`, Temporal, Hatchet, DAG engine, scheduler, cron, DSL, event
replay as authoritative state, worktree execution, hardlinked clone, automatic
merge/rebase, `RepositoryEvidenceProvider`, or Nerve reference. The only
`hardlink` matches are the `--no-hardlinks` flag ADR-0001 requires and a test
that deliberately builds a hardlinked clone as a negative control.

`grep` for adapter-specific branching in core (`adapter == "codex"` and
equivalents) returns nothing.

---

## 11. Skills and subagents

**Used:** `superpowers:test-driven-development` throughout.

**Subagents:** two, on disjoint file scopes — one for the adapter, one for the
integration and crash matrix. Both were given explicit "do not touch" lists and
both respected them. Both reported findings they were not authorised to fix
rather than fixing them out of scope, which is what surfaced §4 and §5; the
orchestrator made those changes.

Every subagent claim that mattered was **independently reproduced** before being
accepted — including the S4 secret-scanner defect carried over from S9, which
turned out to be worse than reported.

---

## 12. Known limitations

- **`CODEX_HOME` is materialised but not yet wired into the CLI's launch path.**
  The mechanism and its tests are in the product; the integration tests build it
  themselves. Wiring it to `conductor task run` is small and belongs with the
  adapter-selection work.
- **Real-agent tests are non-blocking by design** and are not run in the default
  suite. That is the master plan's rule, and it means adapter regressions against
  a *new* Codex release are caught only when somebody runs the ignored suite.
- **`--output-schema` enforcement is the agent's, not Conductor's.** A schema
  violation is detected at parse time and routed as row 5; nothing prevents the
  model from emitting one.
- **S3's crash matrix lists "duplicate spawn" and `crash_matrix.rs` does not
  test it.** It is a lease/fence property and adapter-independent. Noted, not
  fixed here.

---

## 13. Master-plan amendments

| Section | Change |
|---|---|
| §6.2 | `-C/--cd` row corrected; `--ephemeral` removed from hermeticity; three measured behaviours added (`--output-schema` shapes every message, absolute `files_touched`, the stdin block) |
| §6.1 | `parse_event` returns `Vec<AgentEvent>`; the reason recorded in the trait's own docs |
| §4.9 | The "adapter's own auth variable" clause extended to cover a credential **directory**, with the per-run materialisation and its two reasons |

---

## 14. Push / parity

*(filled in at commit)*

---

## 15. Next slice

**S11 — Plan ledger and decisions.** Its pure core (model, canonical hashing,
`plan validate`) is already built and green, including the forward-dependency
rule §3.7 names and which cycle detection does not imply. What remains is
persistence, task materialisation, `.conductor/**` rejection at reconciliation,
and the database-loss reconstruction proof.

**S10 COMPLETE — CONTINUING AUTOMATICALLY**
