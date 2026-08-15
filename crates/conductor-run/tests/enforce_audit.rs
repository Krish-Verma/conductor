//! S9 — the two §4.8 surfaces that nothing was watching.
//!
//! §4.8's reconciled surface is captured on every exit from `RUNNING`, and S2
//! built almost all of it: status, diffs, untracked files, commits, local
//! config, remotes, refs, reflog, stash, hooks, submodules, nested repos and
//! scope. Two items were listed and never implemented — *"secret-pattern scan
//! over the whole diff"* and *"`/tmp` delta during the attempt window"* — and
//! they are the two that §4.9 leans on hardest, because layer 8 is the only
//! layer that sees an agent staging a credential for later pickup (M7,
//! limitation 2).
//!
//! Every test here obeys the rule S8 paid for and S9's env tests inherited: **a
//! security test must prove both that the intended detection fires and that a
//! clean input produces nothing.** A one-sided test passes under an
//! implementation that flags everything and under one that flags nothing, and
//! this repository has shipped both.
//!
//! # Canaries, never real secrets
//!
//! Every "secret" below is a synthetic string this file invented, structurally
//! valid and deliberately dead. `AKIAIOSFODNN7EXAMPLE` is AWS's own published
//! documentation value. Nothing here reads an operator credential, and nothing
//! needs to: proving that a *shape* is detected and that the *value* never
//! reaches a finding does not require a live key.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use conductor_run::enforce::audit::{
    AuditFinding, KIND_RUN_TMPDIR_RESIDUE, KIND_SECRET_IN_DIFF, KIND_SECRET_IN_DIFF_CONTEXT,
    KIND_SECRET_IN_RUN_TMPDIR, KIND_SYSTEM_TMP_DELTA, KIND_TEMP_AUDIT_INCOMPLETE,
    SEVERITY_CRITICAL, SEVERITY_INFO, SEVERITY_WARNING, audit_diff_for_secrets, audit_temp_delta,
    watch_temp,
};

/// AWS's own published example key id. Structurally valid, never live.
const CANARY_AWS: &str = "AKIAIOSFODNN7EXAMPLE";
/// A GitHub personal-access-token shape. Invented here.
const CANARY_GITHUB: &str = "ghp_1234567890abcdefghijklmnopqrstuvwxyzAB";

/// Every rendering a finding can reach a database, a packet or a log through.
///
/// The rule is not "the detail is redacted" — it is that the *value* is absent
/// from anything serialisable, because a finding is copied into
/// `finding.evidence_ref` and from there into every report generated from it.
fn renderings(finding: &AuditFinding) -> Vec<String> {
    vec![
        finding.detail().to_string(),
        format!("{finding:?}"),
        serde_json::to_string(finding).expect("a finding must serialise"),
    ]
}

fn assert_never_echoes(findings: &[AuditFinding], canary: &str) {
    for finding in findings {
        for rendering in renderings(finding) {
            assert!(
                !rendering.contains(canary),
                "the secret value reached a finding: {rendering}"
            );
        }
    }
}

fn kinds(findings: &[AuditFinding]) -> BTreeSet<&str> {
    findings.iter().map(|f| f.kind()).collect()
}

// ---------------------------------------------------------------------------
// §4.8 — "secret-pattern scan over the whole diff"
// ---------------------------------------------------------------------------

#[test]
fn a_planted_key_in_the_diff_is_found_and_never_echoed() {
    let diff = [
        "diff --git a/src/config.rs b/src/config.rs",
        "index 1111111..2222222 100644",
        "--- a/src/config.rs",
        "+++ b/src/config.rs",
        "@@ -1,2 +1,3 @@",
        " fn main() {",
        &format!("+    let key = \"{CANARY_AWS}\";"),
        " }",
    ]
    .join("\n");

    let findings = audit_diff_for_secrets(&diff);

    assert_eq!(
        findings.len(),
        1,
        "expected exactly one finding, got {findings:#?}"
    );
    let finding = &findings[0];
    assert_eq!(finding.kind(), KIND_SECRET_IN_DIFF);
    assert_eq!(
        finding.severity(),
        SEVERITY_CRITICAL,
        "a shaped credential added by the attempt must block the completion gate"
    );
    assert!(
        finding.detail().contains("aws-access-key-id"),
        "the finding must name the kind: {}",
        finding.detail()
    );
    assert!(
        finding.detail().contains("src/config.rs"),
        "the finding must name the file: {}",
        finding.detail()
    );
    assert!(
        finding.detail().contains("src/config.rs:2"),
        "the finding must name the line in the *new* file, not the offset in the \
         diff text — a reviewer opens the file, not the patch: {}",
        finding.detail()
    );

    assert_never_echoes(&findings, CANARY_AWS);
}

#[test]
fn an_ordinary_code_diff_raises_nothing() {
    // The other half of the pair. Without this, an implementation that flags
    // every added line passes the test above and blocks every run on day one.
    let diff = [
        "diff --git a/src/session.rs b/src/session.rs",
        "index aaaaaaa..bbbbbbb 100644",
        "--- a/src/session.rs",
        "+++ b/src/session.rs",
        "@@ -10,6 +10,14 @@ impl Session {",
        "     pub fn new(cfg: &Config) -> Result<Self, Error> {",
        "+        let token = self.parse_token(input)?;",
        "+        let credentials = Credentials::from_env()?;",
        "+        tracing::debug!(\"api_key present: {}\", cfg.api_key.is_some());",
        "+        assert_eq!(auth_token, expected);",
        "+        // TODO: rotate the secret_key monthly, see docs/security.md",
        "+        let password: Option<String> = None;",
        "+        return Ok(Self { token, credentials });",
        "     }",
        "diff --git a/package-lock.json b/package-lock.json",
        "--- a/package-lock.json",
        "+++ b/package-lock.json",
        "@@ -1,3 +1,5 @@",
        "+    \"resolved\": \"https://registry.npmjs.org/@babel/core/-/core-7.24.0.tgz\",",
        "+    \"integrity\": \"sha512-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH==\",",
        "+    \"version\": \"7.24.0\",",
    ]
    .join("\n");

    let findings = audit_diff_for_secrets(&diff);
    assert!(
        findings.is_empty(),
        "ordinary code was flagged as a secret: {findings:#?}"
    );
}

#[test]
fn a_secret_on_a_context_line_is_reported_but_not_blamed_on_this_attempt() {
    // A credential that was already in the file appears in the diff as context.
    // Reporting it is right — §4.8 scans the whole diff. Blaming *this* attempt
    // for it, and blocking the run on it, is a false accusation.
    let diff = [
        "diff --git a/src/config.rs b/src/config.rs",
        "--- a/src/config.rs",
        "+++ b/src/config.rs",
        "@@ -1,3 +1,3 @@",
        &format!(" const KEY: &str = \"{CANARY_AWS}\";"),
        "-let timeout = 30;",
        "+let timeout = 60;",
    ]
    .join("\n");

    let findings = audit_diff_for_secrets(&diff);

    assert_eq!(
        kinds(&findings),
        BTreeSet::from([KIND_SECRET_IN_DIFF_CONTEXT])
    );
    let finding = &findings[0];
    assert_eq!(
        finding.severity(),
        SEVERITY_WARNING,
        "a pre-existing secret must not block the run on this attempt's behalf"
    );
    assert!(
        finding.detail().contains("not introduced by this attempt"),
        "the finding must say what it cannot prove: {}",
        finding.detail()
    );
    assert_never_echoes(&findings, CANARY_AWS);
}

#[test]
fn a_secret_the_attempt_removed_is_not_charged_to_the_attempt() {
    let diff = [
        "--- a/src/config.rs",
        "+++ b/src/config.rs",
        "@@ -1,3 +1,2 @@",
        &format!("-const KEY: &str = \"{CANARY_GITHUB}\";"),
        "+const KEY: &str = std::env::var(\"GH\").unwrap();",
    ]
    .join("\n");

    let findings = audit_diff_for_secrets(&diff);
    assert_eq!(
        kinds(&findings),
        BTreeSet::from([KIND_SECRET_IN_DIFF_CONTEXT])
    );
    assert_never_echoes(&findings, CANARY_GITHUB);
}

#[test]
fn a_hardcoded_assignment_is_reported_below_the_blocking_severity() {
    // The scanner's `assigned-secret` rule is name-based and therefore the one
    // that can be wrong. It is reported — silently dropping it would be worse —
    // but it does not block a run on a heuristic.
    let diff = [
        "--- a/deploy/env",
        "+++ b/deploy/env",
        "@@ -1 +1,2 @@",
        "+DATABASE_PASSWORD=s3cr3t-p4ssw0rd-value",
    ]
    .join("\n");

    let findings = audit_diff_for_secrets(&diff);
    assert_eq!(kinds(&findings), BTreeSet::from([KIND_SECRET_IN_DIFF]));
    assert_eq!(
        findings[0].severity(),
        SEVERITY_WARNING,
        "a name-based heuristic must not be a blocking finding"
    );
    assert_never_echoes(&findings, "s3cr3t-p4ssw0rd-value");
}

#[test]
fn text_that_is_not_a_unified_diff_still_gets_scanned() {
    // `git diff` output is what the caller passes, but a caller that passes
    // something else must not silently get a clean audit.
    let findings = audit_diff_for_secrets(&format!("some blob of text with {CANARY_GITHUB} in it"));
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert!(
        findings[0].detail().contains("github-token"),
        "{}",
        findings[0].detail()
    );
    assert_never_echoes(&findings, CANARY_GITHUB);
}

#[test]
fn an_empty_diff_is_clean() {
    assert!(audit_diff_for_secrets("").is_empty());
}

#[test]
fn a_long_line_is_redacted_before_it_is_truncated_not_after() {
    // Found by mutation testing, not by reading. The quoted line is capped, and
    // the order of "redact" and "truncate" decides whether the cap is a safety
    // feature or a leak: truncating first can cut a secret in half, and half an
    // AWS key id is 10 characters that no length-based rule recognises any
    // more, so the second redaction pass waves it through. The finding then
    // carries a fragment of the value it exists to report.
    // 149 filler characters and a space, so the key starts at character 150 and
    // is its own whitespace-delimited word — the scanner is word-oriented, and a
    // key glued to filler is a different (documented) blind spot.
    let padding = format!("{} ", "x".repeat(149));
    let diff = [
        "--- a/src/lib.rs",
        "+++ b/src/lib.rs",
        "@@ -1 +1 @@",
        &format!("+{padding}{CANARY_AWS}"),
    ]
    .join("\n");

    let findings = audit_diff_for_secrets(&diff);
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert_never_echoes(&findings, CANARY_AWS);
    for rendering in renderings(&findings[0]) {
        assert!(
            !rendering.contains(&CANARY_AWS[..10]),
            "a truncated fragment of the secret reached the finding: {rendering}"
        );
    }
}

// ---------------------------------------------------------------------------
// §4.8 — "`/tmp` delta during the attempt window"
// ---------------------------------------------------------------------------

fn now() -> SystemTime {
    SystemTime::now()
}

#[test]
fn an_untouched_pair_of_temp_directories_produces_no_findings() {
    let system_tmp = tempfile::tempdir().unwrap();
    let run_tmp = tempfile::tempdir().unwrap();
    fs::write(system_tmp.path().join("pre-existing.lock"), b"x").unwrap();

    let watch = watch_temp(system_tmp.path(), run_tmp.path(), now());
    let findings = audit_temp_delta(&watch);

    assert!(
        findings.is_empty(),
        "a quiet attempt window produced findings: {findings:#?}"
    );
}

#[test]
fn what_the_agent_left_in_its_own_tmpdir_is_inventoried() {
    let system_tmp = tempfile::tempdir().unwrap();
    let run_tmp = tempfile::tempdir().unwrap();

    let watch = watch_temp(system_tmp.path(), run_tmp.path(), now());
    fs::create_dir_all(run_tmp.path().join("nested")).unwrap();
    fs::write(run_tmp.path().join("nested/scratch.txt"), b"ordinary\n").unwrap();

    let findings = audit_temp_delta(&watch);

    assert_eq!(kinds(&findings), BTreeSet::from([KIND_RUN_TMPDIR_RESIDUE]));
    let finding = &findings[0];
    assert_eq!(
        finding.severity(),
        SEVERITY_INFO,
        "a temp directory containing temp files is expected, not alarming"
    );
    assert!(
        finding.detail().contains("nested/scratch.txt"),
        "the inventory must name what it found: {}",
        finding.detail()
    );
}

#[test]
fn a_secret_staged_in_the_run_tmpdir_is_found_and_never_echoed() {
    // §4.9 limitation 2: staging is possible and is *detected*, not prevented.
    // The per-run TMPDIR is the surface where that detection is attributable.
    let system_tmp = tempfile::tempdir().unwrap();
    let run_tmp = tempfile::tempdir().unwrap();

    let watch = watch_temp(system_tmp.path(), run_tmp.path(), now());
    fs::write(
        run_tmp.path().join("stash.env"),
        format!("AWS_KEY={CANARY_AWS}\n"),
    )
    .unwrap();

    let findings = audit_temp_delta(&watch);

    assert!(
        kinds(&findings).contains(KIND_SECRET_IN_RUN_TMPDIR),
        "staged secret not detected: {findings:#?}"
    );
    let secret = findings
        .iter()
        .find(|f| f.kind() == KIND_SECRET_IN_RUN_TMPDIR)
        .unwrap();
    assert_eq!(secret.severity(), SEVERITY_CRITICAL);
    assert!(secret.detail().contains("stash.env"), "{}", secret.detail());
    assert_never_echoes(&findings, CANARY_AWS);
}

#[test]
fn an_ordinary_file_in_the_run_tmpdir_is_not_called_a_secret() {
    let system_tmp = tempfile::tempdir().unwrap();
    let run_tmp = tempfile::tempdir().unwrap();

    let watch = watch_temp(system_tmp.path(), run_tmp.path(), now());
    fs::write(
        run_tmp.path().join("rustc-build.log"),
        "   Compiling conductor-run v0.1.0\ntest result: ok. 27 passed; 0 failed\n",
    )
    .unwrap();

    let findings = audit_temp_delta(&watch);
    assert!(
        !kinds(&findings).contains(KIND_SECRET_IN_RUN_TMPDIR),
        "a build log was called a secret: {findings:#?}"
    );
}

#[test]
fn a_new_entry_in_system_tmp_is_reported_without_being_blamed_on_the_agent() {
    let system_tmp = tempfile::tempdir().unwrap();
    let run_tmp = tempfile::tempdir().unwrap();

    let watch = watch_temp(system_tmp.path(), run_tmp.path(), now());
    fs::write(system_tmp.path().join("appeared-during-window"), b"x").unwrap();

    let findings = audit_temp_delta(&watch);

    assert_eq!(kinds(&findings), BTreeSet::from([KIND_SYSTEM_TMP_DELTA]));
    let finding = &findings[0];
    assert_ne!(
        finding.severity(),
        SEVERITY_CRITICAL,
        "a multi-process /tmp must never block a run"
    );
    assert!(
        finding.detail().contains("appeared-during-window"),
        "{}",
        finding.detail()
    );
    assert!(
        finding.detail().contains("not proof"),
        "the finding must state what it cannot prove: {}",
        finding.detail()
    );
}

#[test]
fn a_system_tmp_entry_the_window_cannot_account_for_drops_to_the_lowest_severity() {
    // Window start in the future: nothing observed afterwards can have been
    // created inside the window, so the evidence does not connect the entry to
    // the run. It is still reported — dropping it silently is the failure mode
    // §4.8 forbids — but at the severity that says so.
    let system_tmp = tempfile::tempdir().unwrap();
    let run_tmp = tempfile::tempdir().unwrap();

    let future = now() + Duration::from_secs(3600);
    let watch = watch_temp(system_tmp.path(), run_tmp.path(), future);
    fs::write(system_tmp.path().join("unattributable"), b"x").unwrap();

    let findings = audit_temp_delta(&watch);
    let delta = findings
        .iter()
        .find(|f| f.kind() == KIND_SYSTEM_TMP_DELTA)
        .expect("the entry must still be reported");
    assert_eq!(delta.severity(), SEVERITY_INFO);
    assert!(
        delta.detail().contains("does not connect"),
        "{}",
        delta.detail()
    );
}

#[test]
fn a_snapshot_that_could_not_be_taken_fails_closed_rather_than_clean() {
    // The dangerous outcome is not "the audit found nothing" — it is "the audit
    // could not look and said nothing". §4.8's contract is that an unexplained
    // delta raises a finding; a delta that could not be computed is the most
    // unexplained of all.
    let run_tmp = tempfile::tempdir().unwrap();
    let missing = Path::new("/nonexistent/conductor-audit-system-tmp");

    let watch = watch_temp(missing, run_tmp.path(), now());
    let findings = audit_temp_delta(&watch);

    assert!(
        kinds(&findings).contains(KIND_TEMP_AUDIT_INCOMPLETE),
        "an unreadable snapshot produced a clean audit: {findings:#?}"
    );
}

#[test]
fn a_missing_before_snapshot_does_not_switch_off_the_secret_scan() {
    // Failing to compute a *delta* is not a reason to stop looking at what is
    // actually sitting there. "The audit could not tell when this appeared" and
    // "the audit did not notice an AWS key in the workspace" are different
    // failures, and only the first one is acceptable.
    let system_tmp = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let run_tmp = parent.path().join("not-created-yet");

    let watch = watch_temp(system_tmp.path(), &run_tmp, now());
    fs::create_dir_all(&run_tmp).unwrap();
    fs::write(run_tmp.join("stash.env"), format!("AWS_KEY={CANARY_AWS}\n")).unwrap();

    let findings = audit_temp_delta(&watch);
    let found = kinds(&findings);
    assert!(
        found.contains(KIND_TEMP_AUDIT_INCOMPLETE),
        "the uncomputable delta must be reported: {findings:#?}"
    );
    assert!(
        found.contains(KIND_SECRET_IN_RUN_TMPDIR),
        "the staged secret must still be found: {findings:#?}"
    );
    assert_never_echoes(&findings, CANARY_AWS);
}

#[test]
fn a_key_glued_to_its_variable_name_is_still_found() {
    // Regression guard. `verify::secrets` splits on whitespace, which is right
    // for a log line and wrong for `.env` syntax: `AWS_KEY=AKIA…` scans clean,
    // because the key id is not its own word and `AWS_KEY` is not on the
    // sensitive-name list. That is the exact shape §4.9 limitation 2 predicts.
    let diff = [
        "--- a/.env",
        "+++ b/.env",
        "@@ -0,0 +1 @@",
        &format!("+AWS_KEY={CANARY_AWS}"),
    ]
    .join("\n");

    let findings = audit_diff_for_secrets(&diff);
    assert_eq!(kinds(&findings), BTreeSet::from([KIND_SECRET_IN_DIFF]));
    assert_eq!(findings[0].severity(), SEVERITY_CRITICAL);
    assert!(findings[0].detail().contains("aws-access-key-id"));
    assert_never_echoes(&findings, CANARY_AWS);
}

#[test]
fn a_private_key_block_never_drags_its_body_into_a_finding() {
    // The scanner matches a private key on its *header* alone, so the base64
    // body is not redacted by anything. A finding that quoted a window of
    // surrounding lines instead of the matching line would copy the key itself
    // into `finding.evidence_ref` and into every packet built from it.
    const BODY: &str = "MIIEowIBAAKCAQEAcanaryBODYbase64abcdefghijklmnop0123456789";
    let diff = [
        "--- /dev/null",
        "+++ b/id_rsa",
        "@@ -0,0 +1,3 @@",
        "+-----BEGIN RSA PRIVATE KEY-----",
        &format!("+{BODY}"),
        "+-----END RSA PRIVATE KEY-----",
    ]
    .join("\n");

    let findings = audit_diff_for_secrets(&diff);
    assert!(
        findings.iter().any(|f| f.detail().contains("private-key")),
        "a private key block was not detected: {findings:#?}"
    );
    assert_never_echoes(&findings, BODY);
}

#[cfg(unix)]
#[test]
fn a_symlink_planted_in_the_run_tmpdir_is_listed_but_never_read_through() {
    // An audit that follows a link out of the directory it is auditing performs
    // the exfiltration it exists to detect: the target's contents would be read
    // and then written into `finding.evidence_ref`.
    let system_tmp = tempfile::tempdir().unwrap();
    let run_tmp = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let target = elsewhere.path().join("credentials");
    fs::write(&target, format!("aws_access_key_id = {CANARY_AWS}\n")).unwrap();

    let watch = watch_temp(system_tmp.path(), run_tmp.path(), now());
    std::os::unix::fs::symlink(&target, run_tmp.path().join("link")).unwrap();

    let findings = audit_temp_delta(&watch);
    assert!(
        !kinds(&findings).contains(KIND_SECRET_IN_RUN_TMPDIR),
        "the audit read through a symlink: {findings:#?}"
    );
    assert!(
        findings.iter().any(|f| f.detail().contains("link")),
        "the link itself must still be inventoried: {findings:#?}"
    );
    assert_never_echoes(&findings, CANARY_AWS);
}

#[test]
fn findings_are_deterministic_across_two_identical_audits() {
    // The worker derives a finding id from a hash of the evidence, so unstable
    // ordering or unstable wording would produce a fresh unresolved finding on
    // every reconciliation of the same state.
    let system_tmp = tempfile::tempdir().unwrap();
    let run_tmp = tempfile::tempdir().unwrap();
    let watch = watch_temp(system_tmp.path(), run_tmp.path(), now());
    fs::write(run_tmp.path().join("b.txt"), b"b").unwrap();
    fs::write(run_tmp.path().join("a.txt"), b"a").unwrap();

    let first = audit_temp_delta(&watch);
    let second = audit_temp_delta(&watch);
    assert_eq!(first, second);
    assert!(first[0].detail().contains("a.txt"));
}
