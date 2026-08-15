//! §4.9's layer 8 — the post-run audit, which prevents nothing and sees almost
//! everything.
//!
//! §4.8 lists the reconciled surface captured after every attempt. S2 built
//! nearly all of it in `conductor_git::{baseline, reconcile}`: status, staged
//! and unstaged diffs, untracked files, new commits, local config, remotes,
//! refs, reflog, stash, hooks, submodules, nested repositories and scope. Two
//! items on that list were specified and never implemented, and this module is
//! exactly those two:
//!
//! 1. **"secret-pattern scan over the whole diff"** — [`audit_diff_for_secrets`].
//! 2. **"`/tmp` delta during the attempt window"** — [`watch_temp`] and
//!    [`audit_temp_delta`].
//!
//! plus the standing rule that governs both: *"Any unexplained delta raises a
//! `Finding`. **Findings never auto-resolve.**"* Nothing here writes to the
//! database. The caller owns the fence and calls `store.record_finding`; this
//! module returns evidence, which is the only thing it is competent to produce.
//!
//! # Why there is no second scanner here
//!
//! The master plan lists `enforce/secrets.rs`. S4 already built the scanner at
//! [`crate::verify::secrets`], including its published
//! [`NOT_DETECTED`](crate::verify::secrets::NOT_DETECTED) list. A second
//! detector would be a second answer to "is this text safe to show", the two
//! would drift, and the honest blind-spot list would end up describing only one
//! of them. This module calls the existing one and inherits its blind spots
//! wholesale — see "What this audit cannot prove" below.
//!
//! # A finding never carries the secret it found
//!
//! A finding is copied into `finding.evidence_ref` and from there into every
//! packet, report and log line generated from that row. A finding that quotes
//! the value has not reported the leak, it has *widened* it. So
//! [`AuditFinding`]'s only constructor runs
//! [`secrets::redact`] over the finished detail
//! string, and the field is private so that there is no way to assemble one that
//! skips it. This is deliberately structural rather than a rule to remember:
//! `enforce::env` records the same lesson about `RunEnvironment`, and the S5 bug
//! it describes was an API that let a caller *have* the thing without the
//! guarantee.
//!
//! # Two temp surfaces, and they are not equally trustworthy
//!
//! §4.9's mitigation for M7 (`/tmp` stays writable under Codex) is a **per-run
//! `TMPDIR` inside the workspace**, created by
//! [`enforce::env::prepare`](super::env::prepare) and hidden from `git status`
//! via `.git/info/exclude` — which is precisely why something other than git has
//! to look at it.
//!
//! | Surface | Attribution | Contents read | Severity ceiling |
//! |---|---|---|---|
//! | per-run `TMPDIR` | strong: Conductor creates it empty, and only this run's children are given it | yes, bounded | `CRITICAL` |
//! | system `/tmp` | weak: a shared, multi-process directory | **no**, names only | `WARNING` |
//!
//! Treating them the same would produce either a useless audit or a false
//! accusation. A multi-process operating system writes to `/tmp` constantly; a
//! rule of "system `/tmp` changed, therefore the agent did it" is wrong on a
//! quiet machine and unusable on a busy one. So system-`/tmp` deltas are
//! reported with the evidence that exists (did an entry appear during the
//! window; is it owned by this uid) and with wording that says what that
//! evidence does not establish. They are never blocking.
//!
//! # What this audit cannot prove
//!
//! Written down here because these sentences belong in `SECURITY.md`, and a
//! claim of "we audit `/tmp`" that is not qualified becomes an assumption.
//!
//! 1. **Same uid is not same process.** Conductor itself, the test harness, the
//!    shell, cargo and every other process running as this user share the
//!    system temp directory. Ownership narrows the field to "this account"; it
//!    never narrows it to "the agent".
//! 2. **Timestamps are not testimony.** Creation time is unavailable on some
//!    filesystems (the audit then falls back to mtime), and mtime is settable by
//!    the process that owns the file. An agent can move its own artefacts out of
//!    the window.
//! 3. **Absence of a delta is not absence of activity.** A file created and
//!    deleted inside the window leaves nothing behind; a write into a file that
//!    already existed changes no name; and the system-temp scan is top-level
//!    only, so a new file inside a pre-existing subdirectory is invisible.
//! 4. **`/tmp` is not the only residual path.** macOS's per-user
//!    `/var/folders/...`, `/dev/shm`, and any other world-writable directory are
//!    not watched. §4.9 limitation 1 already says detection-only under a
//!    non-sandboxed launcher; this is the shape of that limit.
//! 5. **Nothing is known about the content of a system-temp entry.** Conductor
//!    records the name and refuses to read the file. Reading other processes'
//!    temp files to audit an agent would be a worse privacy posture than the
//!    thing being audited.
//! 6. **Even in the per-run `TMPDIR`, attribution stops at the run.** It does
//!    not identify which child process wrote a file, and it does not establish
//!    that the agent had no other temp path available.
//! 7. **Secret detection is shape-based.** Every entry in
//!    [`NOT_DETECTED`](crate::verify::secrets::NOT_DETECTED) applies unchanged:
//!    encoded, split, or unprefixed high-entropy secrets are not seen. A clean
//!    audit is not evidence that no secret was written.
//! 8. **File reads are bounded.** Only the first [`MAX_SCAN_BYTES_PER_FILE`]
//!    bytes of each temp file are scanned, decoded lossily as UTF-8; content
//!    past that cap and content inside binary formats are not examined.
//! 9. **The diff scan sees only what git produced.** A file written and reverted
//!    inside the attempt, or written outside the working tree, is not in the
//!    diff and is not scanned here.
//! 10. **A file in the per-run `TMPDIR` is judged "changed" by name, length and
//!     mtime.** An in-place overwrite that preserves all three — same size,
//!     within one mtime tick — is not seen as a delta. Content hashing would
//!     close this and is not worth its cost at S9; it is written down rather
//!     than left to be discovered.
//! 11. **A credential glued to filler with no whitespace is still invisible.**
//!     `scan_merged` recovers `NAME=AKIA…` by blanking value separators, but
//!     the underlying scanner is word-oriented, so `…xxxAKIA…` inside one
//!     unbroken token matches nothing. This is a narrower version of
//!     `NOT_DETECTED`'s first entry, and the same fix (entropy analysis) is
//!     S9-out-of-scope for the same reason.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

use crate::verify::secrets::{self, SecretKind, SecretMatch};

/// Blocking severity: §4.6's completion gate requires zero unresolved findings
/// at this level. Spelled here rather than imported so that this module's
/// vocabulary is readable on its own; it is the same string the vertical uses.
pub const SEVERITY_CRITICAL: &str = "CRITICAL";
/// Reaches a human at review, does not block the gate.
pub const SEVERITY_WARNING: &str = "WARNING";
/// Recorded so that it is not silently dropped. Nothing more is claimed.
pub const SEVERITY_INFO: &str = "INFO";

/// A credential shape on a line **this attempt added**.
pub const KIND_SECRET_IN_DIFF: &str = "SECRET_IN_DIFF";
/// A credential shape elsewhere in the diff — context, a removed line, or a
/// header. Present in the diff, not introduced by this attempt.
pub const KIND_SECRET_IN_DIFF_CONTEXT: &str = "SECRET_IN_DIFF_CONTEXT";
/// A credential shape inside the per-run `TMPDIR` (§4.9's M7 mitigation).
pub const KIND_SECRET_IN_RUN_TMPDIR: &str = "SECRET_IN_RUN_TMPDIR";
/// An inventory of what the attempt left in the per-run `TMPDIR`.
pub const KIND_RUN_TMPDIR_RESIDUE: &str = "RUN_TMPDIR_RESIDUE";
/// Names that appeared in the system temp directory during the attempt window.
pub const KIND_SYSTEM_TMP_DELTA: &str = "SYSTEM_TMP_DELTA";
/// The audit could not read one of the directories it was asked to compare, so
/// a delta could not be computed. Reported rather than returned as "clean".
pub const KIND_TEMP_AUDIT_INCOMPLETE: &str = "TEMP_AUDIT_INCOMPLETE";

/// How much of any single temp file is scanned.
///
/// A cap rather than a whole-file read because the per-run `TMPDIR` is where
/// build tools put multi-gigabyte intermediates, and an audit that can be made
/// to OOM by writing a large file is an audit an agent can switch off.
pub const MAX_SCAN_BYTES_PER_FILE: usize = 64 * 1024;

/// How many entries the walk of the per-run `TMPDIR` will visit.
const MAX_TMPDIR_ENTRIES: usize = 4096;
/// How deep that walk goes.
const MAX_TMPDIR_DEPTH: usize = 12;
/// How many names an aggregate finding spells out before summarising the rest.
const MAX_LISTED_ENTRIES: usize = 20;
/// How many per-match findings a single surface may produce.
const MAX_MATCH_FINDINGS: usize = 50;
/// How much of a matching line is quoted, after redaction.
const MAX_CONTEXT_CHARS: usize = 160;

/// One unexplained delta, ready for `store.record_finding`.
///
/// `detail` is redacted at construction and the field is private: there is no
/// way to build an `AuditFinding` whose evidence still contains a value the
/// scanner recognises. See the module docs for why that is structural.
///
/// `Ord` is derived so that a caller can sort a mixed set deterministically; the
/// worker derives a finding id from a hash of the evidence, and unstable wording
/// or ordering would raise a fresh never-resolving finding on every
/// reconciliation of an unchanged state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct AuditFinding {
    kind: &'static str,
    severity: &'static str,
    detail: String,
}

impl AuditFinding {
    /// Build a finding, redacting the evidence.
    ///
    /// The redaction pass runs over the *finished* string rather than over the
    /// pieces, so a future caller who forgets to redact an excerpt still cannot
    /// produce a finding that carries a recognised secret.
    fn new(kind: &'static str, severity: &'static str, detail: impl Into<String>) -> AuditFinding {
        let detail = detail.into();
        AuditFinding {
            kind,
            severity,
            detail: redact_spans(&detail, &scan_merged(&detail)),
        }
    }

    /// The finding kind, as stored in `finding.kind`.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The finding severity, as stored in `finding.severity`.
    pub fn severity(&self) -> &'static str {
        self.severity
    }

    /// The redacted evidence, as stored in `finding.evidence_ref`.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

// ---------------------------------------------------------------------------
// §4.8 — "secret-pattern scan over the whole diff"
// ---------------------------------------------------------------------------

/// Which side of the diff a line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// A `+` line: this attempt put it there.
    Added,
    /// Context, a removed line, or a header: it was already true.
    Existing,
}

/// How much a match is worth.
///
/// The scanner's prefix-shaped rules (`AKIA…`, `ghp_…`, a private-key header, a
/// JWT) are close to unambiguous. Its `assigned-secret` rule is name-based —
/// "the identifier is called `token`, so the value is a secret" — and that rule
/// is the one that fires on ordinary source code. Both are reported; only the
/// first is allowed to block a run, because blocking on a heuristic teaches
/// operators to clear findings without reading them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confidence {
    Shaped,
    Heuristic,
}

fn confidence_of(kind: SecretKind) -> Confidence {
    match kind {
        SecretKind::AssignedSecret => Confidence::Heuristic,
        _ => Confidence::Shaped,
    }
}

/// Scan a unified diff for secret patterns (§4.8).
///
/// The whole diff is scanned, as §4.8 says, but the two sides are not the same
/// claim. A credential on a `+` line was introduced by this attempt and blocks
/// the completion gate. The same credential on a context line was already in the
/// file: it is still worth a finding — the branch contains a secret either
/// way — but charging it to this attempt, and refusing to complete the run over
/// it, would be a false accusation of the agent and an unfixable state for the
/// operator.
///
/// Input that is not a unified diff is scanned as though every line were
/// context, so a caller who passes the wrong text never receives a
/// falsely-clean audit.
pub fn audit_diff_for_secrets(diff: &str) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    let mut path: Option<String> = None;
    let mut new_line: u64 = 0;
    let mut old_line: u64 = 0;
    let mut overflow = 0usize;

    for (index, raw) in diff.lines().enumerate() {
        let (side, content, location) = if let Some(rest) = raw.strip_prefix("+++ ") {
            path = parse_header_path(rest);
            (Side::Existing, raw, Location::DiffLine(index + 1))
        } else if raw.starts_with("--- ") {
            (Side::Existing, raw, Location::DiffLine(index + 1))
        } else if raw.starts_with("@@") {
            if let Some((old, new)) = parse_hunk_header(raw) {
                old_line = old;
                new_line = new;
            }
            (Side::Existing, raw, Location::DiffLine(index + 1))
        } else if let Some(rest) = raw.strip_prefix('+') {
            let at = new_line;
            new_line += 1;
            (Side::Added, rest, Location::File(at))
        } else if let Some(rest) = raw.strip_prefix('-') {
            let at = old_line;
            old_line += 1;
            (Side::Existing, rest, Location::File(at))
        } else if let Some(rest) = raw.strip_prefix(' ') {
            let at = new_line;
            new_line += 1;
            old_line += 1;
            (Side::Existing, rest, Location::File(at))
        } else {
            // `diff --git`, `index …`, `\ No newline …`, or text that is not a
            // diff at all. Scanned, attributed to nobody.
            (Side::Existing, raw, Location::DiffLine(index + 1))
        };

        for (kind, redacted) in matches_in(content) {
            if findings.len() >= MAX_MATCH_FINDINGS {
                overflow += 1;
                continue;
            }
            let (finding_kind, severity) = match (side, confidence_of(kind)) {
                (Side::Added, Confidence::Shaped) => (KIND_SECRET_IN_DIFF, SEVERITY_CRITICAL),
                (Side::Added, Confidence::Heuristic) => (KIND_SECRET_IN_DIFF, SEVERITY_WARNING),
                (Side::Existing, Confidence::Shaped) => {
                    (KIND_SECRET_IN_DIFF_CONTEXT, SEVERITY_WARNING)
                }
                (Side::Existing, Confidence::Heuristic) => {
                    (KIND_SECRET_IN_DIFF_CONTEXT, SEVERITY_INFO)
                }
            };
            let where_ = location.describe(path.as_deref(), index + 1);
            let provenance = match side {
                Side::Added => "added by this attempt",
                Side::Existing => {
                    "present in the diff but not introduced by this attempt (context, \
                     a removed line, or a header)"
                }
            };
            let note = match confidence_of(kind) {
                Confidence::Shaped => "",
                Confidence::Heuristic => {
                    " — name-based match: the identifier reads as a credential, which is \
                     a heuristic and can be an ordinary expression"
                }
            };
            findings.push(AuditFinding::new(
                finding_kind,
                severity,
                format!(
                    "{} at {where_} — {provenance}{note}. Line: {redacted}",
                    kind.label()
                ),
            ));
        }
    }

    if overflow > 0 {
        findings.push(AuditFinding::new(
            KIND_SECRET_IN_DIFF,
            SEVERITY_CRITICAL,
            format!(
                "{overflow} further secret-pattern match(es) in this diff were not \
                 enumerated: the audit stops at {MAX_MATCH_FINDINGS} findings per surface. \
                 A diff with this many matches is reviewed as a whole, not line by line."
            ),
        ));
    }
    findings
}

/// Where a match was found.
#[derive(Debug, Clone, Copy)]
enum Location {
    /// A line number in the file the hunk describes.
    File(u64),
    /// A line number in the diff text, used when there is no hunk to count in.
    DiffLine(usize),
}

impl Location {
    fn describe(&self, path: Option<&str>, diff_line: usize) -> String {
        match (self, path) {
            (Location::File(line), Some(path)) => format!("{path}:{line}"),
            (Location::File(line), None) => {
                format!("<no file header>:{line} (diff line {diff_line})")
            }
            (Location::DiffLine(line), Some(path)) => format!("{path} (diff line {line})"),
            (Location::DiffLine(line), None) => format!("<no file header> (diff line {line})"),
        }
    }
}

/// `+++ b/src/config.rs` → `src/config.rs`; `+++ /dev/null` → nothing.
fn parse_header_path(rest: &str) -> Option<String> {
    let path = rest.split('\t').next().unwrap_or(rest).trim();
    if path.is_empty() || path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("b/")
            .or_else(|| path.strip_prefix("a/"))
            .unwrap_or(path)
            .to_string(),
    )
}

/// `@@ -12,7 +12,9 @@ trailing` → `(12, 12)`.
fn parse_hunk_header(line: &str) -> Option<(u64, u64)> {
    let body = line.trim_start_matches('@').trim();
    let mut old = None;
    let mut new = None;
    for field in body.split_whitespace() {
        let (target, digits) = match field.as_bytes().first() {
            Some(b'-') => (&mut old, &field[1..]),
            Some(b'+') => (&mut new, &field[1..]),
            _ => continue,
        };
        let count = digits.split(',').next().unwrap_or(digits);
        if let Ok(value) = count.parse::<u64>() {
            *target = Some(value);
        }
    }
    Some((old?, new?))
}

/// Every credible match in one line, paired with a redacted quotation of it.
///
/// The quotation is the *whole line* after redaction, truncated. Quoting only
/// the neighbourhood of the match would be worse: a private-key header is
/// matched on its header line alone, so the base64 body on the following lines
/// is not redacted, and a "context window" that reached into it would copy the
/// key into the finding.
fn matches_in(line: &str) -> Vec<(SecretKind, String)> {
    let found = scan_merged(line);
    if found.is_empty() {
        return Vec::new();
    }
    let kept: Vec<SecretKind> = found
        .iter()
        .filter(|m| {
            confidence_of(m.kind) == Confidence::Shaped
                // A slice that is not a char boundary cannot be judged, so it is
                // kept: the failure direction here is "report something that
                // might be ordinary", never "stay silent about a credential".
                || line.get(m.start..m.end).is_none_or(is_credential_shaped_literal)
        })
        .map(|m| m.kind)
        .collect();
    if kept.is_empty() {
        return Vec::new();
    }
    // Redaction covers **every** span found, not only the ones being reported.
    // A match filtered out as probable code is still a span the scanner thought
    // was a credential, and quoting it because this module disagreed would be
    // the leak this whole design exists to prevent.
    let redacted = truncate_chars(&redact_spans(line, &found), MAX_CONTEXT_CHARS);
    kept.into_iter()
        .map(|kind| (kind, redacted.clone()))
        .collect()
}

/// Characters that glue a credential to the name in front of it.
///
/// Each is one ASCII byte and is replaced by one space, so offsets into the
/// rewritten copy are valid offsets into the original.
const VALUE_SEPARATORS: &[char] = &['=', ':', '"', '\'', ','];

/// [`secrets::scan`], plus the same scan over a copy in which `NAME=value` has
/// been split apart.
///
/// The shared scanner splits a line on **whitespace**, which is right for the
/// log excerpts S4 built it for and wrong for the two inputs §4.8 hands it. In
/// `AWS_KEY=AKIAIOSFODNN7EXAMPLE` — the canonical shape of a credential staged
/// in a `.env` file, which is exactly what §4.9's limitation 2 predicts — the
/// key id is not its own word, so no prefix rule sees it, and `AWS_KEY` is not
/// on the sensitive-name list either. The line scans clean while carrying an
/// AWS key in plain sight. That was found by a test, not by reading.
///
/// The correction is not a second detector: the identical
/// [`secrets::scan`] runs again over a copy with [`VALUE_SEPARATORS`] blanked,
/// and the two result sets are merged. Every rule, and every published blind
/// spot, stays in one place. Fixing it inside `verify::secrets` was rejected
/// because that module's callers are S4's log paths, whose tokenisation is
/// correct as it stands, and changing a shared detector for one caller's input
/// distribution is how detectors silently lose rules.
fn scan_merged(text: &str) -> Vec<SecretMatch> {
    let split: String = text
        .chars()
        .map(|c| {
            if VALUE_SEPARATORS.contains(&c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    debug_assert_eq!(
        split.len(),
        text.len(),
        "separator blanking changed the byte length"
    );

    let mut found = secrets::scan(text);
    found.extend(secrets::scan(&split));
    found.sort_by_key(|m| (m.start, m.end));

    let mut merged: Vec<SecretMatch> = Vec::with_capacity(found.len());
    for m in found {
        match merged.last_mut() {
            // The same secret seen by both passes, or two rules covering
            // overlapping spans: one finding, one redaction marker.
            Some(last) if m.start < last.end => last.end = last.end.max(m.end),
            _ => merged.push(m),
        }
    }
    merged
}

/// Replace each span with `[REDACTED:<kind>]`.
///
/// [`secrets::redact`] does this for the spans *it* finds; this module has to
/// splice the merged set, because a span found only by the separator-blanked
/// pass would otherwise survive into the finding verbatim.
fn redact_spans(text: &str, matches: &[SecretMatch]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for m in matches {
        if m.start < cursor
            || m.end > text.len()
            || !text.is_char_boundary(m.start)
            || !text.is_char_boundary(m.end)
        {
            continue;
        }
        out.push_str(&text[cursor..m.start]);
        out.push_str("[REDACTED:");
        out.push_str(m.kind.label());
        out.push(']');
        cursor = m.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Whether an `assigned-secret` value looks like a credential rather than code.
///
/// [`crate::verify::secrets`] was written for **log excerpts**, where
/// `token=<something>` is almost always a credential. §4.8 points it at a
/// **source diff**, where `let token = self.parse_token(input)?;` and
/// `pub password: Option<String>,` are ordinary lines, and where the same rule
/// would raise a finding on a large fraction of real commits. The fix belongs
/// here, at the call site that changed the input distribution, and not in the
/// shared scanner: S4's callers still want the log behaviour, and rewriting a
/// module two slices own is how a shared detector silently loses a rule.
///
/// The test is deliberately about the value's *alphabet*, not its entropy. A
/// credential is drawn from `[A-Za-z0-9]` plus a small set of separators; a Rust
/// or JSON expression contains brackets, semicolons, angle brackets or a call.
/// A dotted lowercase path like `config.api_key` passes the alphabet test, so a
/// digit is also required — which is what loses `correcthorsebattery` and is
/// recorded in the module's blind-spot list rather than papered over.
fn is_credential_shaped_literal(value: &str) -> bool {
    let value = value
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ';' | ',' | ')' | ']' | '}' | '('))
        .trim();
    if value.len() < 12 {
        return false;
    }
    if !value.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'+' | b'/' | b'=' | b':' | b'~')
    }) {
        return false;
    }
    value.bytes().any(|b| b.is_ascii_digit()) && value.bytes().any(|b| b.is_ascii_alphabetic())
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.trim().to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// §4.8 — "`/tmp` delta during the attempt window"
// ---------------------------------------------------------------------------

/// What a directory entry looked like at snapshot time.
///
/// Name alone would miss a file whose contents changed under an unchanged name,
/// which matters because §4.7's recovery re-enters a workspace whose per-run
/// `TMPDIR` is already populated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    is_dir: bool,
    is_symlink: bool,
    len: u64,
    modified: Option<SystemTime>,
}

/// The before-half of the `/tmp` delta: a snapshot taken **before** the attempt.
///
/// Obtained from [`watch_temp`] and consumed by [`audit_temp_delta`], which
/// re-reads the same two paths. The paths are carried inside the watch rather
/// than passed twice, because an audit that compared a snapshot of one directory
/// against a listing of another would produce confident nonsense.
#[derive(Debug, Clone)]
pub struct TempWatch {
    system_tmp: PathBuf,
    run_tmpdir: PathBuf,
    window_start: SystemTime,
    /// `None` when the directory could not be listed — which is reported, not
    /// treated as "it was empty".
    system_before: Option<BTreeSet<OsString>>,
    run_before: Option<BTreeMap<PathBuf, Stamp>>,
}

impl TempWatch {
    /// The instant the attempt window opens.
    pub fn window_start(&self) -> SystemTime {
        self.window_start
    }

    /// The system temp directory being watched (the caller passes `/tmp`).
    pub fn system_tmp(&self) -> &Path {
        &self.system_tmp
    }

    /// The per-run `TMPDIR` being watched.
    pub fn run_tmpdir(&self) -> &Path {
        &self.run_tmpdir
    }
}

/// Snapshot both temp surfaces before the attempt starts.
///
/// Infallible on purpose. A snapshot that could not be taken is recorded as
/// absent and turns into a [`KIND_TEMP_AUDIT_INCOMPLETE`] finding at audit time;
/// returning an error here would put the caller in the position of choosing
/// between aborting the attempt and dropping the audit, and the second is what
/// would actually happen.
///
/// `system_tmp` is a parameter rather than a hardcoded `/tmp` so that this is
/// testable without writing into the host's real temp directory — a test that
/// has to litter `/tmp` to assert anything is a test nobody runs twice.
/// `window_start` is likewise explicit: this crate passes timestamps in from the
/// caller everywhere else, and a test needs to be able to describe a window that
/// the observed files cannot fall inside.
pub fn watch_temp(system_tmp: &Path, run_tmpdir: &Path, window_start: SystemTime) -> TempWatch {
    TempWatch {
        system_tmp: system_tmp.to_path_buf(),
        run_tmpdir: run_tmpdir.to_path_buf(),
        window_start,
        system_before: list_top_level(system_tmp),
        run_before: walk_bounded(run_tmpdir),
    }
}

/// Compare both temp surfaces against the snapshot and report every delta.
///
/// The per-run `TMPDIR` is the surface with real signal: Conductor created it
/// empty, only this run's children were given it, and its contents are read.
/// The system temp directory is the residual surface: shared with every other
/// process on the machine, so only names are collected and no finding from it is
/// ever blocking. See the module docs for the full list of what this cannot
/// prove.
pub fn audit_temp_delta(watch: &TempWatch) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    audit_run_tmpdir(watch, &mut findings);
    audit_system_tmp(watch, &mut findings);
    findings
}

fn audit_run_tmpdir(watch: &TempWatch, findings: &mut Vec<AuditFinding>) {
    let observed = walk_bounded(&watch.run_tmpdir);
    if watch.run_before.is_none() || observed.is_none() {
        findings.push(AuditFinding::new(
            KIND_TEMP_AUDIT_INCOMPLETE,
            SEVERITY_WARNING,
            format!(
                "the per-run TMPDIR at {} could not be listed on one side of the attempt \
                 window, so §4.8's delta could not be computed. Reported rather than \
                 returned as a clean audit: an audit that could not look has not found \
                 nothing. Everything present is treated as new below.",
                watch.run_tmpdir.display()
            ),
        ));
    }
    // Losing the *delta* is not a reason to stop looking at what is there. A
    // credential sitting in the workspace is worth a finding whether or not the
    // audit can say when it arrived, and an early return here would have made
    // "the snapshot failed" into a way of switching the scan off.
    let Some(after) = observed else { return };
    let empty = BTreeMap::new();
    let before = watch.run_before.as_ref().unwrap_or(&empty);

    let changed: Vec<&PathBuf> = after
        .iter()
        .filter(|(path, stamp)| before.get(*path) != Some(*stamp))
        .map(|(path, _)| path)
        .collect();
    if changed.is_empty() {
        return;
    }

    let mut matches = 0usize;
    for path in &changed {
        let stamp = &after[*path];
        if stamp.is_dir || stamp.is_symlink {
            continue;
        }
        let absolute = watch.run_tmpdir.join(path);
        let Some(text) = read_head(&absolute) else {
            continue;
        };
        for (line_no, line) in text.lines().enumerate() {
            for (kind, redacted) in matches_in(line) {
                if matches >= MAX_MATCH_FINDINGS {
                    continue;
                }
                matches += 1;
                let severity = match confidence_of(kind) {
                    Confidence::Shaped => SEVERITY_CRITICAL,
                    Confidence::Heuristic => SEVERITY_WARNING,
                };
                findings.push(AuditFinding::new(
                    KIND_SECRET_IN_RUN_TMPDIR,
                    severity,
                    format!(
                        "{} staged in the per-run TMPDIR at {}:{} — Conductor creates this \
                         directory empty and gives it only to this run's child processes \
                         (§4.9's M7 mitigation), so the content is attributable to the \
                         attempt, though not to a particular child process. Line: {redacted}",
                        kind.label(),
                        display_relative(path),
                        line_no + 1,
                    ),
                ));
            }
        }
    }

    let listed: Vec<String> = changed
        .iter()
        .take(MAX_LISTED_ENTRIES)
        .map(|path| display_relative(path))
        .collect();
    let remainder = changed.len().saturating_sub(listed.len());
    let suffix = if remainder > 0 {
        format!(" … and {remainder} more")
    } else {
        String::new()
    };
    findings.push(AuditFinding::new(
        KIND_RUN_TMPDIR_RESIDUE,
        SEVERITY_INFO,
        format!(
            "run TMPDIR inventory: {} new or changed entr{} under {} during the attempt \
             window. A per-run TMPDIR exists so that ordinary temp usage stays inside the \
             workspace (§4.9), so its contents are expected — this is the inventory that \
             makes them auditable, not an accusation. It is invisible to `git status` by \
             design (`.git/info/exclude`), which is why the audit reads it directly. \
             Entries: {}{suffix}",
            changed.len(),
            if changed.len() == 1 { "y" } else { "ies" },
            watch.run_tmpdir.display(),
            listed.join(", "),
        ),
    ));
}

fn audit_system_tmp(watch: &TempWatch, findings: &mut Vec<AuditFinding>) {
    let (before, after) = match (&watch.system_before, list_top_level(&watch.system_tmp)) {
        (Some(before), Some(after)) => (before, after),
        _ => {
            findings.push(AuditFinding::new(
                KIND_TEMP_AUDIT_INCOMPLETE,
                SEVERITY_WARNING,
                format!(
                    "the system temp directory {} could not be listed on one side of the \
                     attempt window, so §4.8's `/tmp` delta could not be computed. \
                     Reported rather than returned as a clean audit. Unlike the per-run \
                     TMPDIR, nothing is enumerated as a fallback: this directory's \
                     contents are never read, so a listing without a baseline would be \
                     an inventory of other processes' files and no evidence at all.",
                    watch.system_tmp.display()
                ),
            ));
            return;
        }
    };

    let appeared: Vec<&OsString> = after.difference(before).collect();
    if appeared.is_empty() {
        return;
    }

    let mut consistent: Vec<String> = Vec::new();
    let mut unconnected: Vec<String> = Vec::new();
    for name in appeared {
        let display = name.to_string_lossy().to_string();
        if evidence_consistent_with_window(&watch.system_tmp.join(name), watch.window_start) {
            consistent.push(display);
        } else {
            unconnected.push(display);
        }
    }

    if !consistent.is_empty() {
        findings.push(AuditFinding::new(
            KIND_SYSTEM_TMP_DELTA,
            SEVERITY_WARNING,
            format!(
                "{} name(s) appeared in {} during the attempt window, owned by this uid \
                 and stamped inside the window. That is consistent with, but **not proof \
                 of**, agent activity: every process running as this user shares that \
                 directory, and both ownership and timestamps are things the writer \
                 controls. Contents were deliberately not read. Never blocking. \
                 Names: {}",
                consistent.len(),
                watch.system_tmp.display(),
                summarise(&consistent),
            ),
        ));
    }
    if !unconnected.is_empty() {
        findings.push(AuditFinding::new(
            KIND_SYSTEM_TMP_DELTA,
            SEVERITY_INFO,
            format!(
                "{} name(s) appeared in {} between the two listings, but the available \
                 evidence — ownership and creation/modification time — does not connect \
                 them to this run. Recorded rather than dropped, because §4.8 turns \
                 unexplained deltas into findings and this is the most unexplained kind. \
                 Names: {}",
                unconnected.len(),
                watch.system_tmp.display(),
                summarise(&unconnected),
            ),
        ));
    }
}

fn summarise(names: &[String]) -> String {
    let listed: Vec<&str> = names
        .iter()
        .take(MAX_LISTED_ENTRIES)
        .map(String::as_str)
        .collect();
    let remainder = names.len().saturating_sub(listed.len());
    if remainder > 0 {
        format!("{} … and {remainder} more", listed.join(", "))
    } else {
        listed.join(", ")
    }
}

/// Whether an entry's own metadata is consistent with having been created by
/// this run: owned by this uid, and stamped at or after the window opened.
///
/// This is the *weakest* of the module's claims and the docs say so. It is
/// computed from `symlink_metadata`, so a symlink is judged on itself rather
/// than on whatever it points at — following a link out of the system temp
/// directory to stat an arbitrary path would be the audit doing the agent's
/// reconnaissance for it.
fn evidence_consistent_with_window(path: &Path, window_start: SystemTime) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: `getuid` has no preconditions and cannot fail.
        if meta.uid() != unsafe { libc::getuid() } {
            return false;
        }
    }
    let stamp = meta.created().or_else(|_| meta.modified());
    matches!(stamp, Ok(at) if at >= window_start)
}

/// Top-level names only. No recursion and no reads: see the module docs.
fn list_top_level(dir: &Path) -> Option<BTreeSet<OsString>> {
    let mut names = BTreeSet::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        names.insert(entry.file_name());
    }
    Some(names)
}

/// Every entry under `root`, relative, bounded in breadth and depth.
///
/// Symlinks are recorded but never followed: a symlink an agent plants in its
/// own `TMPDIR` pointing at `~/.ssh` would otherwise turn the audit into the
/// exfiltration path it exists to detect.
fn walk_bounded(root: &Path) -> Option<BTreeMap<PathBuf, Stamp>> {
    if !root.is_dir() {
        return None;
    }
    let mut out = BTreeMap::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            if out.len() >= MAX_TMPDIR_ENTRIES {
                return Some(out);
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            let is_symlink = meta.file_type().is_symlink();
            let is_dir = meta.is_dir() && !is_symlink;
            out.insert(
                relative.to_path_buf(),
                Stamp {
                    is_dir,
                    is_symlink,
                    len: meta.len(),
                    modified: meta.modified().ok(),
                },
            );
            if is_dir && depth + 1 < MAX_TMPDIR_DEPTH {
                stack.push((path, depth + 1));
            }
        }
    }
    Some(out)
}

/// The first [`MAX_SCAN_BYTES_PER_FILE`] bytes, decoded lossily.
///
/// Lossy rather than "skip if not UTF-8": a credential written into a file that
/// also contains a stray non-UTF-8 byte is still a credential, and the scanner's
/// patterns are ASCII. Content past the cap is not scanned, which is item 8 in
/// the module's list of what this cannot prove.
fn read_head(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut buffer = Vec::new();
    file.take(MAX_SCAN_BYTES_PER_FILE as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// A relative path with `/` separators, so that a finding reads the same on
/// every platform and hashes the same on every run.
fn display_relative(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hunk_header_gives_both_starting_line_numbers() {
        assert_eq!(
            parse_hunk_header("@@ -12,7 +34,9 @@ fn main() {"),
            Some((12, 34))
        );
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ nonsense @@"), None);
    }

    #[test]
    fn a_code_expression_is_not_a_credential_literal() {
        // The half of the pair that keeps the filter from being "return false".
        assert!(!is_credential_shaped_literal("self.parse_token(input)?"));
        assert!(!is_credential_shaped_literal("Option<String>"));
        assert!(!is_credential_shaped_literal("config.api_key"));
        assert!(!is_credential_shaped_literal("short1"));
        assert!(is_credential_shaped_literal("s3cr3t-p4ssw0rd-value"));
        assert!(is_credential_shaped_literal("\"abcd1234efgh5678ijkl\";"));
    }

    #[test]
    fn a_finding_cannot_be_built_with_the_secret_still_in_it() {
        let finding = AuditFinding::new(
            KIND_SECRET_IN_DIFF,
            SEVERITY_CRITICAL,
            "somebody forgot to redact AKIAIOSFODNN7EXAMPLE before building this",
        );
        assert!(!finding.detail().contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(finding.detail().contains("[REDACTED:aws-access-key-id]"));
    }

    #[test]
    fn a_header_path_loses_its_prefix_and_dev_null_is_no_path() {
        assert_eq!(
            parse_header_path("b/src/config.rs"),
            Some("src/config.rs".to_string())
        );
        assert_eq!(parse_header_path("/dev/null"), None);
    }
}
