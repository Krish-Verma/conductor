//! The three loop-breakers and the budget — master plan §4.6.

use super::config::RepairConfig;
use super::failure::{Failure, progressed};

/// What one attempt produced, as far as repair is concerned.
///
/// Four cases, because §4.6's breakers and §4.7's budget need different things
/// from each. `Failed` is the only one a loop-breaker can read — the other three
/// carry no fingerprint, and a breaker that treated an absence of evidence as
/// evidence of repetition would stop runs that were merely unlucky.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptResult {
    /// Verification failed.
    Failed(Failure),
    /// The attempt changed nothing (§4.8's `NO_CHANGE`).
    NoChange,
    /// The agent died before any verification result existed.
    ///
    /// Distinct from [`AttemptResult::NoChange`], and the distinction is
    /// acceptance row 2's: "Crash before edits … new attempt, same packet …
    /// `COMPLETE`". An agent that *ran to completion* and edited nothing is
    /// breaker 3's empty edit; an agent that *died* is an attempt that never
    /// got the chance, and the row requires it be retried. Both spend the work
    /// budget, because both consumed a spawn.
    Crashed,
    /// Something around the work broke (§4.7's infrastructure retry).
    ///
    /// The one result that costs no work budget: §4.7 says an infrastructure
    /// retry "does **not** consume budget", and "conflating them is how a broken
    /// API key silently exhausts a task's budget". It is still an invocation,
    /// so it still counts against S6's hard invocation ceiling.
    Infrastructure,
}

/// Which limit stopped the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLimit {
    /// §4.6's `repair.max_attempts`.
    RepairMaxAttempts,
    /// §4.6's `repair.escalate_after`.
    EscalateAfter,
    /// Part 5.1's `task.attempt_budget`.
    TaskAttemptBudget,
    /// §4.6's `repair.max_infra_retries` — acceptance row 8's "infra retry ×1".
    ///
    /// A separate limit because §4.7 requires the two kinds "never conflated",
    /// and because the two send a person to different places: a work budget that
    /// ran out is a task that was too big, and infrastructure retries that ran
    /// out is a host that is broken.
    InfrastructureRetries,
}

/// Why repair stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Loop-breaker 1.
    IdenticalFingerprint,
    /// Loop-breaker 2.
    Oscillation,
    /// Loop-breaker 3.
    EmptyEdit,
    /// `progressed()` said no, for a reason with no more specific name.
    NoProgress,
    /// The budget is spent.
    BudgetExhausted {
        /// Which of the three limits bound.
        limit: BudgetLimit,
    },
}

/// What to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Spend one repair attempt.
    Repair,
    /// Stop, and hand the run to a human.
    Stop(StopReason),
}

/// Every attempt of one run, oldest first. Entry 0 is the initial attempt.
#[derive(Debug, Clone, Default)]
pub struct RepairHistory {
    entries: Vec<AttemptResult>,
}

impl RepairHistory {
    /// An empty history.
    pub fn new() -> RepairHistory {
        RepairHistory {
            entries: Vec::new(),
        }
    }

    /// Record what an attempt produced.
    pub fn record(&mut self, result: AttemptResult) {
        self.entries.push(result);
    }

    /// Every attempt, oldest first.
    pub fn entries(&self) -> &[AttemptResult] {
        &self.entries
    }

    /// How many attempts spent the task's work budget — §4.7's "work retry".
    ///
    /// Everything except [`AttemptResult::Infrastructure`]. §4.7 is explicit
    /// that an infrastructure retry "does **not** consume budget", and the
    /// exemption has to be applied *here*, in the count, rather than at one of
    /// the three places the count is compared against a limit: a rule enforced
    /// at each call site is a rule that is missing from the next one.
    pub fn work_attempts(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| !matches!(entry, AttemptResult::Infrastructure))
            .count()
    }

    /// How many attempts were spent on infrastructure rather than on the work.
    pub fn infrastructure_attempts(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, AttemptResult::Infrastructure))
            .count()
    }

    /// How many **repair** attempts have been spent.
    ///
    /// Work attempts minus the initial one, which was not a repair. An
    /// infrastructure attempt is not in the subtrahend either — a run whose
    /// first spawn hit a broken adapter has still not made its initial *work*
    /// attempt.
    pub fn repairs_used(&self) -> usize {
        self.work_attempts().saturating_sub(1)
    }

    /// The most recent failure, if the last attempt produced one.
    pub fn last_failure(&self) -> Option<&Failure> {
        match self.entries.last() {
            Some(AttemptResult::Failed(failure)) => Some(failure),
            _ => None,
        }
    }
}

/// §4.6's window: "Detected by keeping the last 4."
///
/// The number is a bound on memory, not on truth. A window of every attempt
/// ever would eventually find a repeat in any long repair and stop a run that
/// was making progress; a window of two is breaker 1 with extra steps.
pub const OSCILLATION_WINDOW: usize = 4;

/// Whether repair may spend another attempt, and if not, why.
///
/// Called **after** the latest attempt's result has been recorded, so
/// `history.last()` is what just happened.
///
/// # The order is the design
///
/// The three loop-breakers are asked before the budget, because §4.6's breaker 1
/// says "stop immediately; **do not spend attempt 2**" — a run that is provably
/// looping should be reported as looping, not as merely out of money. A human
/// reading `BudgetExhausted` looks for a task that was too big; a human reading
/// `IdenticalFingerprint` looks for an agent that cannot see its own mistake.
/// Both stop the loop, so no safety property depends on the order; the reason a
/// person is handed does.
pub fn decide(history: &RepairHistory, config: &RepairConfig, attempt_budget: i64) -> Decision {
    let entries = history.entries();
    if entries.is_empty() {
        return Decision::Repair;
    }

    // ---- breaker 3: an attempt that changed nothing -----------------------
    //
    // §4.6 breaks on "a **repair attempt** producing `NO_CHANGE`". The *first*
    // attempt of all producing nothing is acceptance row 2 — "crash before
    // edits … new attempt, same packet … `COMPLETE`" — which is a retry the
    // suite requires, so the breaker deliberately does not reach it.
    //
    // "Repair attempt" is counted in **work** attempts: a run whose first spawn
    // died on a broken adapter has not yet had an attempt to repeat, so the
    // empty edit that follows is still the first one.
    if history.work_attempts() > 1 && matches!(entries.last(), Some(AttemptResult::NoChange)) {
        return Decision::Stop(StopReason::EmptyEdit);
    }

    // Only a `Failed` attempt carries a fingerprint, so only a `Failed` attempt
    // can take part in a comparison. `Crashed` and `Infrastructure` are skipped
    // rather than treated as breaks in the chain: a crash between two identical
    // failures does not make them two different failures.
    let failures: Vec<&Failure> = entries
        .iter()
        .filter_map(|entry| match entry {
            AttemptResult::Failed(failure) => Some(failure),
            AttemptResult::NoChange | AttemptResult::Crashed | AttemptResult::Infrastructure => {
                None
            }
        })
        .collect();

    if let [.., prev, next] = failures.as_slice() {
        // ---- breaker 1: the same failure twice ----------------------------
        if next.fingerprint() == prev.fingerprint() && config.stop_on_identical_fingerprint {
            return Decision::Stop(StopReason::IdenticalFingerprint);
        }

        // ---- breaker 2: A → B → A -----------------------------------------
        //
        // A *non-adjacent* repeat inside the window. Adjacency is what
        // separates this from breaker 1: `A → A` is the same failure again,
        // which is breaker 1's case and is reported as such (or, when breaker 1
        // is switched off, as the absence of progress). Treating an adjacent
        // repeat as oscillation would make `stop_on_identical_fingerprint:
        // false` unobservable, because breaker 2 would stop every loop breaker
        // 1 was told not to.
        let window: &[&Failure] = if failures.len() > OSCILLATION_WINDOW {
            &failures[failures.len() - OSCILLATION_WINDOW..]
        } else {
            &failures
        };
        for (i, earlier) in window.iter().enumerate() {
            for later in window.iter().skip(i + 2) {
                if earlier.fingerprint() == later.fingerprint() {
                    return Decision::Stop(StopReason::Oscillation);
                }
            }
        }

        // ---- the predicate itself -----------------------------------------
        if !progressed(prev, next) {
            return Decision::Stop(StopReason::NoProgress);
        }
    }

    // ---- the budget -------------------------------------------------------
    //
    // Four limits over two different things, so the smallest applicable one
    // binds. They are asked most-specific first only so the reported limit is
    // the informative one; any of them stopping is the loop stopping.
    //
    // The infrastructure limit is asked **only when infrastructure was the last
    // thing that happened**. §4.5 says a report holding a `FAIL` "has real work
    // to do", so a run whose latest attempt failed a check must be reported
    // against a work limit even if a broken adapter cost it two spawns earlier
    // — otherwise the person called in goes looking at the host instead of at
    // the failing check.
    if matches!(entries.last(), Some(AttemptResult::Infrastructure))
        && history.infrastructure_attempts() > config.max_infra_retries
    {
        return Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::InfrastructureRetries,
        });
    }

    let repairs_used = history.repairs_used();
    if repairs_used >= config.max_attempts {
        return Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::RepairMaxAttempts,
        });
    }
    if repairs_used >= config.escalate_after {
        return Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::EscalateAfter,
        });
    }
    // Part 5.1's `attempt_budget` counts *every* work attempt, the initial one
    // included, which is why this reads the count and not `repairs_used`.
    if attempt_budget <= 0 || history.work_attempts() as i64 >= attempt_budget {
        return Decision::Stop(StopReason::BudgetExhausted {
            limit: BudgetLimit::TaskAttemptBudget,
        });
    }

    Decision::Repair
}
