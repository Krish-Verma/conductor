# ADR-0015 — Approval survives the store as a receipt, not as a re-decision

**Status:** ACCEPTED
**Date:** 2026-08-16
**Slice:** S11 (found implementing T9, the slice's verify line)

---

## Question

S11's verify line is §3.2's stated invariant:

> Delete `conductor.db`, rebuild: no plan, decision, policy or verification
> definition is lost.

and §3.5 puts *"every approved plan"* on the **Not lost** list.

But §5.2 says of the plan machine:

> **Authority:** `APPROVED` only via a human at the control socket.

A §4.3 grant is a row. It lives only in `conductor.db`. Once the store is gone,
**no witness exists anywhere on the machine**, so `ledger::approve` — which
re-derives a witness, consumes a grant, and refuses without one — cannot be
replayed. Read literally, the two requirements contradict each other:

**Can a rebuild restore `APPROVED`, and if so, on whose authority?**

## Why the answer matters to Conductor

This is the hinge of §3.1's whole split. If approval cannot be reconstructed,
then §3.2's four reasons plans live in git are false in the one case they were
written for: the first reason is *literally* "an approved plan must survive loss
of `conductor.db`". A machine that lost its database would need a human to
re-approve every plan it had ever approved, and `materialize`'s §4.3 gate —
which refuses any plan version that is not `APPROVED` — means no task could run
until they did.

If approval *can* be reconstructed, the mechanism is a path that writes
`APPROVED` without a grant. §3.3 exists because `.conductor/` is inside the
agent's workspace and an agent *can* write `.conductor/plans/v3/APPROVED`. A
careless reconstruction is therefore a self-approval hole with extra steps.

## Evidence

**1. The sidecar is not an input to the approval decision. It is an output of
one.** `ledger::approve` writes it **last**, in a documented order:

```
1. re-derive the witness (refuses before any state moves)
2. move the state through §5.2's legality table
3. consume the grant                    ← the human decision is spent here
4. record the approval in the store
5. supersede earlier versions
6. write the APPROVED sidecar           ← only now
```

A sidecar therefore cannot exist unless a real grant was consumed at step 3.
It is a **receipt**, in exactly the sense §3.4 already relies on for commit
trailers: *"the audit trail for anything consequential survives total local
state loss and travels with the repository."* §3.4 is the same argument applied
to commits; nobody calls a commit trailer a second door onto authority.

**2. The trust boundary is unchanged, because §3.3's control 1 is what guards
the file.** A change to `.conductor/**` arriving on a run branch is rejected at
reconciliation unconditionally and never fetched (ADR-0014 gave that its own
verdict so no approval can launder it). An agent therefore cannot put a sidecar
into the **registered** working tree — which is the only tree
`adopt_approval` reads, because the root comes from the `project` row and there
is no path parameter that could offer another.

Anyone who *can* write that tree could already run `conductor plan approve`.
Reconstruction does not widen who can approve; it changes nothing about the
answer to "who could forge this".

**3. What a receipt is trusted for must be bounded by what it cannot control.**
`adopt_approval` checks every field against an independent source:

| Field | Checked against | Refusal |
|---|---|---|
| `plan_version` | the id derived from the project and directory | `Disagreement` |
| `version` | the directory it was found in | `Disagreement` |
| `content_hash` | a **fresh** hash of the plan document | `Edited` |
| `content_hash` | the row `register_plan_version` just wrote | `Disagreement` |
| `approved_at` | RFC 3339, never defaulted to `0` | `UnreadableSidecar` |

Every refusal happens **before** the state moves, so a refused adoption leaves
the version exactly where registration put it.

## Decision

**Reconstruction re-reads a receipt. It does not mint authority.**

`ledger::adopt_approval` restores `APPROVED` from the `APPROVED` sidecar in the
registered working tree, taking **no `Authorization` witness and spending no
grant** — because there is no second human decision to authorize. It walks
§5.2's machine (`VALIDATED → AWAITING_APPROVAL → APPROVED`) rather than writing
the state directly, so the legality table remains the only thing that decides
which transitions exist.

§5.2's "only via a human at the control socket" is **retained and now qualified**:
it governs how a plan *becomes* approved. It does not govern how a machine that
lost its database learns that a human already did so.

## Consequences

- §3.2's first reason for plans living in git is now true rather than aspirational.
- An **edited** plan is `Edited` here exactly as it is in `verify_approval`. A
  rebuild is not a way to launder a changed document into an approval — the
  receipt names a hash, and the hash is recomputed from the file every time.
- `conductor recover` is the operator-facing door (§7.1's thirteenth command).
  It is explicit and human-invoked; nothing adopts a receipt automatically on
  store open.
- **Residual risk, stated rather than mitigated:** the receipt is exactly as
  trustworthy as the registered working tree. A party with direct write access
  to that tree *outside* Conductor's run-branch path can author a sidecar. This
  is unchanged from before this ADR — such a party could invoke `plan approve` —
  and it is outside §3.3's threat model, which is about what an agent can do
  from its own clone. It is recorded here so nobody later reads the absence of a
  cryptographic binding as an oversight.
- **Not done:** the receipt carries `policy_hash` but nothing binds it to a
  signature or to the grant id. Binding it would need a key, and a key needs a
  place to live that an agent cannot read — which is §4.3's unsolved problem,
  not this ADR's.

## Impacted sections

- **§3.5** — the recovery path now has an implementation: `plan::reconstruct`.
- **§5.2 (Plan)** — the "Authority" clause is qualified, not weakened.
- **§7.1** — `conductor recover` implemented, without `--scan` (S14 owns it).

## Evidence index

- `crates/conductor-run/src/plan/reconstruct.rs` — the path and its ordering.
- `crates/conductor-run/src/plan/ledger.rs::adopt_approval` — the receipt checks.
- `crates/conductor-run/tests/plan_reconstruct.rs` — both of §3.5's lists.
- `crates/conductor-cli/tests/recover.rs` — the same, in a separate process.
- Mutation evidence: removing the adoption kills 4 of 5 tests; removing the
  receipt/document hash check kills the edited-plan test specifically. Both
  mutations were verified present in the source before the result was read.
