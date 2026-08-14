# S7 — Policy engine — completion report

**Starting HEAD:** `bc6da8f` (S6)
**Ending HEAD:** recorded at commit time
**Date:** 2026-08-14
**Status:** COMPLETE

---

## Objective

§4.4's policy architecture: typed actions, two-stage evaluation with a locked ceiling,
deterministic fact extractors, canonical snapshots pinned for a run's lifetime,
`conductor policy explain`, `unknown → deny`, and §4.2's `execution_requirements`
eligibility gate consuming S2.5's measured capabilities.

## Implementation

**`model.rs`** — `Effect` with the total order `allow < require_approval < deny`, joined by
`max` and met by `min`. The 22 typed actions. `Fact` with `source`. Rules, documents,
exceptions, and the four non-configurable built-in invariants.

**`load.rs`** — global + project YAML, canonical serialization (sorted keys, no
timestamps), BLAKE3 `blake3:<hex>` per ADR-0007, persisted to the `policy_snapshot` table
that has existed since schema v1. Malformed policy is a hard named error, never a silent
permissive default. Includes a hand-written RFC 3339 **UTC-only** parser (§2.2 has no date
crate) that rejects offsets rather than guessing.

**`evaluate.rs`** — §4.4's two stages, kept genuinely separate:

```
ceiling = join over locked GLOBAL rules only                              (Stage 1)
joined  = max(action.floor, ceiling, builtin, global, project, task)      (Stage 2)
effect  = joined                                       if no exception matches
        = min(joined, max(exception, ceiling, builtin, action.floor))     if one does
```

**`facts.rs`** — the deterministic extractors of §4.4's table: manifest dependency diff,
lockfile path match, `git config --get-regexp '^remote\.'` before/after, migration globs,
workspace-root comparison, secret scan (reusing `verify::secrets`, not reimplemented).
`architecture.change` is modelled as `model_assisted` and therefore capped at
`require_approval`, never `deny`, always with the diff attached.

**`explain.rs` / CLI** — action, resolved effect, the ceiling that applied, every rule that
matched **and every rule considered that did not, with the reason**, facts and sources,
policy hash, exceptions with scope and expiry.

**`eligibility.rs`** — §4.2's ~50-line comparison. Stale or absent probe refuses. Does not
rank adapters and does not choose between eligible options.

**Run-lifetime pinning.** `conductor task run` previously wrote the placeholder blob `"{}"`;
it now resolves policy, canonicalizes, persists the snapshot and pins `run.policy_hash`. A
malformed policy file stops the run.

## Files changed

Created: `conductor-run/src/policy/{model,load,evaluate,facts,eligibility,explain,mod}.rs`
(1098/802/614/469/374/145/41) · `conductor-run/tests/{policy,policy_snapshot}.rs`
(1466/737) · `conductor-cli/src/policy.rs` (226) · `conductor-cli/tests/policy.rs` (319).

Modified (5 files, +69/−17): `conductor-run/src/lib.rs` · `conductor-cli/src/main.rs` ·
`conductor-cli/src/task.rs` (real snapshot pinning) · `conductor-git/src/{lib,reconcile}.rs`
(`glob_match` made `pub`).

**On the `conductor-git` change**, which was outside the scope given: policy migration globs
and `architecture.change` proxy globs are the same "does this path match" question, and a
second matcher would be a second set of over-matching semantics to review — over-matching
being the dangerous direction, since it silently widens scope. Reviewed and accepted: it is
a visibility change plus a doc comment, no logic touched, no new dependency.

## Tests

**680 passing, 0 failing** (S6: 594). Delta +86: 38 `tests/policy.rs` · 19
`tests/policy_snapshot.rs` · 6 CLI · 21 unit · 2 `compile_fail` doctests.

```
cargo fmt --check                                          exit 0
cargo check --all-targets                                  Finished
cargo clippy --all-targets --all-features -- -D warnings   Finished, no warnings
cargo test --all                                           680 passed; 0 failed
```

All four re-run independently by the orchestrator on the restored tree.

## The precedence matrix

Five positions × {absent, allow, require_approval, deny} = **1024 cells, every one
asserted**, with `assert_eq!(cells, 1024)` so the sweep cannot silently shrink. Expected
values come from **hand-written literal 3×3 join and meet tables**, not from `max`/`min` —
so a wrong operator in the implementation cannot agree with itself. Twelve cells are
additionally written out by hand as a control on the generator, and the literal tables have
their own 12-value control.

| Case | Result |
|---|---|
| project rule vs global `deny` | stays `deny` (join only tightens) |
| project rule `deny` vs global `allow` | becomes `deny` |
| exception vs **unlocked** global `require_approval` | loosens to `allow` |
| exception vs **locked** global `require_approval` | clamped to `require_approval` |
| exception vs locked `deny` | stays `deny` |
| exception trying to raise an effect | ignored (meet) |
| any configuration vs a built-in invariant | invariant wins |
| unknown action, any configuration | `deny` |

## Mutation / non-vacuity verification

Five mechanisms mutated; **none left the suite green**. Every denial test carries a
**positive control that passes under the mutation**, which is what rules out a fixture that
prevented the bad outcome by construction.

| # | Mechanism | Mutation | Tests killed | Positive control |
|---|---|---|---|---|
| 1 | locked ceiling | drop `.join(ceiling)` from the exception clamp | 3 | `positive_control_the_same_loosening_succeeds_without_the_lock` |
| 2 | `unknown → deny` | `Action::floor` always `Allow` | 4 (3 binaries) | `positive_control_the_same_empty_policy_allows_a_known_action` |
| 3 | deny needs deterministic facts | `may_carry_a_deny` → `true` | 7 (3 binaries) | `positive_control_the_same_rule_denies_on_a_deterministic_fact` |
| 4 | run-lifetime pinning | pin lookup → newest snapshot | 2 | second run pinned to the tightened policy |
| 5 | stale-probe refusal | fabricate all-`Hard` caps on cache miss | 3 | `positive_control_the_same_requirements_pass_against_the_fresh_row` |

**Independently reproduced by the orchestrator** (not taken from the implementing agent's
report): mutation 1 → 3 tests fail with `left: Allow / right: RequireApproval`, the
exception having loosened past the lock; mutation 2 → 4 tests fail across three binaries.
The positive controls were confirmed passing under mutation in both cases. Restored tree:
`grep -rn "if false" crates/` empty, 680 green.

**A vacuity was found and fixed during the exercise.** Mutation 4's first run left the suite
**green**. The pinning test seeded only one run and one snapshot, so "newest snapshot" and
"this run's snapshot" were the same row — the assertion could not fail. Reseeded with a
second run pinned to the tightened policy; the mutation then fails. This is the fourth
consecutive slice in which a test that looked strong was proven vacuous only by mutating the
mechanism it claimed to cover.

## Skills used

`superpowers:test-driven-development`, `superpowers:using-superpowers`.

## Subagents used

One focused implementation subagent for the whole slice. The orchestrator independently
re-ran all four gates, reproduced two of five mutations, verified the two-stage evaluation
had not collapsed into a single join, and reviewed both out-of-scope file changes. The agent
correctly declined to edit `docs/` and reported five findings for the orchestrator to record.

A peer Claude session in the same worktree reported S7 "complete and integrated" while this
agent was still mid-write, and was about to build S8 on it. Corrected against `git log`;
that session halted having written nothing. Recorded because it is the failure mode
CLAUDE.md names: **file presence is not slice completion**.

## Master-plan amendments

1. **§4.4 Stage 1** — the ceiling's load-bearing work is on exceptions and grants; "a
   project can always tighten, never loosen" is true but vacuous, since `max` makes
   loosening structurally impossible for a rule.
2. **§4.4 built-ins** — two of the four are conditions, not actions; modelled as
   fact-conditioned, with their real enforcement points named.
3. **§4.4 deny cap** — scoped to a rule's own `when:` facts (ADR-0010).
4. **§4.4** — global policy path (XDG) recorded; expiry timestamps are RFC 3339 UTC-only.
5. **Acceptance row 30** — decided at S7, **enforced at S9**; must be scored `NOT RUN`.

## ADRs

- **ADR-0010** — the deny cap is scoped to a rule's own facts; the literal reading is a
  weakening vector a model could pull.

## Security implications

S7 decides but does not enforce. The policy engine now determines what Conductor will do;
making the environment rather than the prompt the boundary is S9's. Two properties here are
security-relevant in their own right: `unknown → deny` means an incomplete taxonomy cannot
read as permission, and `tool_interception` is structurally incapable of satisfying a gating
requirement, so S0's measured hook bypasses cannot be laundered into eligibility.

## Known limitations

- **Eligibility is not wired into the attempt-launch path.** `eligibility::check` is a
  tested pure function; the call site belongs to S9. **Acceptance row 30 is `NOT RUN`, not
  `PASS`.**
- **`policy explain` exits 0 even for a `deny`.** §7.2's exit code 4 remains unclaimed;
  S9 owns it.
- **Fact provenance is asserted by the extractor**, not verified. S7 has no mechanism for
  detecting a mislabelled `source`, and none is claimed (ADR-0010).
- **The deny cap depends on rules declaring dependencies honestly in `when:`.** A rule
  denying on grounds it does not name would evade the cap — a policy-authoring hazard.
- **No model-assisted fact production.** Only the type that prevents such a fact from ever
  producing a `deny`.
- The RFC 3339 parser is UTC-only by choice; a policy written with an offset is rejected
  rather than converted.

## Next slice

**S8 — Approvals.** Durable, exactly-scoped, expiring approvals over a socket the agent
cannot reach: `binding_hash`, TTL, one-shot vs reuse, revocation in each of four states,
persistence across restart, the four distinct approval kinds, and the operator-nonce
mechanism (default off, activated when `control_surface < Hard`). Note ADR-0010's carried
constraint: a grant must not become a cheaper path around a capped deny.

---

**S7 COMPLETE — CONTINUING AUTOMATICALLY**
