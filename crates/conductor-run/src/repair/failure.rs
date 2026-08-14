//! What one failed verification looked like, and whether the next one is
//! better — master plan §4.6.

use std::collections::BTreeSet;

use super::fingerprint::Fingerprint;

/// One verification failure, reduced to what repair decisions are made from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Failure {
    failing_checks: BTreeSet<String>,
    fingerprint: Fingerprint,
    tree_hash: String,
    assertion: String,
}

impl Failure {
    /// Build one. The fingerprint is computed rather than supplied, so a
    /// `Failure` whose fingerprint disagrees with its own contents cannot exist.
    pub fn new(
        failing_checks: impl IntoIterator<Item = String>,
        assertion: &str,
        tree_hash: &str,
    ) -> Failure {
        let failing_checks: BTreeSet<String> = failing_checks.into_iter().collect();
        let fingerprint =
            Fingerprint::compute(failing_checks.iter().map(String::as_str), assertion);
        Failure {
            failing_checks,
            fingerprint,
            tree_hash: tree_hash.to_string(),
            assertion: assertion.to_string(),
        }
    }

    /// Which checks failed.
    pub fn failing_checks(&self) -> &BTreeSet<String> {
        &self.failing_checks
    }

    /// §4.6's fingerprint.
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// The tree the checks observed.
    pub fn tree_hash(&self) -> &str {
        &self.tree_hash
    }

    /// The first failing assertion, as read from the log.
    pub fn assertion(&self) -> &str {
        &self.assertion
    }
}

/// §4.6's `progressed()`, exactly as written:
///
/// ```text
/// progressed(prev, next) :=
///       next.failing_checks ⊂ prev.failing_checks              # strictly fewer
///    OR (next.fingerprint ≠ prev.fingerprint AND tree changed) # different problem, real edit
/// ```
///
/// Two details the symbols carry and prose loses:
///
/// * **`⊂` is proper containment.** Reading it as `⊆` would make every repeated
///   failure count as progress, which disables all three loop-breakers by
///   arithmetic rather than by bug.
/// * **The second disjunct needs both halves.** A different failure at an
///   unchanged tree is a flaky check, not a fix, and spending an attempt on it
///   is exactly the waste §4.6 exists to bound.
///
/// # The regression guard, which §4.6 as written does not have (ADR-0008)
///
/// Read literally, the second disjunct makes a repair that *fixed nothing and
/// broke something new* count as progress: `{alpha} → {alpha, beta}` has a
/// different fingerprint (the failing-check set is hashed into it) at a changed
/// tree, so both halves hold. Every check that failed before still fails, one
/// more fails now, and the loop is told to keep going.
///
/// That is the silent direction of error this module's sibling documents as the
/// dangerous one: a stuck run spends its whole budget and nothing announces it.
/// So a **proper superset** — everything that failed before still failing, plus
/// more — short-circuits to `false` before the disjuncts are read.
///
/// The guard is deliberately narrow. `{alpha} → {beta}` is *not* a superset:
/// alpha was fixed and beta was revealed, which is the ordinary shape of real
/// repair, and it stays progress via the second disjunct. Only unambiguous
/// regression is refused.
pub fn progressed(prev: &Failure, next: &Failure) -> bool {
    // Everything that failed before still fails, and something else does too.
    let regressed = prev.failing_checks.is_subset(&next.failing_checks)
        && next.failing_checks != prev.failing_checks;
    if regressed {
        return false;
    }

    let strictly_fewer = next.failing_checks.is_subset(&prev.failing_checks)
        && next.failing_checks != prev.failing_checks;
    let different_problem_after_a_real_edit =
        next.fingerprint != prev.fingerprint && next.tree_hash != prev.tree_hash;
    strictly_fewer || different_problem_after_a_real_edit
}
