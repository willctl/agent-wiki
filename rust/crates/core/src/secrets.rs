//! The secret guard: what the wiki refuses to store and what the logs redact (src/secrets.mjs).
//! The patterns use look-around, so they run on fancy-regex; the texts are short.

use fancy_regex::{Captures, Regex};
use std::sync::LazyLock;

struct Pattern {
    kind: &'static str,
    re: Regex,
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    [
        ("a private key block", r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----"),
        ("an API key (sk-...)", r"(?<![A-Za-z0-9])sk-(?:ant-)?[A-Za-z0-9_-]{20,}"),
        ("a GitHub token", r"(?<![A-Za-z0-9])(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{20,})"),
        ("a Slack token", r"(?<![A-Za-z0-9])xox[abposr]-[A-Za-z0-9-]{10,}"),
        ("an AWS access key ID", r"(?<![A-Za-z0-9])(?:AKIA|ASIA)[0-9A-Z]{16}(?![A-Za-z0-9])"),
        ("a Google API key", r"(?<![A-Za-z0-9])AIza[0-9A-Za-z_-]{35}"),
        ("a JSON Web Token", r"(?<![A-Za-z0-9])eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"),
        ("an npm token", r"(?<![A-Za-z0-9])npm_[A-Za-z0-9]{36}(?![A-Za-z0-9])"),
        ("a US Social Security number", r"(?<![\d-])(?!000|666|9\d\d)\d{3}-(?!00)\d{2}-(?!0000)\d{4}(?![\d-])"),
    ]
    .into_iter()
    .map(|(kind, p)| Pattern { kind, re: Regex::new(p).expect("secret pattern") })
    .collect()
});

// "password: X", "api key = X", "token: X": only when X is 12+ chars with a letter AND a digit.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(?:pass(?:word|wd|phrase)?|pwd|secret|client[ _-]?secret|api[ _-]?key|apikey|access[ _-]?key|(?:auth|access|bearer|refresh)[ _-]?token|token)\b["']?\s*[:=]\s*["']?([^\s"'`,;)]+)"#,
    )
    .expect("assignment pattern")
});

static CARDS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [r"(?<![\d.])\d{15,16}(?![\d.]?\d)", r"(?<![\d-])\d{4}([ -])\d{4}\1\d{4}\1\d{3,4}(?![\d-])", r"(?<![\d-])\d{4}([ -])\d{6}\1\d{5}(?![\d-])"]
        .into_iter()
        .map(|p| Regex::new(p).expect("card pattern"))
        .collect()
});

static PRIVATE_KEY_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|$)").expect("key block"));

fn luhn(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut alt = false;
    for c in digits.chars().rev() {
        let mut n = c as u32 - '0' as u32;
        if alt {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
        alt = !alt;
    }
    sum.is_multiple_of(10)
}

fn is_card(m: &str) -> bool {
    let d: String = m.chars().filter(|c| c.is_ascii_digit()).collect();
    (d.len() == 15 || d.len() == 16) && matches!(d.as_bytes()[0], b'3'..=b'6') && luhn(&d)
}

fn strong(v: &str) -> bool {
    v.chars().count() >= 12 && v.chars().any(|c| c.is_ascii_alphabetic()) && v.chars().any(|c| c.is_ascii_digit())
}

/// A description of the first secret-looking thing in `text`, or None.
pub fn find_secret(text: &str) -> Option<&'static str> {
    if text.is_empty() {
        return None;
    }
    for p in PATTERNS.iter() {
        if p.re.is_match(text).unwrap_or(false) {
            return Some(p.kind);
        }
    }
    for c in ASSIGNMENT.captures_iter(text).flatten() {
        if c.get(1).is_some_and(|v| strong(v.as_str())) {
            return Some("a credential assignment (e.g. \"password: ...\")");
        }
    }
    for re in CARDS.iter() {
        for m in re.find_iter(text).flatten() {
            if is_card(m.as_str()) {
                return Some("a payment card number");
            }
        }
    }
    None
}

/// Replaces everything find_secret would flag with [REDACTED]. Used before anything is written to a log.
pub fn redact_secrets(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut s = PRIVATE_KEY_BLOCK.replace_all(text, "[REDACTED]").into_owned();
    for p in PATTERNS.iter() {
        s = p.re.replace_all(&s, "[REDACTED]").into_owned();
    }
    s = ASSIGNMENT
        .replace_all(&s, |c: &Captures<'_, str>| {
            let whole = c.get(0).map_or("", |m| m.as_str());
            match c.get(1) {
                Some(v) if strong(v.as_str()) => whole.replacen(v.as_str(), "[REDACTED]", 1),
                _ => whole.to_string(),
            }
        })
        .into_owned();
    for re in CARDS.iter() {
        s = re
            .replace_all(&s, |c: &Captures<'_, str>| {
                let m = c.get(0).map_or("", |m| m.as_str());
                if is_card(m) { "[REDACTED]".to_string() } else { m.to_string() }
            })
            .into_owned();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_redacts() {
        assert_eq!(find_secret("key AKIAIOSFODNN7EXAMPLE here"), Some("an AWS access key ID"));
        assert_eq!(find_secret("the token refreshes every 8 hours"), None);
        assert_eq!(find_secret("password: hunter2hunter2x9"), Some("a credential assignment (e.g. \"password: ...\")"));
        assert_eq!(find_secret("card 4111 1111 1111 1111"), Some("a payment card number"));
        assert_eq!(find_secret("ssn 123-45-6789"), Some("a US Social Security number"));
        assert_eq!(find_secret("order 123-45-67890"), None);
        assert_eq!(redact_secrets("use AKIAIOSFODNN7EXAMPLE now"), "use [REDACTED] now");
        assert_eq!(redact_secrets("api_key=abc123def456ghi"), "api_key=[REDACTED]"); // gitleaks:allow -- synthetic rejection fixture
    }
}
