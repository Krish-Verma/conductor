# S8 — Approvals — completion report

**Starting HEAD:** `2200316` (S7)
**Ending HEAD:** recorded at commit time
**Date:** 2026-08-14
**Status:** COMPLETE (mechanism); three acceptance rows deliberately left for S9 — see below

---

## Objective

§4.3's approval architecture: durable, exactly-scoped, expiring approvals over a socket the
agent cannot reach. Four distinct kinds, `binding_hash` scoping, TTL, one-shot vs reuse,
revocation semantics, persistence across restart, and the operator-nonce mechanism.

## Interruption and recovery

The first implementation agent was terminated mid-slice by an API session limit. Roughly
2,700 lines of implementation and 1,381 of tests survived but did not build: no
`approval/mod.rs`, the module unregistered in `lib.rs`, and a `SideEffectKind` variant named
in the tests that does not exist. The tree was snapshotted to `.git/conductor-recovery/`
before anything was touched; nothing was reset or rewritten.

**Two of the surviving 30 tests disagreed with the implementation, and the implementation
was right.** Both asserted `authorize()` returns `NoMatchingGrant` for a consumed or revoked
grant; the implementation returns `AlreadyConsumed` / `Revoked`. The safety property held
either way — the operation is refused — so what was at stake was only which reason a human
is handed. The deciding evidence was internal: the *same test* already expected `Revoked`
from `consume` on the next assertion, so it demanded two names for one fact while the
implementation was self-consistent. The assertions were **tightened** to the specific
variant, not loosened. Reporting "no matching grant" to an operator who had a grant and
spent it sends them to write a policy rule for an action that was already approved — the
same distinction §4.6 draws between `IdenticalFingerprint` and `BudgetExhausted`.

## Implementation

**Model** (`kind.rs`) — the four kinds carry their `Subject` and `ExpiryRule` together, so a
plan approval and a policy approval are not two values of one type. `ExpiryRule` makes "a
plan approval with a TTL" and "a policy approval without one" unconstructible.

**Binding** (`binding.rs`) — `blake3(action ‖ canonical(facts) ‖ policy_hash ‖ scope)` with
domain separation, recomputed at use time and never read back from the row.

**Store** (`store.rs`, schema v6) — two separate state machines. v6 rebuilds
`approval_request`/`approval_grant` to add `kind`, make `run_id` nullable (a plan approval
authorizes a plan version, not a run) and `expires_at` nullable (two kinds never expire),
preserving existing rows as `POLICY_APPROVAL`.

**Socket** (`conductor-cli/src/socket.rs`) — publication by **bind → chmod → `rename(2)`**
inside a directory `mkdir(2)` creates `0700`. Stale-vs-live decided by attempting to
connect; deletion detected by `(dev, ino)` identity.

**CLI** (`conductor-cli/src/approval.rs`) — `approval {list,show,approve,deny,revoke}`.
Granting is mutating and goes through the socket, never a file an agent could write.

## Files changed

Created: `conductor-run/src/approval/{mod,authorize,binding,gate,kind,nonce,revoke,store}.rs`
· `conductor-run/tests/{approval,layering,approval_restart}.rs` ·
`conductor-run/src/bin/conductor-s8-approval-victim.rs` ·
`conductor-cli/src/{socket,approval}.rs` · `conductor-cli/tests/approval.rs`.

Modified: `conductor-store/src/{schema,migrate,run,lib}.rs` · `conductor-run/src/{lib,recovery}.rs`
· `conductor-cli/src/main.rs` · `conductor-run/Cargo.toml`.

**Also fixed, out of slice** (see "Two pre-existing flakes" below):
`conductor-run/tests/{host_probe,supervise,recovery}.rs`.

## Tests

**766 passing, 0 failing** (S7: 680). Delta +86 (38 from the final S8 push, 48 earlier).

```
cargo fmt --check                                          exit 0
cargo check --all-targets                                  Finished
cargo clippy --all-targets --all-features -- -D warnings   Finished, no warnings
cargo test --all --no-fail-fast                            766 passed; 0 failed
```

All re-run independently by the orchestrator on the restored tree.

## Failure injection

**Fifty kill-restart cycles.** Real `SIGKILL` to fifty separate processes, each asserted to
have died of signal 9. Each cycle attempts the shared one-shot grant, commits a request with
a cycle-unique TTL, and on every third cycle writes a row in a transaction it never commits.
After each kill the store is reopened and `PRAGMA integrity_check` must return `ok`.

Result: **exactly one** cycle consumed the grant; the other 49 were refused with the
"already consumed" reason. Every committed TTL came back exact; every uncommitted row was
absent. The assertion is `== 1`, not `<= 1` — zero would satisfy "never twice" and prove
nothing.

## Mutation / non-vacuity verification

| # | Mechanism | Tests killed | Positive control |
|---|---|---|---|
| 1 | `binding_hash` exact match | 7 + 1 CLI | "the same grant DOES authorize its own operation" passed |
| 2 | no-double-consume | 2 + both 50-cycle tests | fresh-grant-after-50-kills fails from the other side |
| 3 | TTL/expiry | 1 unit + 2 integration | the `now = 999` half of the boundary passed |
| 4 | source-scan layering | 2 | scanner self-control over 7 real approval files |
| 5 | revocation state | 3 + 1 CLI | "authorizes before the revocation" passed |
| 6 | socket mode → `0666` | 1 integration; **0 unit on first run** | — see below |
| 7 | `authorize` refuses everything (audit) | 9 | — see below |

**Independently reproduced by the orchestrator:** the layering mutation (wiring an approval
path into `conductor-probe-action` → 2 tests red, reverted, `git diff` empty) and mutation 7
(`authorize` never authorizing → the new positive control fails at the exact assertion added
for it).

**Two vacuous tests found, one of them minutes old.**

- **Mutation 6** killed no unit test, because three socket assertions compared the observed
  mode against `SOCKET_MODE` — the code agreeing with itself, so changing the constant
  changed the expectation. Fixed to assert §7.3's literal `0o600`/`0o700`, with the constant
  checked separately; re-running the mutation then failed 2 unit tests.
- **Mutation 7** was an audit of the 30 pre-existing tests. With `authorize` refusing
  everything, 9 failed but `the_binding_is_recomputed_at_use_time_and_not_read_back_from_the_row`
  **passed** — it asserted only a refusal, which a function that never authorizes anything
  satisfies trivially. The implementing agent correctly reported it rather than editing it.
  A positive control was added at review and proven to fail under that mutation.

Residue: `grep -rn "if false\|if true" crates/` empty; all mutated files restored.

## Two pre-existing flakes, fixed out of slice

Neither was an S8 regression — S8 never touched `containment/` — but S8's new test binaries
raised parallel load enough to expose both. Both failed in the **safe** direction, and both
were fixed because a gate that intermittently refuses is a gate people learn to re-run.

- **`host_probe.rs`** — six tests driving the real host (spawning `codex`, entering seatbelt,
  binding sockets) contend for those resources. `--test-threads=1` passed 6/6, which is what
  identified contention rather than a wrong measurement. Serialized with a `Mutex`; no new
  dependency.
- **`supervise.rs::a_successful_agent_is_spawned_streamed_and_reaped`** — shared the 600 ms
  `idle_timeout` meant for tests that actually measure timeouts. Under load the idle timer
  fires first and the supervisor correctly kills the agent. **This is precisely the finding
  S3 recorded** — "the product was right and the test was wrong. Budgets are now separated by
  what the test is measuring" — with this test left outside the fix. Given a `completing()`
  budget.

Measured effect: from ~1 failure per 3 full-suite runs to **766/0 on three consecutive
runs** at the end of the slice.

**Residual, stated because it is not zero.** One later full-suite invocation reported
765/1. It did not reproduce, and the failing test could not be named — the run that
produced the count and the runs that searched for the name were separate invocations, and
the three that followed were clean. So the honest position is: the two *identified*
load-dependent flakes are fixed and each has a named root cause, and a third, rarer one may
remain unidentified. It has never produced a false pass. This is recorded rather than
rolled over, because "run it again until it is green" is how a real intermittent failure
becomes invisible. If it recurs, the first move is capturing one invocation's output whole
rather than sampling two.

## Skills used

`superpowers:test-driven-development`, `superpowers:systematic-debugging` (for the flake
diagnosis), `superpowers:using-superpowers`.

## Subagents used

Two. The first was terminated by an API session limit mid-slice; the orchestrator recovered
its work, repaired the module tree, resolved the two test/implementation disagreements, and
diagnosed and fixed both flakes. The second completed the socket, CLI, layering test and
50-cycle test, and produced the mutation evidence. The orchestrator independently re-ran all
gates and reproduced two mutations.

## Master-plan amendments

1. **§4.3** — "the shim" does not exist in v1; the census is the two agent-position binaries,
   and absence is proven three ways.
2. **Acceptance rows 12, 13, 25** — mechanism built, call site is S9's; scoring recorded so
   the v1 sweep cannot mark them `PASS` on unit coverage.

## ADRs

None. S8 raised frictions (below) but no decision that reversed a plan position.

## Security implications

The honest claim is narrow and stated as such. S8 makes a grant authorize **exactly one**
operation, expire, be spendable once, survive restart, and be revocable with a defined
outcome in every state. It does **not** claim that a grant proves a human made it, that
`0600` is a boundary, or that the agent cannot connect — those are properties of the
execution mode (ADR-0002), measured per host, and acted on by §4.2's eligibility check.
This host delivers **tier C**, and `serve` prints §4.3's "Not a boundary. Approvals are
advisory." verbatim.

## Known limitations

- **Rows 13 and 12's "resumes on grant" are NOT enforced**; row 25 is mechanism-only.
  Nothing in the run path creates an `approval_request` yet — S9 owns it.
- **Nonce activation is manual** (`--arm-nonce`). S8's scope says "activated when
  `control_surface < Hard`", but the measured `control_surface` is per (adapter × launcher),
  not a property of the socket server, so there is no single value for `serve` to read. The
  decision belongs to §4.2's eligibility check, which S9 wires.
- **Tier B's CLI happy path has no automated test** — arming needs a controlling terminal and
  testing it would print a secret to the operator's tty. The load-bearing half (a mutating
  verb refused without the nonce, allowed with it) is unit-tested with a positive control.
- **§7.1 spelling divergence**: §7.1 lists top-level `conductor approve`/`conductor deny`;
  what exists is `conductor approval {…}`. Flagged, not silently reconciled.
- **`approval serve` is a hidden fourteenth command**, needed until S14's daemon auto-start.
- **The socket server is single-threaded** with a 10 s per-client read timeout. S14 owns
  concurrency.
- **Revoking a runless grant** (plan approval, review acceptance) is refused explicitly
  rather than fabricating a fence over a nonexistent run; S11/S13 own those kinds.

## Next slice

**S9 — Enforcement and post-run audit. ⛔ HARD GATE before S10.** No real agent runs before
it exists. Env allowlist, per-run `HOME` and `TMPDIR`, failing `GIT_ASKPASS`, `SSH_AUTH_SOCK`
unset, secret scanner, the full post-run audit surface including `/tmp` delta scanning, and
`SECURITY.md` populated with **measured** values — no item listed as prevented without a
passing test. S9 also owns the call sites S7 and S8 deliberately left open: eligibility at
launch (row 30) and request creation from a `require_approval` decision (rows 12, 13, 25).

---

**S8 COMPLETE — CONTINUING AUTOMATICALLY**
