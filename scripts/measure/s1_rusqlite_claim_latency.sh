#!/usr/bin/env bash
# S1 re-measurement of the run claim under the SHIPPING stack.
#
# ADR-0004 pre-registered falsification trigger 1: S0 measured SQLite-via-Python
# (3.53.3), not `rusqlite`. This script re-measures using the real store code
# (`conductor_store::Store::claim_next_run`, the production open path and the
# production pragmas) with separate writer PROCESSES, matching S0's process
# model so the numbers are comparable.
#
# Configurations:
#   A  fullfsync=1, think_ms=1   -- production durability, genuinely contended.
#                                  Directly comparable to ADR-0004's fullfsync
#                                  table (rows=300, repeat=2). THE HEADLINE.
#   B  fullfsync=1, think_ms=0   -- production durability, uncontended.
#   C  fullfsync=0, think_ms=1   -- measurement-only downgrade, comparable to
#                                  ADR-0004's "contended" table.
#
# Every result file carries its own provenance and a unique name: S0's report
# records a shared result path being silently clobbered by a concurrent run.
# The instrument refuses to overwrite an existing --out.
#
# Usage:
#   scripts/measure/s1_rusqlite_claim_latency.sh              # all three
#   scripts/measure/s1_rusqlite_claim_latency.sh self-test    # checkers only

set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
results_dir="$repo_root/scripts/measure/results"
cd "$repo_root"

bin="$repo_root/target/release/conductor-claim-bench"
echo "building the instrument (release) ..."
cargo build --release -p conductor-store --bin conductor-claim-bench

rusqlite_version="$(
  awk '/^name = "rusqlite"$/{found=1; next} found && /^version = /{gsub(/[",]/,"",$3); print $3; exit}' \
    "$repo_root/Cargo.lock"
)"
git_short="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"

echo "rusqlite ${rusqlite_version} · commit ${git_short} · ${stamp}"
echo

echo "=== instrument self-test (ADR-0004 decision 4) ==="
"$bin" --self-test
echo

if [[ "${1:-}" == "self-test" ]]; then
  exit 0
fi

run_config() {
  local label="$1"; shift
  local out="$results_dir/s1_rusqlite_claim_${label}_${stamp}_${git_short}.json"
  echo "=== ${label} -> $(basename "$out") ==="
  "$bin" --label "$label" --rusqlite-version "$rusqlite_version" --out "$out" "$@"
  echo
}

# A — production durability, contended. Mirrors ADR-0004's fullfsync run.
run_config fullfsync_contended \
  --writers 1,4,16 --rows 300 --repeat 2 --think-ms 1

# B — production durability, uncontended.
run_config fullfsync_uncontended \
  --writers 1,4,16 --rows 2000 --repeat 3 --think-ms 0

# C — fullfsync off. Measurement only; not a shipping configuration.
run_config nofullfsync_contended \
  --writers 1,4,16 --rows 2000 --repeat 3 --think-ms 1 --fullfsync-off

# D, E — production durability at a realistic ARRIVAL RATE rather than under
# saturation. Configurations A-C keep every writer claiming in a tight loop, so
# their p99 is dominated by queueing at 100% utilisation. Conductor claims a run
# when a run starts -- a few times per hour with 1-4 concurrent runs -- so the
# gap between claims, not the claim itself, is what determines the tail a user
# ever sees. These two configurations are what §2.6 revisit trigger 3 is
# actually about; A is retained as the saturation ceiling. (ADR-0005)
run_config arrivalgap_25ms \
  --writers 1,4 --rows 200 --repeat 2 --think-ms 25

run_config arrivalgap_250ms \
  --writers 1,4 --rows 200 --repeat 2 --think-ms 250

echo "results written under $results_dir"
