//! §6.5's agent report, and the five review inputs that were deferred until
//! their consumer existed.
//!
//! # Why these fields arrive at S13 and not earlier
//!
//! §6.5's report table marks `commands_run`, `acceptance_criteria`, `deviations`,
//! `blockers` and `unverified_claims` **not in v1**, with a reason:
//!
//! > Every one of them is a *review* input, and their only consumer is §6.5's
//! > review packet, which **S13 owns**. Asking an agent to produce a field
//! > nothing reads is exactly the no-op knob CLAUDE.md forbids, so they are
//! > deferred to the slice that would consume them rather than shipped as
//! > decoration.
//!
//! S13 is that slice. The review packet reads all five, so they stop being
//! decoration and are added — which is the deferral being *discharged* rather
//! than quietly forgotten, and is why these tests name the fields one by one.
//!
//! # Still `agent-report.v1`, deliberately
//!
//! Every added field is `#[serde(default)]`, so a report written by an agent that
//! has never heard of them still parses, and a report carrying them still parses
//! on a build that ignores them. That is a compatible extension, not a new
//! schema, and `$id` therefore stays `agent-report.v1`. Bumping it would force
//! every adapter to be told about a change that cannot break any of them.

use conductor_core::{AgentReport, ReportClaim};

#[test]
fn a_report_from_before_these_fields_existed_still_parses() {
    // The compatibility claim, as a test rather than a comment. This is the exact
    // shape S3's fake agent and S10's Codex adapter have been writing.
    let report: AgentReport = serde_json::from_str(
        r#"{"claim":"COMPLETE","files_touched":["src/added.rs"],"summary":"added a function"}"#,
    )
    .expect("a pre-S13 report must still parse");

    assert_eq!(report.claim, ReportClaim::Complete);
    assert_eq!(report.files_touched, vec!["src/added.rs".to_string()]);
    assert!(report.deviations.is_empty());
    assert!(report.blockers.is_empty());
    assert!(report.unverified_claims.is_empty());
    assert!(report.commands_run.is_empty());
    assert!(report.acceptance_criteria.is_empty());
    assert_eq!(report.task_id, None);
}

#[test]
fn every_review_input_section_6_5_names_round_trips() {
    let json = r#"{
      "claim": "PARTIAL",
      "files_touched": ["src/lib.rs"],
      "summary": "got halfway",
      "task_id": "T-0012",
      "commands_run": ["cargo test", "cargo fmt"],
      "acceptance_criteria": ["AC-1"],
      "deviations": ["renamed the helper the plan called `greet`"],
      "blockers": ["the fixture needs a network mock"],
      "unverified_claims": ["the new path is faster"]
    }"#;
    let report: AgentReport = serde_json::from_str(json).expect("parse");

    assert_eq!(report.claim, ReportClaim::Partial);
    assert_eq!(report.task_id.as_deref(), Some("T-0012"));
    assert_eq!(report.commands_run, vec!["cargo test", "cargo fmt"]);
    assert_eq!(report.acceptance_criteria, vec!["AC-1"]);
    assert_eq!(
        report.deviations,
        vec!["renamed the helper the plan called `greet`"]
    );
    assert_eq!(report.blockers, vec!["the fixture needs a network mock"]);
    assert_eq!(report.unverified_claims, vec!["the new path is faster"]);

    // Round trip: the review packet is built from this value, so a field that
    // parsed and then vanished on the way out would reach a human as an empty
    // list — the worst shape, because an empty `deviations` reads as "the agent
    // deviated from nothing".
    let again: AgentReport =
        serde_json::from_str(&serde_json::to_string(&report).expect("serialize")).expect("reparse");
    assert_eq!(again, report);
}

#[test]
fn an_unknown_field_does_not_refuse_the_report() {
    // §4.8: the report is evidence, not authority, and §2.2 forbids
    // `deny_unknown_fields` on *"a structure an agent produced"*. An agent from a
    // later Conductor — or one that simply guessed — must not be able to make its
    // own report unparseable, because `REPORT_UNPARSEABLE` is acceptance row 5's
    // finding and it should mean malformed, not merely unfamiliar.
    let report: AgentReport =
        serde_json::from_str(r#"{"claim":"FAILED","confidence":0.4,"next_steps":["ask a human"]}"#)
            .expect("an unknown field must not refuse the report");
    assert_eq!(report.claim, ReportClaim::Failed);
}

#[test]
fn a_claim_conductor_does_not_know_is_still_refused() {
    // The other side of the previous test, so "tolerant of unknown fields" is not
    // read as "tolerant of anything". `claim` is the field reconciliation
    // compares against the repository, and an unrecognised claim must not become
    // one of the three — least of all `COMPLETE`.
    for bad in [
        r#"{"claim":"DONE"}"#,
        r#"{"claim":"complete"}"#,
        r#"{"claim":""}"#,
        r#"{"files_touched":[]}"#,
    ] {
        assert!(
            serde_json::from_str::<AgentReport>(bad).is_err(),
            "{bad} must not parse"
        );
    }
}
