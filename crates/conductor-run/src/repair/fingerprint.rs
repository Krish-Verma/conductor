//! The failure fingerprint — master plan §4.6.
//!
//! ```text
//! fingerprint(failure) = blake3(
//!       sorted(failing_check_ids)
//!    ‖  normalized(first_failing_assertion)   # paths, line numbers, addresses, timings stripped
//! )
//! ```
//!
//! # Normalization is the load-bearing half
//!
//! Every loop-breaker in §4.6 compares fingerprints. If normalization leaves one
//! line number in, then an agent that edits *anything* above a failing assertion
//! produces a different fingerprint for the same failure — `progressed()` returns
//! true, breaker 1 never fires, breaker 2 never sees a repeat, and the only thing
//! left standing between a stuck task and its whole budget is the budget. Nothing
//! fails loudly when that happens, which is why this module is tested against
//! real `cargo test` and `rustc` output rather than against paraphrases.
//!
//! # Which direction the errors go
//!
//! Two ways to be wrong, and they are not symmetric:
//!
//! * **Too little stripping** → two identical failures look different → the loop
//!   runs to the budget. Silent.
//! * **Too much stripping** → two different failures look identical → the loop
//!   stops early and a human is asked. Loud, and safe.
//!
//! So where a token is ambiguous this module strips it. The one thing it must
//! **not** do is strip the values inside an assertion (`left: 1` / `right: 2`):
//! those are the entire difference between "the same bug again" and "a different
//! bug", and a normalizer that replaced every integer would collapse the two.
//! Numbers are therefore only removed where something else in the line says the
//! number is a position, a duration or an address.
//!
//! # No regex crate
//!
//! For the reason `verify::secrets` gives: §2.2's dependency list does not
//! include `regex`, these are prefix-and-shape tests over ASCII tokens, and the
//! tests below cover them exhaustively.

/// What a stripped path becomes.
pub const PATH: &str = "<path>";
/// What a stripped address becomes.
pub const ADDR: &str = "<addr>";
/// What a stripped duration or timestamp becomes.
pub const TIME: &str = "<time>";
/// What a stripped positional number becomes.
pub const NUMBER: &str = "<n>";
/// What a stripped long hex digest becomes.
pub const HEX: &str = "<hex>";

/// How many lines of an assertion the fingerprint reads.
///
/// One line is not enough: `thread '…' panicked at <path>` is identical for
/// every panic in a file, so the values that distinguish two failures live on
/// the lines after it. The whole log is far too much — it contains the timings
/// and counts that make every run unique. The block ends at the first blank
/// line, at the first `note:`/`help:`/`warning:`/`test result:` line, or here.
pub const ASSERTION_MAX_LINES: usize = 8;

/// Words after which a bare number is a position rather than a value.
const POSITIONAL: &[&str] = &["line", "lines", "col", "column", "pid", "offset", "byte"];

/// Lines that end an assertion block: everything after them is advice or
/// summary, and the summary is where the timings are.
const BLOCK_ENDS: &[&str] = &["note:", "help:", "warning:", "test result:", "failures:"];

/// Markers of a failure, most specific first.
///
/// The tiers matter. `cargo test` prints `test foo ... FAILED` *before* the
/// panic that explains it, so a scanner that took the first line containing
/// "FAILED" would fingerprint the test's name and nothing else — and every
/// failure of that test, for any reason, would then look identical.
const MARKERS: &[&[&str]] = &[
    &[
        "panicked at",
        "assertion failed",
        "assertion `",
        "assertionerror",
        "fatal error:",
        "segmentation fault",
    ],
    &["error[", "error:", "exception:", "traceback (most recent"],
    &["failed:", "failure:", "fail:", " failed", "✗"],
];

/// Strip everything that varies between two runs of the same failure.
pub fn normalize(text: &str) -> String {
    text.lines()
        .map(normalize_line)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_line(line: &str) -> String {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];

        // `took 15 ms` — the number and its unit are two tokens.
        if is_number(trim_punctuation(token))
            && tokens
                .get(index + 1)
                .is_some_and(|next| is_bare_time_unit(trim_punctuation(next)))
        {
            out.push(TIME.to_string());
            index += 2;
            continue;
        }

        // `line 42`, `pid 1234` — the previous word says this number is a
        // position, so it is not a value.
        if index > 0 && is_positional_word(tokens[index - 1]) && is_number(trim_punctuation(token))
        {
            out.push(NUMBER.to_string());
            index += 1;
            continue;
        }

        out.push(normalize_token(token));
        index += 1;
    }
    out.join(" ")
}

fn normalize_token(token: &str) -> String {
    let core = trim_punctuation(token);
    if core.is_empty() {
        return token.to_string();
    }
    if is_address(core) {
        return ADDR.to_string();
    }
    if is_timestamp(core) || is_clock(core) || is_duration(core) {
        return TIME.to_string();
    }
    if is_path(core) || is_file_reference(core) {
        return PATH.to_string();
    }
    if is_long_hex(core) {
        return HEX.to_string();
    }
    token.to_string()
}

/// Strip the punctuation a token is wrapped in, keeping what is inside.
///
/// Deliberately does **not** strip `[` from `error[E0308]`: the brackets are
/// part of the code, and the code is the most discriminating thing in a
/// compiler error. Only leading/trailing wrappers go.
fn trim_punctuation(token: &str) -> &str {
    token.trim_matches(|c: char| "([{<,;.)]}>'\"`".contains(c))
}

fn is_number(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
        && text.chars().any(|c| c.is_ascii_digit())
}

fn is_positional_word(token: &str) -> bool {
    let word = trim_punctuation(token)
        .trim_end_matches(':')
        .to_ascii_lowercase();
    POSITIONAL.contains(&word.as_str())
}

fn is_bare_time_unit(text: &str) -> bool {
    matches!(
        text,
        "s" | "ms" | "us" | "µs" | "ns" | "sec" | "secs" | "seconds" | "min" | "mins" | "minutes"
    )
}

fn is_address(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) else {
        return false;
    };
    rest.len() >= 4 && rest.chars().all(|c| c.is_ascii_hexdigit())
}

/// `12.5s`, `0.04s`, `15ms`, `3m`, `2h`.
fn is_duration(text: &str) -> bool {
    for unit in ["ms", "us", "µs", "ns", "s", "m", "h"] {
        if let Some(head) = text.strip_suffix(unit)
            && is_number(head)
        {
            return true;
        }
    }
    false
}

/// `2026-08-13T10:11:12.345Z` and its date-only prefix.
fn is_timestamp(text: &str) -> bool {
    text.len() >= 10
        && text.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && text.as_bytes()[4] == b'-'
        && text.as_bytes()[7] == b'-'
}

/// `10:11:12`, `10:11:12.345`.
fn is_clock(text: &str) -> bool {
    let parts: Vec<&str> = text.split(':').collect();
    parts.len() == 3 && parts.iter().all(|p| is_number(p))
}

/// Anything with a separator in it. A URL counts, and that is intended: a
/// remote in a message is as run-specific as a temporary directory.
fn is_path(text: &str) -> bool {
    text.contains('/') && text.len() > 1
}

/// `evaluate.rs:212:9` — a file reference that happens to have no directory.
fn is_file_reference(text: &str) -> bool {
    let mut parts = text.split(':');
    let Some(head) = parts.next() else {
        return false;
    };
    let rest: Vec<&str> = parts.collect();
    !rest.is_empty()
        && head.contains('.')
        && !head.is_empty()
        && rest.iter().all(|p| !p.is_empty() && is_number(p))
}

/// A git sha, a blake3 digest, a temporary-directory suffix.
fn is_long_hex(text: &str) -> bool {
    text.len() >= 12 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// The first failing assertion in a check's log, with the lines that give it
/// meaning.
///
/// `None` only when there is nothing at all to read: a log with no recognised
/// marker falls back to its first non-empty line, because returning `None`
/// there would give every unrecognised failure the same fingerprint — which is
/// the conflation that stops the loop on failures with nothing in common.
pub fn first_failing_assertion(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let start = find_marker(&lines).or_else(|| lines.iter().position(|l| !l.trim().is_empty()))?;

    let mut block: Vec<&str> = Vec::new();
    for line in lines.iter().skip(start).take(ASSERTION_MAX_LINES) {
        if !block.is_empty() && (line.trim().is_empty() || ends_block(line)) {
            break;
        }
        block.push(line.trim_end());
    }
    let text = block.join("\n");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn find_marker(lines: &[&str]) -> Option<usize> {
    for tier in MARKERS {
        if let Some(index) = lines.iter().position(|line| {
            let lower = line.to_ascii_lowercase();
            tier.iter().any(|marker| lower.contains(marker))
        }) {
            return Some(index);
        }
    }
    None
}

fn ends_block(line: &str) -> bool {
    let lower = line.trim_start().to_ascii_lowercase();
    BLOCK_ENDS.iter().any(|end| lower.starts_with(end))
}

/// A failure's identity — §4.6.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// §4.6's definition, exactly.
    ///
    /// The ids are sorted because §4.6 says `sorted(…)`: without it the
    /// fingerprint would depend on the order the schedule happened to run in.
    /// The separator is the same unit separator `OperationId::compute` uses, and
    /// for the same reason — it cannot occur in a check id or in normalized
    /// text, so no two different inputs can produce one concatenation.
    pub fn compute<'a>(
        failing_check_ids: impl IntoIterator<Item = &'a str>,
        first_failing_assertion: &str,
    ) -> Fingerprint {
        const SEP: u8 = 0x1f;
        let mut ids: Vec<&str> = failing_check_ids.into_iter().collect();
        ids.sort_unstable();

        let mut hasher = blake3::Hasher::new();
        for id in ids {
            hasher.update(id.as_bytes());
            hasher.update(&[SEP]);
        }
        hasher.update(&[SEP]);
        hasher.update(normalize(first_failing_assertion).as_bytes());
        Fingerprint(format!("blake3:{}", hasher.finalize().to_hex()))
    }

    /// The digest, `blake3:` first (ADR-0007).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first twelve digest characters, for messages a human reads.
    pub fn short(&self) -> String {
        self.0
            .trim_start_matches("blake3:")
            .chars()
            .take(12)
            .collect()
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
