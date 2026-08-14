//! A secret detector for anything on its way out of a log.
//!
//! §4.5 requires verification logs to be "secret-scanned before any excerpt
//! enters a packet", and §11.2 makes the minimum "scanner on every path into an
//! artifact or packet, tested with planted secrets".
//!
//! # Scope, stated rather than implied
//!
//! **S9 owns the real scanner.** This is the subset S4 needs to keep its own
//! promise, and it is written down exactly so that S9 inherits a specification
//! rather than a guess. See [`SecretKind`] for what is detected and
//! [`NOT_DETECTED`] for what is not. What this module will **not** do is return
//! "clean" without looking: every excerpt is scanned, and a scanner that cannot
//! decide redacts rather than passes.
//!
//! # Why hand-written matchers and not a regex crate
//!
//! §2.2's dependency list does not include `regex`, and CLAUDE.md requires a
//! justification for anything outside it. These patterns are prefix-and-shape
//! tests over ASCII; a regex engine would add a dependency tree to express what
//! is already exhaustively covered by the planted-secret tests below. If S9
//! needs entropy analysis or a large managed ruleset, that is the slice to
//! re-open the question in, with a corpus in hand.

/// What kind of secret a match is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SecretKind {
    /// `-----BEGIN … PRIVATE KEY-----`.
    PrivateKeyBlock,
    /// An AWS access key id (`AKIA…`, `ASIA…`, and the other documented
    /// prefixes) followed by 16 upper-case alphanumerics.
    AwsAccessKeyId,
    /// A GitHub token: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_` + 36 or more.
    GitHubToken,
    /// A Slack token: `xoxb-`, `xoxa-`, `xoxp-`, `xoxr-`, `xoxs-`.
    SlackToken,
    /// An Anthropic key: `sk-ant-` + 20 or more.
    AnthropicKey,
    /// A generic `sk-` key of 32 or more characters.
    GenericSkKey,
    /// A Google API key: `AIza` + 35.
    GoogleApiKey,
    /// A three-part JWT beginning with two base64url `{`-objects.
    JsonWebToken,
    /// Credentials inline in a URL: `scheme://user:password@host`.
    UrlCredentials,
    /// A value assigned to a name that means "secret".
    AssignedSecret,
}

impl SecretKind {
    /// A short label, used in the redaction marker and in findings.
    pub fn label(&self) -> &'static str {
        match self {
            SecretKind::PrivateKeyBlock => "private-key",
            SecretKind::AwsAccessKeyId => "aws-access-key-id",
            SecretKind::GitHubToken => "github-token",
            SecretKind::SlackToken => "slack-token",
            SecretKind::AnthropicKey => "anthropic-key",
            SecretKind::GenericSkKey => "sk-key",
            SecretKind::GoogleApiKey => "google-api-key",
            SecretKind::JsonWebToken => "jwt",
            SecretKind::UrlCredentials => "url-credentials",
            SecretKind::AssignedSecret => "assigned-secret",
        }
    }
}

/// What this scanner deliberately does **not** detect.
///
/// Written down because "we scan for secrets" is the kind of claim that turns
/// into an assumption. A reader of a redacted excerpt should know exactly how
/// much the redaction is worth.
pub const NOT_DETECTED: &[&str] = &[
    "a high-entropy string with no recognisable prefix and no assignment context",
    "a secret split across two or more lines",
    "a secret that has been base64-, hex- or URL-encoded",
    "a secret inside binary output that does not decode as UTF-8 text",
    "a password passed positionally, e.g. `mysql -u root hunter2`",
    "anything requiring entropy analysis, which S9 owns",
];

/// One detected secret, as a byte range in the text that was scanned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretMatch {
    /// What it looks like.
    pub kind: SecretKind,
    /// Byte offset of the first byte of the secret value.
    pub start: usize,
    /// Byte offset one past its last byte.
    pub end: usize,
}

/// Text with every detected secret replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    /// The text, safe to show.
    pub text: String,
    /// Every kind found, sorted and deduplicated.
    pub kinds: Vec<SecretKind>,
}

impl Redacted {
    /// Whether anything was found.
    pub fn is_clean(&self) -> bool {
        self.kinds.is_empty()
    }
}

/// Names whose value is a secret by virtue of the name.
const SENSITIVE_NAMES: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "access_key",
    "accesskey",
    "secret_key",
    "private_key",
    "auth_token",
    "authtoken",
    "credential",
    "credentials",
];

/// Values that look like a secret's shape but are a stand-in for one.
fn is_placeholder(value: &str) -> bool {
    let trimmed = value.trim_matches(['"', '\'', '`']);
    if trimmed.len() < 8 {
        return true;
    }
    if trimmed.starts_with('<') || trimmed.starts_with("${") || trimmed.starts_with('$') {
        return true;
    }
    let squashed: String = trimmed.to_ascii_lowercase();
    if squashed.chars().all(|c| c == '*' || c == '.' || c == 'x') {
        return true;
    }
    matches!(
        squashed.as_str(),
        "redacted" | "changeme" | "placeholder" | "your-key-here" | "none" | "null" | "example"
    )
}

/// Every secret in `text`, in order.
pub fn scan(text: &str) -> Vec<SecretMatch> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();

    for (line_start, line) in line_spans(text) {
        // Private key blocks are matched on the header alone: the body is
        // base64 and is redacted line by line only if it matches something else.
        // A header in a log is already the disclosure worth reporting.
        if let Some(offset) = line.find("-----BEGIN ")
            && line[offset..].contains("PRIVATE KEY-----")
        {
            let end = line[offset..]
                .find("PRIVATE KEY-----")
                .map(|i| offset + i + "PRIVATE KEY-----".len())
                .unwrap_or(line.len());
            found.push(SecretMatch {
                kind: SecretKind::PrivateKeyBlock,
                start: line_start + offset,
                end: line_start + end,
            });
        }

        scan_tokens(line, line_start, &mut found);
        scan_assignments(line, line_start, &mut found);
    }

    found.sort_by_key(|m| (m.start, m.end));
    // Overlapping matches (an assignment whose value is also a known token
    // shape) collapse to the first, so redaction never splices inside a marker.
    let mut deduped: Vec<SecretMatch> = Vec::with_capacity(found.len());
    for m in found {
        match deduped.last_mut() {
            Some(last) if m.start < last.end => {
                last.end = last.end.max(m.end);
            }
            _ => deduped.push(m),
        }
    }
    let _ = bytes;
    deduped
}

/// Prefix-shaped credentials, wherever they appear in a line.
fn scan_tokens(line: &str, line_start: usize, found: &mut Vec<SecretMatch>) {
    for (offset, word) in word_spans(line) {
        let kind = if is_aws_access_key_id(word) {
            Some(SecretKind::AwsAccessKeyId)
        } else if is_github_token(word) {
            Some(SecretKind::GitHubToken)
        } else if is_slack_token(word) {
            Some(SecretKind::SlackToken)
        } else if is_anthropic_key(word) {
            Some(SecretKind::AnthropicKey)
        } else if is_generic_sk_key(word) {
            Some(SecretKind::GenericSkKey)
        } else if is_google_api_key(word) {
            Some(SecretKind::GoogleApiKey)
        } else if is_jwt(word) {
            Some(SecretKind::JsonWebToken)
        } else {
            None
        };
        if let Some(kind) = kind {
            found.push(SecretMatch {
                kind,
                start: line_start + offset,
                end: line_start + offset + word.len(),
            });
            continue;
        }
        if let Some((start, end)) = url_credentials_span(word) {
            found.push(SecretMatch {
                kind: SecretKind::UrlCredentials,
                start: line_start + offset + start,
                end: line_start + offset + end,
            });
        }
    }
}

/// `NAME=value`, `"name": "value"` and `--name value`.
fn scan_assignments(line: &str, line_start: usize, found: &mut Vec<SecretMatch>) {
    for (offset, word) in word_spans(line) {
        // `--token value`: the name and the value are separate words.
        if let Some(flag) = word.strip_prefix("--")
            && is_sensitive_name(flag)
            && let Some((value_offset, value)) = next_word(line, offset + word.len())
            && !value.starts_with('-')
            && !is_placeholder(value)
        {
            let (trim_start, trim_end) = quote_trim(value);
            found.push(SecretMatch {
                kind: SecretKind::AssignedSecret,
                start: line_start + value_offset + trim_start,
                end: line_start + value_offset + value.len() - trim_end,
            });
        }
    }

    // `name=value` / `name: value`, including JSON's quoted spelling.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c != b'=' && c != b':' {
            i += 1;
            continue;
        }
        let name_end = i;
        let mut name_start = name_end;
        while name_start > 0 && is_name_byte(bytes[name_start - 1]) {
            name_start -= 1;
        }
        let name = line[name_start..name_end].trim_matches(['"', '\'', ' ']);
        if !is_sensitive_name(name) {
            i += 1;
            continue;
        }

        let mut value_start = name_end + 1;
        while value_start < bytes.len()
            && (bytes[value_start] == b' ' || bytes[value_start] == b'\t')
        {
            value_start += 1;
        }
        let mut value_end = value_start;
        while value_end < bytes.len() && !matches!(bytes[value_end], b' ' | b'\t' | b',') {
            value_end += 1;
        }
        let value = &line[value_start..value_end];
        if !value.is_empty() && !is_placeholder(value) {
            let (trim_start, trim_end) = quote_trim(value);
            found.push(SecretMatch {
                kind: SecretKind::AssignedSecret,
                start: line_start + value_start + trim_start,
                end: line_start + value_end - trim_end,
            });
        }
        i = value_end.max(i + 1);
    }
}

fn quote_trim(value: &str) -> (usize, usize) {
    let start = usize::from(value.starts_with(['"', '\'', '`']));
    let end = usize::from(value.len() > start && value.ends_with(['"', '\'', '`']));
    (start, end)
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'"' || b == b'\''
}

fn is_sensitive_name(name: &str) -> bool {
    let normalised: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let normalised = normalised.trim_start_matches('_');
    SENSITIVE_NAMES.iter().any(|candidate| {
        let candidate_squashed: String = candidate.replace('_', "");
        let name_squashed: String = normalised.replace('_', "");
        name_squashed == candidate_squashed || name_squashed.ends_with(&candidate_squashed)
    })
}

fn next_word(line: &str, from: usize) -> Option<(usize, &str)> {
    word_spans(&line[from..])
        .next()
        .map(|(offset, word)| (from + offset, word))
}

fn line_spans(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    text.split_inclusive('\n').map(move |line| {
        let at = start;
        start += line.len();
        (at, line.trim_end_matches(['\n', '\r']))
    })
}

fn word_spans(line: &str) -> impl Iterator<Item = (usize, &str)> {
    line.split_whitespace()
        .map(move |word| (word.as_ptr() as usize - line.as_ptr() as usize, word))
}

fn trimmed(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.')
}

fn is_aws_access_key_id(word: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "AKIA", "ASIA", "AGPA", "AIDA", "AROA", "AIPA", "ANPA", "ANVA", "ABIA", "A3T",
    ];
    let word = trimmed(word);
    PREFIXES.iter().any(|p| word.starts_with(p))
        && word.len() == 20
        && word
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

fn is_github_token(word: &str) -> bool {
    const PREFIXES: &[&str] = &["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];
    let word = trimmed(word);
    PREFIXES.iter().any(|p| word.starts_with(p))
        && word.len() >= 36
        && word[4..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn is_slack_token(word: &str) -> bool {
    const PREFIXES: &[&str] = &["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-", "xoxe-"];
    let word = trimmed(word);
    PREFIXES.iter().any(|p| word.starts_with(p)) && word.len() >= 20
}

fn is_anthropic_key(word: &str) -> bool {
    let word = trimmed(word);
    word.starts_with("sk-ant-") && word.len() >= 27
}

fn is_generic_sk_key(word: &str) -> bool {
    let word = trimmed(word);
    word.starts_with("sk-")
        && word.len() >= 35
        && word[3..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_google_api_key(word: &str) -> bool {
    let word = trimmed(word);
    word.starts_with("AIza")
        && word.len() == 39
        && word[4..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_jwt(word: &str) -> bool {
    let word = trimmed(word);
    if !word.starts_with("eyJ") {
        return false;
    }
    let parts: Vec<&str> = word.split('.').collect();
    parts.len() == 3
        && parts[1].starts_with("eyJ")
        && parts.iter().all(|p| p.len() >= 8)
        && parts.iter().all(|p| {
            p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// `scheme://user:password@host` — returns the span of `user:password`.
fn url_credentials_span(word: &str) -> Option<(usize, usize)> {
    let scheme_end = word.find("://")? + 3;
    let rest = &word[scheme_end..];
    let at = rest.find('@')?;
    let userinfo = &rest[..at];
    let colon = userinfo.find(':')?;
    if colon == 0 || colon + 1 == userinfo.len() {
        return None;
    }
    if is_placeholder(&userinfo[colon + 1..]) {
        return None;
    }
    Some((scheme_end, scheme_end + at))
}

/// Replace every detected secret with `[REDACTED:<kind>]`.
pub fn redact(text: &str) -> Redacted {
    let matches = scan(text);
    let mut out = String::with_capacity(text.len());
    let mut kinds: Vec<SecretKind> = Vec::new();
    let mut cursor = 0;
    for m in &matches {
        // Every pattern here is ASCII, so the span boundaries are char
        // boundaries. Assert rather than assume: a panic in the log writer is
        // the one failure this module must not cause.
        debug_assert!(text.is_char_boundary(m.start) && text.is_char_boundary(m.end));
        if m.start < cursor || m.end > text.len() {
            continue;
        }
        out.push_str(&text[cursor..m.start]);
        out.push_str(&format!("[REDACTED:{}]", m.kind.label()));
        cursor = m.end;
        if !kinds.contains(&m.kind) {
            kinds.push(m.kind);
        }
    }
    out.push_str(&text[cursor..]);
    Redacted { text: out, kinds }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<SecretKind> {
        scan(text).into_iter().map(|m| m.kind).collect()
    }

    /// Planted secrets, §11.2's requirement. Every value here is synthetic:
    /// structurally valid, deliberately not a live credential.
    #[test]
    fn planted_secrets_are_all_found() {
        let cases: &[(&str, SecretKind)] = &[
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIEow==\n-----END RSA PRIVATE KEY-----",
                SecretKind::PrivateKeyBlock,
            ),
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----",
                SecretKind::PrivateKeyBlock,
            ),
            (
                "error: using AKIAIOSFODNN7EXAMPLE failed",
                SecretKind::AwsAccessKeyId,
            ),
            ("ASIAY34FZKBOKMUTVV7A expired", SecretKind::AwsAccessKeyId),
            (
                "remote: ghp_1234567890abcdefghijklmnopqrstuvwxyzAB rejected",
                SecretKind::GitHubToken,
            ),
            (
                "ghs_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab",
                SecretKind::GitHubToken,
            ),
            (
                "slack said xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx",
                SecretKind::SlackToken,
            ),
            (
                "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789",
                SecretKind::AnthropicKey,
            ),
            (
                "sk-abcdefghijklmnopqrstuvwxyz0123456789ABCD",
                SecretKind::GenericSkKey,
            ),
            (
                "AIzaSyD-1234567890abcdefghijklmnopqrstu",
                SecretKind::GoogleApiKey,
            ),
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk",
                SecretKind::JsonWebToken,
            ),
            (
                "fatal: could not read from https://alice:hunter2swordfish@github.com/x/y.git",
                SecretKind::UrlCredentials,
            ),
            (
                "DATABASE_PASSWORD=s3cr3t-p4ssw0rd-value",
                SecretKind::AssignedSecret,
            ),
            (
                r#"  "api_key": "abcd1234efgh5678ijkl""#,
                SecretKind::AssignedSecret,
            ),
            (
                "running with --token ghijklmnop12345678",
                SecretKind::AssignedSecret,
            ),
        ];

        for (text, expected) in cases {
            let found = kinds(text);
            assert!(
                found.contains(expected),
                "planted {expected:?} was not detected in {text:?}; found {found:?}"
            );
        }
    }

    #[test]
    fn redaction_removes_the_secret_and_keeps_the_diagnosis() {
        let line = "remote: rejected because ghp_1234567890abcdefghijklmnopqrstuvwxyzAB is revoked";
        let redacted = redact(line);

        assert!(
            !redacted
                .text
                .contains("ghp_1234567890abcdefghijklmnopqrstuvwxyzAB"),
            "the secret survived redaction: {}",
            redacted.text
        );
        assert!(redacted.text.contains("[REDACTED:github-token]"));
        // The excerpt is what a human or a repair agent reads. Redacting the
        // whole line would destroy the reason the excerpt exists.
        assert!(redacted.text.starts_with("remote: rejected because "));
        assert!(redacted.text.ends_with(" is revoked"));
        assert_eq!(redacted.kinds, vec![SecretKind::GitHubToken]);
    }

    #[test]
    fn an_assignment_keeps_its_key_and_loses_its_value() {
        let redacted = redact("DATABASE_PASSWORD=s3cr3t-p4ssw0rd-value");
        assert_eq!(
            redacted.text,
            "DATABASE_PASSWORD=[REDACTED:assigned-secret]"
        );
    }

    #[test]
    fn several_secrets_on_one_line_are_all_redacted() {
        let redacted =
            redact("a AKIAIOSFODNN7EXAMPLE b ghp_1234567890abcdefghijklmnopqrstuvwxyzAB c");
        assert_eq!(
            redacted.text,
            "a [REDACTED:aws-access-key-id] b [REDACTED:github-token] c"
        );
        assert_eq!(
            redacted.kinds,
            vec![SecretKind::AwsAccessKeyId, SecretKind::GitHubToken]
        );
    }

    #[test]
    fn ordinary_build_output_is_left_alone() {
        // Over-redaction is a real cost: the excerpt is what a repair agent
        // reads, and a redacted compiler error is a wasted attempt.
        let clean = [
            "   Compiling conductor-run v0.1.0 (/Users/x/Conductor/crates/conductor-run)",
            "error[E0433]: cannot find module or crate `blake3` in this scope",
            "test verify::classify::tests::exit_zero_is_pass ... ok",
            "test result: ok. 27 passed; 0 failed; 0 ignored",
            "warning: unused variable: `token`",
            "note: the next token is a semicolon",
            "https://github.com/anthropics/conductor.git",
            "commit 864ef1e0f0d1a2b3c4d5e6f708192a3b4c5d6e7f",
            "Compiling secret-santa v0.3.1",
        ];
        for line in clean {
            let redacted = redact(line);
            assert!(
                redacted.is_clean(),
                "false positive on ordinary output {line:?}: {:?}",
                redacted.kinds
            );
            assert_eq!(redacted.text, line);
        }
    }

    #[test]
    fn a_placeholder_is_not_a_secret() {
        for line in [
            "PASSWORD=********",
            "api_key: <your-key-here>",
            "TOKEN=${GITHUB_TOKEN}",
            "password: REDACTED",
            "secret = xxxxxxxxxxxx",
            "PASSWORD=",
        ] {
            assert!(
                redact(line).is_clean(),
                "placeholder treated as a secret: {line:?}"
            );
        }
    }

    #[test]
    fn multi_byte_text_around_a_secret_survives_redaction() {
        // Redaction splices byte ranges; a panic on a char boundary here would
        // take down the log writer on the one path that must not fail.
        let redacted = redact("句読点 AKIAIOSFODNN7EXAMPLE ✅ done");
        assert_eq!(redacted.text, "句読点 [REDACTED:aws-access-key-id] ✅ done");
    }

    #[test]
    fn scanning_is_line_oriented_and_finds_secrets_on_every_line() {
        let text = "line one is fine\nAKIAIOSFODNN7EXAMPLE\nalso fine\nPASSWORD=hunter2swordfish\n";
        let redacted = redact(text);
        assert_eq!(
            redacted.kinds,
            vec![SecretKind::AwsAccessKeyId, SecretKind::AssignedSecret]
        );
        assert!(redacted.text.contains("line one is fine"));
        assert!(redacted.text.contains("also fine"));
        assert!(!redacted.text.contains("hunter2swordfish"));
    }

    #[test]
    fn the_documented_blind_spots_are_real_and_stated() {
        // If one of these ever starts being detected, the honest move is to
        // move it out of NOT_DETECTED rather than to leave the list stale.
        assert!(redact("Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MA==").is_clean());
        assert!(!NOT_DETECTED.is_empty());
    }
}
