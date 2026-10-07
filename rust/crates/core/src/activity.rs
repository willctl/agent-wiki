//! The activity log (log/YYYY/YYYY-MM-DD.md) as structured entries, for the window (src/activity.mjs).

use crate::text::to_lf;
use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

static WIKILINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([a-z0-9][a-z0-9-]{0,79})(?:\|[^\]]*)?\]\]").unwrap());
static FULL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^## ([0-9]{2}:[0-9]{2}) · ([^·]+?) · (.+)$").unwrap());
static COMPACT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^- ([0-9]{2}:[0-9]{2}) · ([^·]+?) · (.+)$").unwrap());
static DAY_HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^# [0-9]{4}-[0-9]{2}-[0-9]{2}$").unwrap());
static NOTE_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"note:([0-9]{4}-[0-9]{2}-[0-9]{2}_[0-9]{2}-[0-9]{2}-[0-9]{2}-[0-9]{3}-[a-z0-9]{6})").unwrap());

/// The page slugs a text links to with [[slug]] or [[slug|text]], in order, once each.
pub fn links_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in WIKILINK.captures_iter(text) {
        let s = c[1].to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub date: String,
    pub time: String,
    pub app: String,
    pub title: String,
    pub tags: Vec<String>,
    pub pages: Vec<String>,
    pub body: String,
    /// The ids of the notes the entry was written from (its "sources:" line).
    pub notes: Vec<String>,
    /// The curator batch that wrote the entry (its "<!-- curator batch ... -->" marker), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
    pub compact: bool,
}

/// One day of the log as entries, newest first.
pub fn parse_log_day(date: &str, text: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    let mut cur: Option<(Entry, Vec<String>)> = None;
    let mut since_marker = 0; // entries before this index belong to an earlier batch marker
    let close = |cur: &mut Option<(Entry, Vec<String>)>, out: &mut Vec<Entry>| {
        if let Some((mut e, lines)) = cur.take() {
            e.body = lines.join("\n").trim().to_string();
            out.push(e);
        }
    };
    for raw in to_lf(text).split('\n') {
        let line = raw.trim_end();
        if let Some(c) = FULL.captures(line) {
            close(&mut cur, &mut out);
            cur = Some((
                Entry {
                    date: date.into(),
                    time: c[1].into(),
                    app: c[2].trim().into(),
                    title: c[3].trim().into(),
                    tags: vec![],
                    pages: vec![],
                    body: String::new(),
                    notes: vec![],
                    batch: None,
                    compact: false,
                },
                vec![],
            ));
            continue;
        }
        if let Some(c) = COMPACT.captures(line) {
            close(&mut cur, &mut out);
            out.push(Entry {
                date: date.into(),
                time: c[1].into(),
                app: c[2].trim().into(),
                title: c[3].trim().into(),
                tags: vec![],
                pages: links_in(&c[3]),
                body: String::new(),
                notes: vec![],
                batch: None,
                compact: true,
            });
            continue;
        }
        let t = line.trim();
        if (t.starts_with("<!--") && t.ends_with("-->")) || DAY_HEADING.is_match(line) {
            close(&mut cur, &mut out);
            if let Some(b) = t.strip_prefix("<!-- curator batch ").and_then(|r| r.strip_suffix(" -->")) {
                for e in &mut out[since_marker..] {
                    e.batch = Some(b.trim().to_string());
                }
            }
            if t.starts_with("<!--") {
                since_marker = out.len();
            }
            continue;
        }
        let Some((entry, lines)) = cur.as_mut() else { continue };
        // "tags:", "pages:" and "sources:" lines come first, before any body text.
        let head = !lines.iter().any(|l| !l.trim().is_empty());
        if head {
            if let Some(v) = line.strip_prefix("tags: ") {
                entry.tags = v.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
                continue;
            }
            if let Some(v) = line.strip_prefix("pages: ") {
                entry.pages = links_in(v);
                continue;
            }
            if let Some(v) = line.strip_prefix("sources: ") {
                entry.notes = NOTE_REF.captures_iter(v).map(|c| c[1].to_string()).collect();
                continue;
            }
        }
        lines.push(line.to_string());
    }
    close(&mut cur, &mut out);
    for e in out.iter_mut().filter(|e| !e.compact) {
        for l in links_in(&e.body) {
            if !e.pages.contains(&l) {
                e.pages.push(l);
            }
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_names_its_source_notes() {
        let day = "# 2026-10-03\n\n## 15:22 · codex · Filed\n\ntags: a  \npages: [[x]]  \nsources: note:2026-10-03_15-22-10-118-a1b2c3, note:2026-10-03_15-23-00-000-bbbbbb\n\nBody.\n";
        let e = &parse_log_day("2026-10-03", day)[0];
        assert_eq!(e.notes, ["2026-10-03_15-22-10-118-a1b2c3", "2026-10-03_15-23-00-000-bbbbbb"]);
        assert_eq!(e.pages, ["x"]);
        assert_eq!(e.body, "Body.");
        assert_eq!(e.batch, None);
    }

    #[test]
    fn entries_know_their_curator_batch() {
        let day = "# 2026-10-03\n\n## 09:00 · codex · Earlier, by hand\n\nText.\n\n## 15:22 · codex · Filed\n\nBody.\n\n- 15:23 · curator · Page updated: X [[x]]\n\n<!-- curator batch 2026-10-03_15-23-00-000-abcdef -->\n\n- 16:00 · window · Reverted the curator's change: X [[x]]\n";
        let e = parse_log_day("2026-10-03", day);
        let by_title = |t: &str| e.iter().find(|x| x.title.starts_with(t)).unwrap().batch.clone();
        assert_eq!(by_title("Page updated").as_deref(), Some("2026-10-03_15-23-00-000-abcdef"));
        assert_eq!(by_title("Filed").as_deref(), Some("2026-10-03_15-23-00-000-abcdef"));
        assert_eq!(by_title("Earlier"), Some("2026-10-03_15-23-00-000-abcdef".into()), "no marker between them: the same chunk as far as the file says");
        assert_eq!(by_title("Reverted"), None);
    }
}
