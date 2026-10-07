//! Time and text helpers shared by every module (src/text.mjs).

use chrono::{DateTime, Datelike, Local, Timelike};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;

pub type Time = DateTime<Local>;

pub fn now() -> Time {
    Local::now()
}

/// Local time with offset, e.g. 2026-10-01T13:42:05-05:00.
pub fn local_iso(d: &Time) -> String {
    let off = d.offset().local_minus_utc() / 60;
    let a = off.abs();
    format!("{}T{:02}:{:02}:{:02}{}{:02}:{:02}", local_date(d), d.hour(), d.minute(), d.second(), if off >= 0 { '+' } else { '-' }, a / 60, a % 60)
}

/// local_iso with milliseconds before the offset (the request log's `t`).
pub fn local_iso_ms(d: &Time) -> String {
    let iso = local_iso(d);
    let (head, off) = iso.split_at(iso.len() - 6);
    format!("{head}.{:03}{off}", d.timestamp_subsec_millis())
}

pub fn local_date(d: &Time) -> String {
    format!("{}-{:02}-{:02}", d.year(), d.month(), d.day())
}

pub fn local_hm(d: &Time) -> String {
    format!("{:02}:{:02}", d.hour(), d.minute())
}

/// Windows-safe (no colons) stamp for .history file names and note ids.
pub fn history_stamp(d: &Time) -> String {
    format!("{}_{:02}-{:02}-{:02}-{:03}", local_date(d), d.hour(), d.minute(), d.second(), d.timestamp_subsec_millis())
}

/// Milliseconds since the epoch.
pub fn epoch_ms(d: &Time) -> i64 {
    d.timestamp_millis()
}

pub fn now_ms() -> i64 {
    epoch_ms(&now())
}

/// Parses an ISO-8601 timestamp with offset (what local_iso writes) to epoch ms.
pub fn parse_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis());
    }
    // A bare date or a local date-time, as Date.parse reads them.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp_millis());
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, f) {
            return t.and_local_timezone(Local).single().map(|d| d.timestamp_millis());
        }
    }
    None
}

pub fn from_ms(ms: i64) -> Time {
    DateTime::from_timestamp_millis(ms).unwrap_or_default().with_timezone(&Local)
}

/// CRLF and CR to LF.
pub fn to_lf(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    s.replace("\r\n", "\n").replace('\r', "\n")
}

static NEWLINE_RUN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\s*\n\s*").unwrap());

/// One line: newlines (with the whitespace around them) become one space; trimmed.
pub fn one_line(s: &str) -> String {
    NEWLINE_RUN.replace_all(&to_lf(s), " ").trim().to_string()
}

/// Short content hash used for optimistic concurrency (base hashes) and idempotency keys.
pub fn hash_text(s: &str, len: usize) -> String {
    let digest = Sha256::digest(to_lf(s).as_bytes());
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        hex.push_str(&format!("{b:02x}"));
    }
    hex.truncate(len);
    hex
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lines that are only an HTML comment (curator batch markers).
pub fn is_marker_line(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("<!--") && t.ends_with("-->")
}

pub fn strip_markers(text: &str) -> String {
    text.split('\n').filter(|l| !is_marker_line(l)).collect::<Vec<_>>().join("\n")
}

pub fn display_path(p: &str) -> String {
    p.replace('\\', "/")
}

/// Random lowercase hex of `n` characters.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n.div_ceil(2)];
    let _ = getrandom::fill(&mut buf);
    let mut s: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    s.truncate(n);
    s
}

/// A random version-4 UUID.
pub fn random_uuid() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Truncates to at most `max` characters (not bytes).
pub fn take_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

// ---------------------------------------------------------------- collation

/// The order JavaScript's `a.localeCompare(b)` gives in the root locale (ICU) for the characters
/// that occur in slugs, titles and paths: whitespace, then punctuation in CLDR root order, then
/// symbols, digits, letters; case and accents only break ties, lowercase first. The Node and Rust
/// programs must order the index the same way, or each would keep rewriting the other's index.md.
pub fn collate_cmp(a: &str, b: &str) -> Ordering {
    let ka: Vec<(u32, u32, u32)> = a.chars().map(weights).collect();
    let kb: Vec<(u32, u32, u32)> = b.chars().map(weights).collect();
    for level in 0..3 {
        for (x, y) in ka.iter().zip(kb.iter()) {
            let (wx, wy) = match level {
                0 => (x.0, y.0),
                1 => (x.1, y.1),
                _ => (x.2, y.2),
            };
            match wx.cmp(&wy) {
                Ordering::Equal => continue,
                o => return o,
            }
        }
        if level == 0 && ka.len() != kb.len() {
            return ka.len().cmp(&kb.len());
        }
    }
    a.cmp(b)
}

const PUNCT_ORDER: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

/// (primary, secondary, tertiary) weights for one character.
fn weights(c: char) -> (u32, u32, u32) {
    if c.is_whitespace() {
        return (100 + c as u32 % 50, 0, 0);
    }
    if let Some(i) = PUNCT_ORDER.find(c) {
        return (200 + i as u32, 0, 0);
    }
    if c.is_ascii_digit() {
        return (1000 + (c as u32 - '0' as u32), 0, 0);
    }
    if c.is_alphabetic() {
        // Base letter without accents, case-folded, at the primary level.
        let base = strip_accents(c);
        let lower = base.to_lowercase().next().unwrap_or(base);
        let secondary = if base == c { 0 } else { 1 + c as u32 % 1000 };
        let tertiary = if c.is_uppercase() { 1 } else { 0 };
        return (2000 + lower as u32, secondary, tertiary);
    }
    // Other symbols: after the known punctuation, before digits, by code point.
    (500 + (c as u32 % 400), 0, 0)
}

fn strip_accents(c: char) -> char {
    use unicode_normalization::UnicodeNormalization;
    let mut it = std::iter::once(c).nfd().filter(|ch| !('\u{0300}'..='\u{036f}').contains(ch));
    it.next().unwrap_or(c)
}

/// A line diff: (' ' kept, '-' removed, '+' added, line), by longest common subsequence. Very long
/// texts (over 3,000 lines) come back as all removed, then all added.
pub fn line_diff<'a>(old: &'a str, new: &'a str) -> Vec<(char, &'a str)> {
    let a: Vec<&str> = if old.is_empty() { vec![] } else { old.split('\n').collect() };
    let b: Vec<&str> = if new.is_empty() { vec![] } else { new.split('\n').collect() };
    if a.len() > 3000 || b.len() > 3000 {
        return a.iter().map(|l| ('-', *l)).chain(b.iter().map(|l| ('+', *l))).collect();
    }
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![0u16; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * (m + 1) + j] = if a[i] == b[j] { lcs[(i + 1) * (m + 1) + j + 1] + 1 } else { lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1]) };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, vec![]);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            out.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[(i + 1) * (m + 1) + j] >= lcs[i * (m + 1) + j + 1]) {
            out.push(('-', a[i]));
            i += 1;
        } else {
            out.push(('+', b[j]));
            j += 1;
        }
    }
    out
}

/// Remote images become plain links, because an image an app renders is fetched without anyone
/// clicking it: a known way to leak data through a URL. Local and data: images are left alone.
pub fn defang_remote_images(text: &str) -> std::borrow::Cow<'_, str> {
    use regex::Regex;
    use std::sync::LazyLock;
    static MD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"!\[([^\]\n]*)\]\(\s*<?((?:[a-zA-Z][a-zA-Z0-9+.-]*:)?//[^)\s>]+)>?(?:\s+(?:"[^"\n]*"|'[^'\n]*'))?\s*\)"#).unwrap());
    static REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!\[([^\]\n]*)\]\[([^\]\n]*)\]").unwrap());
    static IMG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)<img\b[^>]*?\bsrc\s*=\s*["']?((?:[a-z][a-z0-9+.-]*:)?//[^"'\s>]+)["']?[^>]*>"#).unwrap());
    if !text.contains("![") && !text.to_ascii_lowercase().contains("<img") {
        return std::borrow::Cow::Borrowed(text);
    }
    let alt = |a: &str| if a.trim().is_empty() { "image".to_string() } else { a.to_string() };
    let t = MD.replace_all(text, |c: &regex::Captures| format!("[{}]({})", alt(&c[1]), &c[2]));
    let t = REF.replace_all(&t, |c: &regex::Captures| format!("[{}][{}]", alt(&c[1]), &c[2])).into_owned();
    let t = IMG.replace_all(&t, |c: &regex::Captures| format!("[image]({})", &c[1])).into_owned();
    std::borrow::Cow::Owned(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_images_become_links() {
        assert_eq!(defang_remote_images("see ![logo](https://evil.example/x.png?d=secret \"t\") here"), "see [logo](https://evil.example/x.png?d=secret) here");
        assert_eq!(defang_remote_images("![](//cdn.example/a.gif)"), "[image](//cdn.example/a.gif)");
        assert_eq!(defang_remote_images("![a][ref]\n\n[ref]: https://x.example/p.png"), "[a][ref]\n\n[ref]: https://x.example/p.png");
        assert_eq!(defang_remote_images(r#"<IMG alt=x SRC="https://x.example/p.png?q=1">"#), "[image](https://x.example/p.png?q=1)");
        assert_eq!(defang_remote_images("![local](images/a.png) and [link](https://ok.example)"), "![local](images/a.png) and [link](https://ok.example)");
        assert!(matches!(defang_remote_images("plain"), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn line_diffs() {
        let d: Vec<String> = line_diff("a\nb\nc", "a\nB\nc\nd").into_iter().map(|(k, l)| format!("{k}{l}")).collect();
        assert_eq!(d, [" a", "-b", "+B", " c", "+d"]);
        assert_eq!(line_diff("", "x"), [('+', "x")]);
    }

    /// Expected orders are what Node 24 prints for `[...].sort((a, b) => a.localeCompare(b))`.
    #[test]
    fn collation_like_locale_compare() {
        let mut v = vec!["atlas", "Agent Wiki", "agent-wiki", "agentwiki", "b", "A", "a", "10", "2", "Zeta"];
        v.sort_by(|a, b| collate_cmp(a, b));
        assert_eq!(v, vec!["10", "2", "a", "A", "Agent Wiki", "agent-wiki", "agentwiki", "atlas", "b", "Zeta"]);
        let mut w = vec![
            "Home lab",
            "Harbor / Tailscale VPN for Atlas",
            "harbor-vpn",
            "home-lab",
            "Team notes",
            "team-notes",
            "Sundial",
            "Orchard",
            "orchard",
            "sundial",
            "Éclair",
            "eclair",
            "Eclair",
            "e_x",
            "e-x",
            "e.x",
            "e x",
            "e(x",
            "e1",
            "eA",
            "ea",
        ];
        w.sort_by(|a, b| collate_cmp(a, b));
        assert_eq!(
            w,
            vec![
                "e x",
                "e_x",
                "e-x",
                "e.x",
                "e(x",
                "e1",
                "ea",
                "eA",
                "eclair",
                "Eclair",
                "Éclair",
                "Harbor / Tailscale VPN for Atlas",
                "harbor-vpn",
                "Home lab",
                "home-lab",
                "orchard",
                "Orchard",
                "sundial",
                "Sundial",
                "Team notes",
                "team-notes",
            ]
        );
    }

    #[test]
    fn one_line_collapses() {
        assert_eq!(one_line("  a\n  b \n\n c "), "a b c");
        assert_eq!(one_line("x\r\ny"), "x y");
    }
}
