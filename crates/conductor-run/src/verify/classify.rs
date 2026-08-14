//! Turning what one check execution did into one of §4.5's four outcomes.
//!
//! Pure. No I/O, no clock, no process. Given the same facts it returns the same
//! outcome on any machine, which is what lets the interesting cases —
//! ENOSPC, a `SIGKILL` from outside, a tree that moved under a green test — be
//! tested without staging each one for real.

use conductor_git::{TreeHash, VerificationOutcome};

use super::profile::OnTimeout;

/// How a check process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Termination {
    /// It ran to completion and returned a code.
    Exited {
        /// The exit code.
        code: i32,
    },
    /// Conductor killed it for exceeding its budget.
    TimedOut,
    /// It died on a signal that Conductor did not send.
    Signalled {
        /// The signal number.
        signal: i32,
    },
    /// The process could not be started at all.
    SpawnFailed {
        /// What the operating system said.
        detail: String,
    },
    /// Something around the check broke: the log could not be written, the
    /// workspace could not be read, the toolchain could not be fingerprinted.
    Infrastructure {
        /// What broke.
        detail: String,
    },
}

/// Whether the tree was the same before and after the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeWitness {
    /// Hashed before and after, and the two agree.
    Held {
        /// The tree both hashes report.
        tree: TreeHash,
    },
    /// Hashed before and after, and they disagree — §4.5's `VOID`.
    Moved {
        /// What the tree was when the check started.
        before: TreeHash,
        /// What it was when the check ended.
        after: TreeHash,
    },
    /// One of the two hashes could not be taken, so nothing can be said about
    /// which tree the check observed.
    Unknown {
        /// Why.
        detail: String,
    },
}

/// Everything one execution of one check produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    /// How the process ended.
    pub termination: Termination,
    /// What the tree did underneath it.
    pub tree: TreeWitness,
    /// What the profile says a timeout means for this check.
    pub on_timeout: OnTimeout,
}

/// A verification finding. Findings never auto-resolve (§4.8).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VerificationFinding {
    /// The `finding.kind` written to the database.
    pub kind: &'static str,
    /// Evidence, in Conductor's own words.
    pub detail: String,
}

/// `finding.kind` for a tree that moved under a running check.
pub const TREE_MUTATED_DURING_CHECK: &str = "TREE_MUTATED_DURING_CHECK";
/// `finding.kind` for a tree hash that could not be taken.
pub const TREE_UNOBSERVABLE: &str = "TREE_UNOBSERVABLE";
/// `finding.kind` for two runs of a flaky check disagreeing.
pub const FLAKY_CHECK_DISAGREEMENT: &str = "FLAKY_CHECK_DISAGREEMENT";
/// `finding.kind` for an unrecognised key in `verification.yaml`.
pub const PROFILE_UNKNOWN_KEY: &str = "PROFILE_UNKNOWN_KEY";
/// `finding.kind` for a secret pattern found in a captured log.
pub const SECRET_IN_VERIFICATION_LOG: &str = "SECRET_IN_VERIFICATION_LOG";
/// `finding.kind` for a cached result contradicting a fresh one at the same key.
pub const VERIFICATION_NONDETERMINISM: &str = "VERIFICATION_NONDETERMINISM";

/// One execution's outcome, with whatever it obliges a human to look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// The §4.5 outcome.
    pub outcome: VerificationOutcome,
    /// Findings raised.
    pub findings: Vec<VerificationFinding>,
}

/// Classify one execution.
///
/// **The tree witness is examined first, and it is decisive.** §4.5 makes a
/// result whose tree moved `VOID` "not `PASS`", and the same reasoning applies
/// to `FAIL`: a failure observed on a tree that no longer exists would send a
/// repair agent after a defect that may be in no tree at all. So `VOID`
/// outranks every termination, including a clean exit 0.
pub fn classify(execution: &Execution) -> Classified {
    match &execution.tree {
        TreeWitness::Moved { before, after } => {
            return Classified {
                outcome: VerificationOutcome::Void,
                findings: vec![VerificationFinding {
                    kind: TREE_MUTATED_DURING_CHECK,
                    detail: format!(
                        "the working tree moved from {before} to {after} while the \
                         check was running, so its result describes no tree that \
                         ever existed"
                    ),
                }],
            };
        }
        TreeWitness::Unknown { detail } => {
            return Classified {
                outcome: VerificationOutcome::Void,
                findings: vec![VerificationFinding {
                    kind: TREE_UNOBSERVABLE,
                    detail: format!(
                        "the working tree could not be hashed around the check \
                         ({detail}), so which tree it observed is unknown"
                    ),
                }],
            };
        }
        TreeWitness::Held { .. } => {}
    }

    let outcome = match &execution.termination {
        Termination::Exited { code: 0 } => VerificationOutcome::Pass,
        Termination::Exited { .. } => VerificationOutcome::Fail,
        Termination::TimedOut => match execution.on_timeout {
            OnTimeout::Inconclusive => VerificationOutcome::Inconclusive,
            OnTimeout::Fail => VerificationOutcome::Fail,
        },
        Termination::Signalled { signal } => {
            if signal_is_a_program_fault(*signal) {
                VerificationOutcome::Fail
            } else {
                VerificationOutcome::Inconclusive
            }
        }
        // Neither says anything about the code under test.
        Termination::SpawnFailed { .. } | Termination::Infrastructure { .. } => {
            VerificationOutcome::Inconclusive
        }
    };

    Classified {
        outcome,
        findings: Vec::new(),
    }
}

/// Combine the runs of a check configured with `flaky_retry`.
///
/// §4.5: "exactly one; disagreement ⇒ INCONCLUSIVE" — explicitly *not* "the
/// good one wins". A flaky check that is allowed to pass on its second try is a
/// check that reports green for a tree it failed on; one that is allowed to
/// fail sends a repair agent after a race. Neither is a fact about the tree, so
/// neither is `PASS` or `FAIL`.
pub fn combine_flaky(runs: &[Classified]) -> Classified {
    let mut findings: Vec<VerificationFinding> =
        runs.iter().flat_map(|r| r.findings.clone()).collect();

    let Some(first) = runs.first() else {
        return Classified {
            outcome: VerificationOutcome::Inconclusive,
            findings,
        };
    };

    // VOID first: a run whose tree moved contaminates the pair, because the
    // other run observed a different tree by definition.
    if runs.iter().any(|r| r.outcome == VerificationOutcome::Void) {
        return Classified {
            outcome: VerificationOutcome::Void,
            findings,
        };
    }

    if runs.iter().all(|r| r.outcome == first.outcome) {
        return Classified {
            outcome: first.outcome,
            findings,
        };
    }

    let observed: Vec<String> = runs.iter().map(|r| format!("{:?}", r.outcome)).collect();
    findings.push(VerificationFinding {
        kind: FLAKY_CHECK_DISAGREEMENT,
        detail: format!(
            "the runs of this check disagreed ({}), so neither is a fact about \
             the tree",
            observed.join(" then ")
        ),
    });
    Classified {
        outcome: VerificationOutcome::Inconclusive,
        findings,
    }
}

/// Whether a signal means the check's own process faulted, or means something
/// outside the check ended it.
///
/// The distinction is the one D8 turns on: a segfault in the code under test is
/// a defect and belongs in repair, while a `SIGKILL` from an out-of-memory
/// killer or an operator says nothing about the code. Anything unrecognised is
/// treated as external, because `INCONCLUSIVE` asks a human and `FAIL` spends
/// an agent attempt.
pub fn signal_is_a_program_fault(signal: i32) -> bool {
    matches!(
        signal,
        libc::SIGILL
            | libc::SIGTRAP
            | libc::SIGABRT
            | libc::SIGBUS
            | libc::SIGFPE
            | libc::SIGSEGV
            | libc::SIGSYS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> TreeHash {
        TreeHash::from_stored(name)
    }

    fn held(termination: Termination) -> Execution {
        Execution {
            termination,
            tree: TreeWitness::Held { tree: tree("t1") },
            on_timeout: OnTimeout::Inconclusive,
        }
    }

    #[test]
    fn exit_zero_is_pass() {
        let c = classify(&held(Termination::Exited { code: 0 }));
        assert_eq!(c.outcome, VerificationOutcome::Pass);
        assert!(c.findings.is_empty());
    }

    #[test]
    fn a_non_zero_exit_is_fail() {
        let c = classify(&held(Termination::Exited { code: 1 }));
        assert_eq!(c.outcome, VerificationOutcome::Fail);
        assert!(
            c.findings.is_empty(),
            "an ordinary failure is not a finding"
        );
    }

    #[test]
    fn a_timeout_is_inconclusive_because_that_is_what_the_profile_configures() {
        // §4.5: `on_timeout: inconclusive   # NOT failure`. Acceptance row 8
        // spends no budget on it, which is only possible if it is not a FAIL.
        let c = classify(&held(Termination::TimedOut));
        assert_eq!(c.outcome, VerificationOutcome::Inconclusive);
    }

    #[test]
    fn a_timeout_is_a_failure_only_where_the_profile_says_so() {
        let mut execution = held(Termination::TimedOut);
        execution.on_timeout = OnTimeout::Fail;
        assert_eq!(classify(&execution).outcome, VerificationOutcome::Fail);
    }

    #[test]
    fn a_check_that_could_not_be_started_is_inconclusive_not_failed() {
        // A missing toolchain is not evidence that the code is wrong. §4.5:
        // FAIL → repair, INCONCLUSIVE → infra retry then human. Sending a
        // vanished `cargo` to a repair agent is exactly the "three wasted agent
        // attempts" the four-outcome split exists to prevent.
        let c = classify(&held(Termination::SpawnFailed {
            detail: "No such file or directory (os error 2)".to_string(),
        }));
        assert_eq!(c.outcome, VerificationOutcome::Inconclusive);
    }

    #[test]
    fn an_infrastructure_failure_is_inconclusive_not_failed() {
        // The disk filling while the log is being written says nothing about
        // the code under test.
        let c = classify(&held(Termination::Infrastructure {
            detail: "No space left on device (os error 28)".to_string(),
        }));
        assert_eq!(c.outcome, VerificationOutcome::Inconclusive);
    }

    #[test]
    fn a_check_killed_from_outside_is_inconclusive() {
        // SIGKILL is not something a compiler or a test suite does to itself.
        let c = classify(&held(Termination::Signalled { signal: 9 }));
        assert_eq!(c.outcome, VerificationOutcome::Inconclusive);
    }

    #[test]
    fn a_check_whose_own_process_faulted_is_a_failure() {
        // A segfault in the code under test is a defect in the code under test,
        // and sending it to repair is the right thing to do with it.
        for signal in [libc::SIGSEGV, libc::SIGABRT, libc::SIGBUS, libc::SIGFPE] {
            assert!(signal_is_a_program_fault(signal), "signal {signal}");
            let c = classify(&held(Termination::Signalled { signal }));
            assert_eq!(
                c.outcome,
                VerificationOutcome::Fail,
                "signal {signal} should be a failure of the check"
            );
        }
        for signal in [libc::SIGKILL, libc::SIGTERM, libc::SIGINT, libc::SIGXCPU] {
            assert!(!signal_is_a_program_fault(signal), "signal {signal}");
        }
    }

    #[test]
    fn an_unknown_signal_is_inconclusive_because_unknown_is_not_a_verdict() {
        let c = classify(&held(Termination::Signalled { signal: 250 }));
        assert_eq!(c.outcome, VerificationOutcome::Inconclusive);
    }

    // ---- VOID: the invariant that matters most --------------------------

    #[test]
    fn a_tree_that_moved_voids_a_passing_check() {
        // §4.5: "a result whose tree moved under it is VOID, not PASS". This is
        // the hole being closed — a green result for a tree that never existed.
        let c = classify(&Execution {
            termination: Termination::Exited { code: 0 },
            tree: TreeWitness::Moved {
                before: tree("t1"),
                after: tree("t2"),
            },
            on_timeout: OnTimeout::Inconclusive,
        });
        assert_eq!(c.outcome, VerificationOutcome::Void);
        assert_eq!(c.findings.len(), 1, "a void result raises a finding");
        assert_eq!(c.findings[0].kind, TREE_MUTATED_DURING_CHECK);
        assert!(c.findings[0].detail.contains("t1"));
        assert!(c.findings[0].detail.contains("t2"));
    }

    #[test]
    fn a_tree_that_moved_voids_a_failing_check_too() {
        // Symmetry matters: a FAIL on a tree that moved would send a repair
        // agent after a defect that may not exist in any tree.
        for termination in [
            Termination::Exited { code: 1 },
            Termination::TimedOut,
            Termination::Signalled { signal: 9 },
            Termination::SpawnFailed {
                detail: "gone".to_string(),
            },
        ] {
            let c = classify(&Execution {
                termination: termination.clone(),
                tree: TreeWitness::Moved {
                    before: tree("t1"),
                    after: tree("t2"),
                },
                on_timeout: OnTimeout::Inconclusive,
            });
            assert_eq!(
                c.outcome,
                VerificationOutcome::Void,
                "{termination:?} on a moved tree must be VOID"
            );
        }
    }

    #[test]
    fn a_tree_that_could_not_be_hashed_is_void_not_pass() {
        // Fails safe. "We could not tell which tree this observed" and "this
        // observed the tree we think" are not the same statement.
        let c = classify(&Execution {
            termination: Termination::Exited { code: 0 },
            tree: TreeWitness::Unknown {
                detail: "workspace disappeared".to_string(),
            },
            on_timeout: OnTimeout::Inconclusive,
        });
        assert_eq!(c.outcome, VerificationOutcome::Void);
        assert_eq!(c.findings[0].kind, TREE_UNOBSERVABLE);
    }

    // ---- flaky retry -----------------------------------------------------

    #[test]
    fn two_runs_that_agree_keep_their_answer() {
        let fail = classify(&held(Termination::Exited { code: 1 }));
        let combined = combine_flaky(&[fail.clone(), fail]);
        assert_eq!(combined.outcome, VerificationOutcome::Fail);
    }

    #[test]
    fn two_runs_that_disagree_are_inconclusive_and_the_good_one_does_not_win() {
        // §4.5: "exactly one; disagreement ⇒ INCONCLUSIVE". Letting the pass
        // win would make a flaky test a silent green; letting the fail win
        // would send a repair agent after a race.
        let pass = classify(&held(Termination::Exited { code: 0 }));
        let fail = classify(&held(Termination::Exited { code: 1 }));

        let combined = combine_flaky(&[fail.clone(), pass.clone()]);
        assert_eq!(combined.outcome, VerificationOutcome::Inconclusive);
        assert!(
            combined
                .findings
                .iter()
                .any(|f| f.kind == FLAKY_CHECK_DISAGREEMENT),
            "a disagreement is worth a human's attention"
        );

        // And in the other order, so that "the last one wins" is excluded too.
        assert_eq!(
            combine_flaky(&[pass, fail]).outcome,
            VerificationOutcome::Inconclusive
        );
    }

    #[test]
    fn a_void_run_voids_the_pair_whatever_the_other_run_said() {
        let void = Classified {
            outcome: VerificationOutcome::Void,
            findings: vec![VerificationFinding {
                kind: TREE_MUTATED_DURING_CHECK,
                detail: "moved".to_string(),
            }],
        };
        let pass = classify(&held(Termination::Exited { code: 0 }));
        assert_eq!(
            combine_flaky(&[void.clone(), pass.clone()]).outcome,
            VerificationOutcome::Void
        );
        assert_eq!(
            combine_flaky(&[pass, void]).outcome,
            VerificationOutcome::Void
        );
    }

    #[test]
    fn a_single_run_is_its_own_answer() {
        let pass = classify(&held(Termination::Exited { code: 0 }));
        assert_eq!(combine_flaky(&[pass]).outcome, VerificationOutcome::Pass);
    }

    #[test]
    fn combining_carries_every_finding_forward() {
        let void = Classified {
            outcome: VerificationOutcome::Void,
            findings: vec![VerificationFinding {
                kind: TREE_MUTATED_DURING_CHECK,
                detail: "moved".to_string(),
            }],
        };
        let combined = combine_flaky(&[void.clone(), void]);
        assert_eq!(
            combined.findings.len(),
            2,
            "findings never auto-resolve, and that includes being merged away"
        );
    }
}
