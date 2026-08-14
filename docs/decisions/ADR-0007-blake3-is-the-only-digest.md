# ADR-0007 — BLAKE3 is Conductor's only content digest; the `sha256` column was incidental wording

**Status:** ACCEPTED
**Date:** 2026-08-13
**Slice:** S4 (raised by the implementing subagent rather than decided by it)

---

## Question

§4.5 says verification logs have their "sha256 recorded", and Part 5.1 declared
`artifact.sha256 TEXT NOT NULL`. But §2.2's authorized dependency list is
`rusqlite`, `clap`, `serde`/`serde_json`/`serde_yaml`, **`blake3`**,
`thiserror`/`anyhow`, `tokio`, `tempfile` — it contains **no SHA-2 implementation**.
Meanwhile S3 had already established `content_hash()` = `blake3:<hex>` for
side-effect preconditions, and every other hash in the design (plan hash, policy
hash, `binding_hash`, `operation_id`) is BLAKE3.

So: add `sha2`, or rename the column?

## Why this needed a ruling rather than a choice

The S4 subagent hit this while recording log digests and **declined to pick a side
silently**, which was correct. Either resolution is small, but they point in opposite
directions:

- Adding `sha2` introduces an unauthorized runtime dependency to satisfy a word.
- Renaming edits the master plan's own schema.

Silently doing the first is how dependency lists rot: nobody ever adds a crate on
purpose, they add it because a comment said "sha256". Silently doing the second is a
schema change with no record.

## Decision

**BLAKE3 is the only content digest in Conductor v1. The column is renamed to
`content_hash` (schema v3, forward migration).**

Reasons, in order of weight:

1. **§2.2's dependency list is deliberate, and "sha256" in §4.5 is not.** One is a
   considered architectural constraint with a stated rationale; the other is a
   generic word for "a hash of the file". Treating incidental prose as binding over
   an explicit dependency decision inverts the hierarchy.
2. **Hash agility costs nothing here and confusion costs something.** Two digest
   algorithms in one system means every reader of every stored digest must ask which
   one produced it.
3. **The stored values are self-describing** (`blake3:<hex>`), so nothing is
   misrepresented at the value level — only the column name was lying.
4. **BLAKE3 is already a dependency, is faster, and is what every other hash in the
   design uses.** Adding SHA-2 would make Conductor slower and larger to be less
   consistent.
5. **Nothing had written the table yet**, so the migration is free. This is the
   cheapest moment this decision will ever have.

Conductor has no interoperability requirement forcing SHA-2 — no external verifier
consumes these digests. If one ever appears (a registry, a signing format, an
attestation standard), that is a real requirement and a new ADR, not a reason to
keep a misnamed column now.

## What this DOES prove

- Schema v3 applies as a forward migration, v1 and v2 untouched, with migration
  idempotency and per-step `integrity_check` still passing.
- No SHA-2 implementation is present in `Cargo.lock`.

## What this DOES NOT prove

- It says nothing about BLAKE3's suitability for any adversarial use. These digests
  are integrity and cache-key material, **not** authentication. `binding_hash`
  (§4.3) is the one security-relevant hash, and its security rests on the approval
  architecture, not on digest choice.
- It does not survive an external interoperability requirement, which does not exist
  today.

## Pre-registered falsification / revisit trigger

1. An external consumer requires a specific digest algorithm for these artifacts.
2. Any second digest algorithm entering the codebase for any reason — at which point
   the "one digest" property is already lost and should be re-argued, not drifted
   into.
3. A stored digest is ever written without its `blake3:` prefix, which would remove
   the self-description this decision partly rests on.

## Master-plan deltas

**(1) §4.5** — "sha256 recorded" becomes "content hash (BLAKE3) recorded".
**(2) Part 5.1** — `artifact.sha256` becomes `artifact.content_hash`, with a note
that schema v3 performs the rename.

## Impacted master-plan sections

§4.5 (verification logs) · Part 5.1 (`artifact`) · §2.2 (dependency list, unchanged
but now load-bearing for this) · slice S4.
