# ADR-0010 — "a `deny` must rest only on deterministic facts", read literally, is a weakening vector

**Status:** ACCEPTED
**Date:** 2026-08-14
**Slice:** S7 (raised by the implementing subagent rather than decided by it)

---

## Question

§4.4 states:

> Every fact carries `source: deterministic | model_assisted | human`. A
> `require_approval` may rest on any; **a `deny` must rest only on
> `deterministic` facts.** A model must never be the sole reason Conductor blocks
> work — a hallucinated block is indistinguishable from a real one and trains the
> user to override blocks.

The rationale is unambiguous and correct. The implementation question is: **which
facts does "rest on" quantify over?**

Two readings:

- **(A) Every fact in the request.** If any fact accompanying an evaluation is
  `model_assisted`, no rule may produce `deny`.
- **(B) The facts the rule itself names** in its `when:` clause.

## Why this needed a ruling rather than a choice

Reading (A) is the literal one, and it inverts the rule's own purpose.

The stated intent is to stop a model from **causing** a block. Under (A), a model
gains the power to **remove** one: attach a single unrelated `model_assisted`
observation to a request — an `architecture.change` proxy-glob hit, say — and an
otherwise fully deterministic `deny` on `git.push` downgrades to
`require_approval`. The deny then rests on a human clicking approve, which is
exactly the "trains the user to override blocks" outcome the paragraph exists to
prevent.

That makes (A) a **weakening vector reachable by the agent under evaluation**,
since agents are what produce the diffs the proxy globs match. A rule intended to
constrain models would become a lever they can pull.

It is also the wrong direction on this project's standing asymmetry. Over-capping
a deny is silent — work proceeds to an approval prompt and nothing announces that
a block was dissolved. Under-capping is loud — a human sees a block they can
question. §4.6's normalizer doctrine and ADR-0008 both resolve ambiguity toward
the loud failure; this resolves the same way.

## Decision

**The cap is scoped to the facts a rule's own `when:` clause depends on
(reading B).**

- A rule whose `when:` names any `model_assisted` or `human` fact cannot produce
  `deny`; its effect is capped at `require_approval` and the cap is recorded and
  surfaced in `policy explain`.
- A rule whose `when:` names only `deterministic` facts denies normally,
  regardless of what other facts the request happens to carry.
- A rule with **no** `when:` is a standing policy statement resting on no facts at
  all, and still denies. ("Never push to a remote" needs no evidence.)

The intent of §4.4 is preserved exactly — a model still cannot be the reason
Conductor blocks work — while the inverse capability is removed.

## What this DOES prove

- Mutation 3 (`FactSource::may_carry_a_deny` forced to `true`) fails 7 tests
  across 3 binaries, including
  `a_deny_resting_on_a_model_assisted_fact_is_capped_at_require_approval` and
  `a_human_sourced_fact_also_cannot_carry_a_deny`.
- The positive control `positive_control_the_same_rule_denies_on_a_deterministic_fact`
  passes under that mutation, which is what proves the cap — and not the fixture —
  is what removed the deny.
- `policy explain` names the cap and the offending fact source, so a capped deny
  is visible rather than silently softened.

## What this DOES NOT prove

- It says nothing about whether the `architecture.change` proxy globs are a *good*
  `model_assisted` signal. They are a proxy by §4.4's own admission; this ADR only
  governs what such a fact may do to an effect.
- It does not address facts whose declared `source` is itself wrong. Fact
  provenance is asserted by the extractor that produced it; S7 has no mechanism
  for detecting a mislabelled fact, and none is claimed.
- Scoping depends on rules declaring their dependencies honestly in `when:`. A
  rule that denies on grounds it does not name would evade the cap. This is a
  policy-authoring hazard, not an enforcement one.

## Pre-registered falsification / revisit trigger

1. A real policy needing a `deny` that legitimately depends on a `model_assisted`
   fact — which would mean §4.4's prohibition itself is too strong, a bigger
   question than this scoping.
2. Any evaluation path where a fact outside a rule's `when:` can change that
   rule's effect in **either** direction, which would mean the scoping is not
   actually implemented.
3. S8 granting approvals in a way that lets a capped deny be satisfied more
   cheaply than an uncapped one.

## Master-plan deltas

**(1) §4.4** — the deny-cap sentence gains "meaning the facts that rule names in
its `when:`, not every fact in the request", with the weakening-vector rationale.

## Impacted master-plan sections

§4.4 (policy architecture, facts and derivation) · slice S7 · slice S8 (grants
must not become a cheaper path around a capped deny) · acceptance rows 13, 24.
