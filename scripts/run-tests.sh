#!/usr/bin/env bash
# Deterministic, attributable test run.
#
# Why this exists (S9): a prior session observed a single "765 passed / 1 failed"
# event and could not name the failing test, because the summary line and the
# failure detail came from two different invocations. One invocation, one log,
# one machine-readable list of failures — so the next occurrence is attributable
# on the spot rather than requiring a re-run to reproduce.
#
# Usage:  scripts/run-tests.sh [extra cargo test args...]
# Output: docs/../ (nothing tracked) — logs land in $CONDUCTOR_TEST_LOG_DIR,
#         defaulting to a scratch dir outside the repository.

set -uo pipefail

CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
LOG_DIR="${CONDUCTOR_TEST_LOG_DIR:-${TMPDIR:-/tmp}/conductor-test-logs}"
mkdir -p "$LOG_DIR"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
HEAD_SHA="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
RUN_ID="${STAMP}-${HEAD_SHA}-$$"
LOG="$LOG_DIR/test-${RUN_ID}.log"

{
  echo "# conductor test run"
  echo "run_id:    $RUN_ID"
  echo "utc:       $STAMP"
  echo "head:      $(git rev-parse HEAD 2>/dev/null || echo nogit)"
  echo "dirty:     $(git status --porcelain 2>/dev/null | wc -l | tr -d ' ') file(s)"
  echo "cargo:     $($CARGO --version)"
  echo "host:      $(uname -srm)"
  echo "args:      $*"
  echo
} >"$LOG"

# --no-fail-fast so one failing suite does not hide the rest.
# Every suite's own "test result:" line is preserved in the log, and libtest
# prints the fully-qualified name of each failure under "failures:".
"$CARGO" test --all --no-fail-fast "$@" >>"$LOG" 2>&1
CARGO_STATUS=$?

# Per-suite tallies. Each suite emits exactly one "test result:" line.
PASSED=$(awk '/^test result:/ {s+=$4} END {print s+0}' "$LOG")
FAILED=$(awk '/^test result:/ {s+=$6} END {print s+0}' "$LOG")
IGNORED=$(awk '/^test result:/ {s+=$8} END {print s+0}' "$LOG")
SUITES=$(grep -c '^test result:' "$LOG")

# The names, not just the count. libtest lists them in a "failures:" block that
# repeats each fully-qualified test path on its own indented line.
FAILLIST="$LOG_DIR/failures-${RUN_ID}.txt"
awk '
  /^failures:$/      { infailures=1; next }
  /^test result:/    { infailures=0 }
  infailures && /^    [A-Za-z_]/ { gsub(/^ +/,""); print }
' "$LOG" | sort -u >"$FAILLIST"

{
  echo
  echo "## summary"
  echo "suites:  $SUITES"
  echo "passed:  $PASSED"
  echo "failed:  $FAILED"
  echo "ignored: $IGNORED"
  echo "cargo_exit: $CARGO_STATUS"
  if [ -s "$FAILLIST" ]; then
    echo "failing tests:"
    sed 's/^/  - /' "$FAILLIST"
  fi
} | tee -a "$LOG"

echo "log:      $LOG"
echo "failures: $FAILLIST"

# A nonzero cargo exit with no named failure is itself a reportable anomaly:
# it means a suite died without libtest reporting, and silently treating that
# as "1 unexplained failure" is how an unattributable flake happens again.
if [ "$CARGO_STATUS" -ne 0 ] && [ ! -s "$FAILLIST" ]; then
  echo "ANOMALY: cargo exited $CARGO_STATUS but no test name was captured." >&2
  echo "         A suite likely aborted (signal, link error, or harness crash)." >&2
  grep -nE 'error|Segmentation|signal|SIGKILL|SIGABRT|panicked at' "$LOG" | tail -20 >&2
fi

exit "$CARGO_STATUS"
