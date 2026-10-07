//! wiki_search (M5): BM25 over sections. Each page's intro and each of its ## and ### sections, each
//! log entry and each note is a document; a file's score is its best section's, with a little credit
//! for other matching sections, so results stay one per file and cite the section that matched.
//!
//! Terms are lowercased words with a light English stemmer (plurals, -ed, -ing), and compound words
//! ("ledgerline-erp", "api.example.test/v3") also count as their parts. Title, tags, the summary and
//! the section heading weigh more than body text (BM25F-style). Then: a page boost, filed notes
//! ranked below everything else (their facts are on a page or in the log already), and a mild
//! recency boost. No model is involved.

use crate::inbox::{self, Note};
use crate::text::*;
use crate::wiki::{self, Hit, Result, list_log_files, list_pages, tokenize};
use regex::Regex;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

const K1: f64 = 1.2;
const B: f64 = 0.75;
/// Field weights: a term in the title counts as this many body occurrences.
const W_TITLE: f64 = 3.0;
const W_TAGS: f64 = 2.0;
const W_SUMMARY: f64 = 2.0;
const W_HEADING: f64 = 2.0;
const PAGE_BOOST: f64 = 1.3;
const FILED_NOTE: f64 = 0.7;
/// How much the other matching sections of a file add to its best one.
const OTHER_SECTIONS: f64 = 0.15;
/// Coordination: a section's score is scaled by (query words it has / all query words) to this power.
const COORD: f64 = 1.0;
/// Added to the score of a log entry or note from a date the query names.
const DATE_BONUS: f64 = 6.0;

/// Debug builds only: AW_SEARCH_TUNE="k1=1.5,coord=0.5,..." overrides the constants above,
/// for tuning against the eval. Release builds always use the constants.
fn tune(name: &str, default: f64) -> f64 {
    #[cfg(debug_assertions)]
    if let Ok(s) = std::env::var("AW_SEARCH_TUNE") {
        for kv in s.split(',') {
            if let Some((k, v)) = kv.split_once('=')
                && k.trim() == name
                && let Ok(x) = v.trim().parse()
            {
                return x;
            }
        }
    }
    let _ = name;
    default
}

/// The dates a query names, as YYYY-MM-DD: "2026-10-01", "October 1", "Oct 1st, 2026", "1 October".
/// Without a year: this year, or last year when that date is more than a month ahead.
pub fn query_dates(query: &str) -> Vec<String> {
    static ISO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([0-9]{4})-([0-9]{2})-([0-9]{2})\b").unwrap());
    static MD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\.?\s+([0-9]{1,2})(?:st|nd|rd|th)?\b(?:,?\s+([0-9]{4}))?").unwrap());
    static DM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b([0-9]{1,2})(?:st|nd|rd|th)?\s+(?:of\s+)?(jan|feb|mar|apr|may|jun|jul|aug|sep|oct|nov|dec)[a-z]*\b(?:,?\s+([0-9]{4}))?").unwrap());
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let today = now();
    let mut out: Vec<String> = ISO.captures_iter(query).map(|c| format!("{}-{}-{}", &c[1], &c[2], &c[3])).collect();
    let mut add = |mon: &str, day: &str, year: Option<&str>| {
        use chrono::Datelike;
        let m = MONTHS.iter().position(|x| mon.to_lowercase().starts_with(x)).map(|i| i as u32 + 1);
        let (Some(m), Ok(d)) = (m, day.parse::<u32>()) else { return };
        let y = match year.and_then(|y| y.parse::<i32>().ok()) {
            Some(y) => y,
            None => {
                let this = today.year();
                let ahead = chrono::NaiveDate::from_ymd_opt(this, m, d).is_some_and(|dt| dt > today.date_naive() + chrono::Duration::days(31));
                if ahead { this - 1 } else { this }
            }
        };
        if chrono::NaiveDate::from_ymd_opt(y, m, d).is_some() {
            out.push(format!("{y:04}-{m:02}-{d:02}"));
        }
    };
    for c in MD.captures_iter(query) {
        add(&c[1], &c[2], c.get(3).map(|x| x.as_str()));
    }
    for c in DM.captures_iter(query) {
        add(&c[2], &c[1], c.get(3).map(|x| x.as_str()));
    }
    out.sort();
    out.dedup();
    out
}

/// Optional limits on what a search returns (wiki_search's since/until/app).
#[derive(Clone, Debug, Default)]
pub struct Filters {
    /// YYYY-MM-DD, inclusive: log entries and notes by their date, pages by when they were last updated.
    pub since: Option<String>,
    pub until: Option<String>,
    /// Only log entries and notes from this app (pages have no app, so they are left out).
    pub app: Option<String>,
}

/// One searchable section.
struct Unit {
    kind: &'static str,
    rel: String,
    target: String,
    label: String,
    /// Anchor of the section ("" for a page's intro, a whole note).
    section: String,
    heading: String,
    title: String,
    tags: String,
    summary: String,
    body: String,
    time: i64,
    date: String,
    app: String,
}

/// A heading's anchor: lowercase letters and digits, words joined by hyphens.
pub fn anchor(heading: &str) -> String {
    let mut out = String::new();
    for c in heading.to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(80).collect()
}

/// A page body as (anchor, heading, text): the intro, then each ## and ### section. Headings inside
/// code fences are text. Anchors are unique within the page (-2, -3 ...).
pub fn page_sections(body: &str) -> Vec<(String, String, String)> {
    static H: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(#{2,3})\s+(.+?)\s*#*\s*$").unwrap());
    let mut out: Vec<(String, String, String)> = vec![(String::new(), String::new(), String::new())];
    let mut fence = false;
    for line in body.split('\n') {
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            fence = !fence;
        }
        if !fence && let Some(c) = H.captures(line) {
            let heading = c[2].trim().to_string();
            let base = anchor(&heading);
            let mut a = base.clone();
            let mut n = 2;
            while out.iter().any(|(x, _, _)| *x == a) {
                a = format!("{base}-{n}");
                n += 1;
            }
            out.push((a, heading, String::new()));
            continue;
        }
        let last = out.last_mut().unwrap();
        last.2.push_str(line);
        last.2.push('\n');
    }
    out.retain(|(a, _, t)| !a.is_empty() || !t.trim().is_empty());
    out
}

/// A log day as (anchor, heading, app, text) per "## HH:MM · app · title" entry (the heading as written,
/// the anchor from the time and title); lines after an entry, its compact page-change lines too, belong to it.
pub fn log_sections(text: &str) -> Vec<(String, String, String, String)> {
    static E: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^## ([0-9]{2}):([0-9]{2}) · ([^·]+?) · (.+)$").unwrap());
    let mut out: Vec<(String, String, String, String)> = vec![];
    for line in text.split('\n') {
        if let Some(c) = E.captures(line) {
            let heading = format!("{}:{} · {} · {}", &c[1], &c[2], c[3].trim(), c[4].trim());
            let base = anchor(&format!("{}:{} {}", &c[1], &c[2], c[4].trim()));
            let mut a = base.clone();
            let mut n = 2;
            while out.iter().any(|(x, _, _, _)| *x == a) {
                a = format!("{base}-{n}");
                n += 1;
            }
            out.push((a, heading, c[3].trim().to_string(), String::new()));
            continue;
        }
        if is_marker_line(line) {
            continue;
        }
        if let Some(last) = out.last_mut() {
            last.3.push_str(line);
            last.3.push('\n');
        }
    }
    out
}

/// A light English stemmer: the same word in different forms should meet ("retries" and "retry",
/// "moved" and "move", "warehouses" and "warehouse"). Numbers, paths and short words stay as they are.
pub fn stem(w: &str) -> String {
    let n = w.chars().count();
    if n <= 3 || w.chars().any(|c| c.is_ascii_digit() || matches!(c, '.' | '/' | '-' | '_' | ':')) {
        return w.to_string();
    }
    let cut = |k: usize| w[..w.len() - k].to_string();
    if let Some(s) = w.strip_suffix("ies").filter(|s| s.len() >= 2) {
        return format!("{s}y");
    }
    if w.ends_with("sses") || w.ends_with("xes") || w.ends_with("ches") || w.ends_with("shes") || w.ends_with("zes") {
        return cut(2);
    }
    if n > 4 && w.ends_with("ing") && w.len() - 3 >= 3 {
        return undouble(cut(3));
    }
    if n > 4 && w.ends_with("ed") && !w.ends_with("eed") && w.len() - 2 >= 3 {
        return undouble(cut(2));
    }
    if w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") && !w.ends_with("is") {
        return cut(1);
    }
    w.to_string()
}

/// "stopped" -> "stopp" -> "stop"; "moved" -> "mov" -> "mov" (matches "move" -> "mov"? no: see e-drop).
fn undouble(s: String) -> String {
    let b = s.as_bytes();
    if b.len() >= 3 && b[b.len() - 1] == b[b.len() - 2] && !matches!(b[b.len() - 1], b'l' | b's' | b'z') && b[b.len() - 1].is_ascii_alphabetic() {
        return s[..s.len() - 1].to_string();
    }
    s
}

/// The final form both sides compare: stemmed, and without a trailing "e" so "move"/"moved"/"moving" meet.
fn norm_term(w: &str) -> String {
    let s = stem(w);
    if s.chars().count() > 3 && s.ends_with('e') && !s.chars().any(|c| c.is_ascii_digit()) { s[..s.len() - 1].to_string() } else { s }
}

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}][\p{L}\p{N}._/-]*").unwrap());
static PART: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]+").unwrap());

/// Adds a text's terms to `tf` with a weight: each word, and the parts of compound words.
fn add_terms(text: &str, weight: f64, tf: &mut HashMap<String, f64>, len: &mut f64) {
    let lower = text.to_lowercase();
    for m in TOKEN.find_iter(&lower) {
        let t = m.as_str().trim_end_matches(['.', '_', '/', '-']);
        if t.is_empty() {
            continue;
        }
        *tf.entry(norm_term(t)).or_default() += weight;
        *len += 1.0;
        if t.contains(['.', '_', '/', '-']) {
            for p in PART.find_iter(t) {
                if p.as_str().chars().count() > 1 || p.as_str().chars().all(|c| c.is_ascii_digit()) {
                    *tf.entry(norm_term(p.as_str())).or_default() += weight * 0.5;
                }
            }
        }
    }
}

fn units(wiki_dir: &Path, scope: &str) -> Result<Vec<Unit>> {
    let mut out = vec![];
    if scope == "all" || scope == "pages" {
        for p in list_pages(wiki_dir)? {
            let date = p.updated.get(..10).map(String::from).unwrap_or_else(|| local_date(&from_ms(p.time)));
            for (section, heading, text) in page_sections(&p.body) {
                out.push(Unit {
                    kind: "page",
                    rel: p.rel.clone(),
                    target: p.slug.clone(),
                    label: format!("{} [{}]", p.title, p.kind),
                    summary: p.summary.clone(),
                    section,
                    heading,
                    title: format!("{} {}", p.title, p.slug),
                    tags: format!("{} {}", p.tags.join(" "), p.aliases.join(" ")),
                    body: text,
                    time: p.time,
                    date: date.clone(),
                    app: String::new(),
                });
            }
        }
    }
    let note_unit = |n: Note, filed: bool| Unit {
        kind: "note",
        label: format!("{} note from {}, {} {}", if filed { "filed" } else { "pending" }, n.app, n.date, n.time),
        target: format!("note:{}", n.id),
        rel: n.rel.clone(),
        section: String::new(),
        heading: String::new(),
        title: n.title.clone(),
        tags: format!("{} {}", n.tags.join(" "), n.pages.join(" ")),
        summary: String::new(),
        body: format!("{}\n{}", n.title, n.body),
        time: n.ms,
        date: n.date.clone(),
        app: n.app.clone(),
    };
    if scope != "pages" {
        out.extend(inbox::list_notes(wiki_dir).unwrap_or_default().into_iter().map(|n| note_unit(n, false)));
    }
    if scope == "all" || scope == "notes" {
        out.extend(inbox::filed_notes(wiki_dir).into_iter().map(|n| note_unit(n, true)));
    }
    if scope == "all" || scope == "log" {
        for (date, rel, file, len, mtime) in list_log_files(wiki_dir) {
            let Some(text) = crate::readcache::read(&file, len, mtime).map(|t| to_lf(&t)) else { continue };
            let day_end = parse_ms(&format!("{date}T23:59:59")).unwrap_or(0);
            for (section, heading, app, body) in log_sections(&text) {
                let hm = heading.get(..5).unwrap_or("");
                out.push(Unit {
                    kind: "log",
                    rel: rel.clone(),
                    target: date.clone(),
                    label: format!("log {date}"),
                    title: date.clone(),
                    tags: String::new(),
                    summary: String::new(),
                    time: parse_ms(&format!("{date}T{hm}:00")).unwrap_or(day_end),
                    section,
                    heading,
                    body,
                    date: date.clone(),
                    app,
                });
            }
        }
    }
    Ok(out)
}

/** The text a unit is embedded as (embed.rs): what it is (page and section, log day, note) and its words. */
fn embed_text(u: &Unit) -> String {
    let text = match u.kind {
        "page" => {
            let title = u.label.rsplit_once(" [").map_or(u.label.as_str(), |(t, _)| t);
            if u.section.is_empty() { format!("{title}\n{}\n{}\n{}", u.summary, u.tags.trim(), u.body) } else { format!("{title} > {}\n{}", u.heading, u.body) }
        }
        "log" => format!("Log {} {}\n{}", u.date, u.heading, u.body),
        _ => u.body.clone(),
    };
    text.trim().chars().take(6000).collect()
}

/// Every unit's embedding text, for embed::sync.
pub fn embedding_texts(wiki_dir: &Path) -> Result<Vec<String>> {
    Ok(units(wiki_dir, "all")?.iter().map(embed_text).collect())
}

/// Hybrid ranking (docs/search-plan.md, S2): a convex combination of BM25, divided by its best file's
/// score, and the cosine of each file's best section, min-max normalized over the files (Bruch et al.,
/// TOIS 2023), weighted by the settings. A file only the semantic side finds gets its best section.
fn fuse(wiki_dir: &Path, s: &crate::embed::Settings, query: &str, qv: &[f32], all: &[Unit], lexical: Vec<Hit>, words: &[String]) -> Vec<Hit> {
    let mut best: HashMap<&str, (f32, usize)> = HashMap::new();
    for (i, u) in all.iter().enumerate() {
        let Some(v) = crate::embed::vector(wiki_dir, &s.model, &crate::embed::text_key(&s.model, &embed_text(u))) else { continue };
        let c = crate::embed::dot(qv, &v);
        let e = best.entry(u.rel.as_str()).or_insert((f32::MIN, i));
        if c > e.0 {
            *e = (c, i);
        }
    }
    if best.is_empty() {
        return lexical;
    }
    let (lo, hi) = best.values().fold((f32::MAX, f32::MIN), |(lo, hi), (c, _)| (lo.min(*c), hi.max(*c)));
    let lex_max = lexical.iter().map(|h| h.score).fold(0.0, f64::max);
    let w = if crate::embed::question_like(query) { s.weight } else { s.keyword_weight };
    let mut fused: HashMap<String, (f64, Hit)> = lexical.into_iter().map(|h| (h.rel.clone(), ((1.0 - w) * if lex_max > 0.0 { h.score / lex_max } else { 0.0 }, h))).collect();
    for (rel, (c, i)) in best {
        let d = if hi > lo { ((c - lo) / (hi - lo)) as f64 } else { 1.0 };
        if let Some((score, _)) = fused.get_mut(rel) {
            *score += w * d;
            continue;
        }
        let u = &all[i];
        let mut lines = snippets(&u.heading, &u.body, words, 3);
        if lines.is_empty() {
            lines = u.body.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).take(2).map(|l| l.chars().take(200).collect()).collect();
        }
        let hit = Hit { kind: u.kind, rel: u.rel.clone(), target: u.target.clone(), label: u.label.clone(), section: u.section.clone(), heading: u.heading.clone(), score: 0.0, snippets: lines };
        fused.insert(rel.to_string(), (w * d, hit));
    }
    let mut out: Vec<(f64, Hit)> = fused.into_values().collect();
    out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal).then_with(|| collate_cmp(&b.1.rel, &a.1.rel)));
    out.into_iter()
        .map(|(score, mut h)| {
            h.score = (score * 1000.0).round() / 10.0;
            h
        })
        .collect()
}

fn passes(u: &Unit, f: &Filters) -> bool {
    if f.since.as_deref().is_some_and(|s| u.date.as_str() < s) || f.until.as_deref().is_some_and(|s| u.date.as_str() > s) {
        return false;
    }
    match f.app.as_deref().map(wiki::normalize_app) {
        Some(app) => u.kind != "page" && u.app == app,
        None => true,
    }
}

/// Lines of a section that contain a search word, under their heading, for the result list.
fn snippets(heading: &str, body: &str, words: &[String], max: usize) -> Vec<String> {
    static H: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#{2,3}\s+(.+)$").unwrap());
    let mut out = Vec::new();
    for line in body.split('\n') {
        if line.trim().is_empty() || is_marker_line(line) || line.starts_with("sources: note:") || H.is_match(line) {
            continue;
        }
        let lower = line.to_lowercase();
        if !words.iter().any(|t| lower.contains(t.as_str())) {
            continue;
        }
        let t = line.trim();
        let text = if t.chars().count() > 220 { format!("{}...", take_chars(t, 217)) } else { t.to_string() };
        out.push(if heading.is_empty() { text } else { format!("[{heading}] {text}") });
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Several wordings of one need, searched together: each is searched as `search` does, and the
/// files are fused by reciprocal rank (k = 60; Cormack et al., SIGIR 2009), so a file that any
/// wording finds near the top ranks high. Each file keeps the section and snippets of the wording
/// that ranked it best; its score is the fused score times 1000.
pub fn search_many(wiki_dir: &Path, queries: &[String], scope: &str, limit: i64, filters: &Filters) -> Result<Vec<Hit>> {
    if queries.len() == 1 {
        return search(wiki_dir, &queries[0], scope, limit, filters);
    }
    let mut fused: HashMap<String, (f64, usize, Hit)> = HashMap::new();
    for q in queries {
        for (i, h) in search(wiki_dir, q, scope, limit.max(20), filters)?.into_iter().enumerate() {
            let add = 1.0 / (60.0 + i as f64 + 1.0);
            match fused.get_mut(&h.rel) {
                Some((score, best, hit)) => {
                    *score += add;
                    if i < *best {
                        (*best, *hit) = (i, h);
                    }
                }
                None => {
                    fused.insert(h.rel.clone(), (add, i, h));
                }
            }
        }
    }
    let mut hits: Vec<(f64, usize, Hit)> = fused.into_values().collect();
    hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal).then(a.1.cmp(&b.1)).then_with(|| a.2.rel.cmp(&b.2.rel)));
    Ok(hits
        .into_iter()
        .take(limit.max(1) as usize)
        .map(|(score, _, mut h)| {
            h.score = (score * 1000.0 * 10.0).round() / 10.0;
            h
        })
        .collect())
}

pub fn search(wiki_dir: &Path, query: &str, scope: &str, limit: i64, filters: &Filters) -> Result<Vec<Hit>> {
    if tokenize(query).is_empty() {
        return Ok(vec![]);
    }
    // Hybrid when embeddings are on and the query's vector arrives in time (embed.rs); else BM25 as it is.
    // A query that names a date stays lexical: BM25 ranks that day's log first, and embeddings know no dates.
    let semantic = if query_dates(query).is_empty() { crate::embed::query_vector(wiki_dir, query) } else { None };
    search_with(wiki_dir, query, scope, limit, filters, semantic)
}

/// search(), given the query's embedding (or none, for BM25 alone).
fn search_with(wiki_dir: &Path, query: &str, scope: &str, limit: i64, filters: &Filters, semantic: Option<(crate::embed::Settings, Vec<f32>)>) -> Result<Vec<Hit>> {
    let words = tokenize(query);
    if words.is_empty() {
        return Ok(vec![]);
    }
    let all: Vec<Unit> = units(wiki_dir, scope)?.into_iter().filter(|u| passes(u, filters)).collect();
    // Term frequencies per section, fields weighted.
    let mut docs: Vec<(HashMap<String, f64>, f64)> = Vec::with_capacity(all.len());
    for u in &all {
        let (mut tf, mut len) = (HashMap::new(), 0.0);
        add_terms(&u.body, 1.0, &mut tf, &mut len);
        let mut extra = 0.0;
        add_terms(&u.title, tune("title", W_TITLE), &mut tf, &mut extra);
        add_terms(&u.tags, tune("tags", W_TAGS), &mut tf, &mut extra);
        add_terms(&u.summary, tune("summary", W_SUMMARY), &mut tf, &mut extra);
        add_terms(&u.heading, tune("heading", W_HEADING), &mut tf, &mut extra);
        docs.push((tf, len.max(1.0)));
    }
    let n = docs.len().max(1) as f64;
    let avg = docs.iter().map(|d| d.1).sum::<f64>() / n;
    // Query terms: each word as a whole; a compound word nobody uses counts as its parts.
    let mut terms: Vec<String> = vec![];
    for w in &words {
        let whole = norm_term(w);
        if docs.iter().any(|d| d.0.contains_key(&whole)) || !w.contains(['.', '_', '/', '-']) {
            terms.push(whole);
        } else {
            terms.extend(PART.find_iter(w).map(|p| norm_term(p.as_str())));
        }
    }
    terms.sort();
    terms.dedup();
    let idf: HashMap<&str, f64> = terms
        .iter()
        .map(|t| {
            let df = docs.iter().filter(|d| d.0.contains_key(t)).count() as f64;
            (t.as_str(), (1.0 + (n - df + 0.5) / (df + 0.5)).ln())
        })
        .collect();
    let dates = query_dates(query);
    static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    let phrase = WS.replace_all(&to_lf(query).to_lowercase(), " ").trim().to_string();
    let now = now_ms();
    // Best sections per file.
    let (k1, b, coord, date_bonus) = (tune("k1", K1), tune("b", B), tune("coord", COORD), tune("date", DATE_BONUS));
    let (page_boost, filed_note, other_sections) = (tune("page", PAGE_BOOST), tune("filed", FILED_NOTE), tune("other", OTHER_SECTIONS));
    let mut by_file: HashMap<String, Vec<(f64, usize)>> = HashMap::new();
    for (i, (tf, len)) in docs.iter().enumerate() {
        let u = &all[i];
        let mut score = 0.0;
        let mut matched = 0;
        for t in &terms {
            if let Some(f) = tf.get(t) {
                score += idf[t.as_str()] * f * (k1 + 1.0) / (f + k1 * (1.0 - b + b * len / avg));
                matched += 1;
            }
        }
        // The date asked about: that day's log entries and notes.
        if dates.contains(&u.date) && u.kind != "page" {
            score += date_bonus;
            matched += 1;
        }
        if score <= 0.0 {
            continue;
        }
        // A section that has more of the question's words beats one with a single rare word.
        score *= (matched as f64 / (terms.len() + dates.len()).max(1) as f64).powf(coord);
        if words.len() > 1 && phrase.chars().count() > 3 && format!("{} {}", u.heading, u.body).to_lowercase().contains(&phrase) {
            score *= 1.25;
        }
        by_file.entry(u.rel.clone()).or_default().push((score, i));
    }
    let mut results: Vec<Hit> = vec![];
    for (_, mut secs) in by_file {
        secs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        let (best, i) = secs[0];
        let u = &all[i];
        let mut score = best + other_sections * secs[1..].iter().map(|s| s.0).sum::<f64>().min(best * 2.0);
        if u.kind == "page" {
            score *= page_boost;
        } else if u.rel.starts_with(".curator/") {
            score *= filed_note;
        }
        let age_days = ((now - u.time) as f64 / 86_400_000.0).max(0.0);
        score *= 1.0 + 0.3 * (-age_days / 14.0).exp();
        let mut lines = snippets(&u.heading, &u.body, &words, 3);
        for &(_, j) in secs.iter().skip(1) {
            if lines.len() >= 3 {
                break;
            }
            lines.extend(snippets(&all[j].heading, &all[j].body, &words, 3 - lines.len()));
        }
        if lines.is_empty() && !u.summary.is_empty() {
            lines.push(u.summary.clone());
        }
        results.push(Hit {
            kind: u.kind,
            rel: u.rel.clone(),
            target: u.target.clone(),
            label: u.label.clone(),
            section: u.section.clone(),
            heading: u.heading.clone(),
            score: (score * 10.0).round() / 10.0,
            snippets: lines,
        });
    }
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal).then_with(|| collate_cmp(&b.rel, &a.rel)));
    if let Some((s, qv)) = semantic {
        results = fuse(wiki_dir, &s, query, &qv, &all, results, &words);
    }
    let limit = if limit <= 0 { 8 } else { limit.min(50) }.max(1) as usize;
    results.truncate(limit);
    Ok(results)
}

/// One section of a page or log day: (heading, text), by anchor.
pub fn section_of(rel: &str, text: &str, want: &str) -> Option<(String, String)> {
    let want = anchor(want);
    if rel.starts_with("log/") {
        log_sections(text).into_iter().find(|(a, ..)| *a == want).map(|(_, h, _, t)| (h, t))
    } else {
        let (_, body) = crate::frontmatter::parse(text);
        page_sections(&body).into_iter().find(|(a, ..)| *a == want).map(|(_, h, t)| (h, t))
    }
}

/// The anchors of a file's sections, for "no such section" answers.
pub fn anchors_of(rel: &str, text: &str) -> Vec<String> {
    if rel.starts_with("log/") {
        log_sections(text).into_iter().map(|(a, ..)| a).collect()
    } else {
        page_sections(&crate::frontmatter::parse(text).1).into_iter().map(|(a, ..)| a).filter(|a| !a.is_empty()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_in_queries() {
        assert_eq!(query_dates("What changed on 2026-09-25?"), ["2026-09-25"]);
        assert_eq!(query_dates("What changed for Lighthouse on October 1, 2026?"), ["2026-10-01"]);
        assert_eq!(query_dates("the 3rd of Sept 2025 and Oct 14th 2026"), ["2025-09-03", "2026-10-14"]);
        assert!(query_dates("May I ask about Harbor?").is_empty(), "\"May\" needs a day");
    }

    #[test]
    fn stems_meet() {
        for (a, b) in [("retries", "retry"), ("moved", "move"), ("moving", "move"), ("warehouses", "warehouse"), ("stopped", "stop"), ("backups", "backup"), ("switches", "switch")] {
            assert_eq!(norm_term(a), norm_term(b), "{a} / {b}");
        }
        for w in ["8443", "v3", "c:/dev", "ssh", "this", "status", "class"] {
            assert_eq!(stem(w), w, "{w} stays");
        }
    }

    #[test]
    fn sections_and_anchors() {
        let s = page_sections("# T\n\nIntro.\n\n## Where things live\n\n- Repo\n\n```\n## not a heading\n```\n\n### Ports\n\n8443\n\n## Where things live\n\nagain\n");
        let names: Vec<&str> = s.iter().map(|(a, ..)| a.as_str()).collect();
        assert_eq!(names, ["", "where-things-live", "ports", "where-things-live-2"]);
        assert!(s[1].2.contains("## not a heading"));
        let l =
            log_sections("# 2026-10-03\n\n## 15:22 · codex · Filed X\n\nBody.\n\n- 15:23 · curator · Page updated: X [[x]]\n\n<!-- curator batch b -->\n\n## 16:00 · claude-code · Other\n\nMore.\n");
        assert_eq!(l.iter().map(|x| (x.0.as_str(), x.2.as_str())).collect::<Vec<_>>(), [("15-22-filed-x", "codex"), ("16-00-other", "claude-code")]);
        assert!(l[0].3.contains("Page updated") && !l[0].3.contains("curator batch"));
        assert_eq!(section_of("log/2026/2026-10-03.md", "## 16:00 · claude-code · Other\n\nMore.\n", "16-00-other").unwrap().1.trim(), "More.");
    }

    #[test]
    fn hybrid_finds_a_page_by_meaning_and_keeps_lexical_hits() {
        let w = std::env::temp_dir().join(format!("aw-search-hybrid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(w.join("pages")).unwrap();
        let page = |title: &str, body: &str| {
            format!(
                "---
title: {title}
type: topic
summary: \"\"
tags: []
---

# {title}

{body}
"
            )
        };
        std::fs::write(w.join("pages/lisbon-trip.md"), page("Lisbon trip", "Flight TP 1234 on 2026-10-14; Hotel Alfama Patio.")).unwrap();
        std::fs::write(w.join("pages/harbor.md"), page("Harbor", "Harbor syncs inventory every five minutes.")).unwrap();
        let s = crate::embed::Settings {
            model: "test/m".into(),
            weight: 0.9,
            keyword_weight: 0.5,
            credential: String::new(),
            query_timeout: std::time::Duration::from_millis(100),
            base_url: "https://x.example".into(),
        };
        // Toy vectors: the Lisbon page points one way, Harbor the other.
        for u in units(&w, "all").unwrap() {
            let v = if u.rel.contains("lisbon") { [1.0, 0.0] } else { [0.0, 1.0] };
            crate::embed::put(&w, &s.model, &crate::embed::text_key(&s.model, &embed_text(&u)), &v).unwrap();
        }
        let f = Filters::default();
        let q = "where am I staying in Portugal";
        assert!(search_with(&w, q, "all", 8, &f, None).unwrap().is_empty(), "no word of the question is in the wiki");
        let hits = search_with(&w, q, "all", 8, &f, Some((s.clone(), vec![0.95, 0.31]))).unwrap();
        assert_eq!(hits.first().map(|h| h.rel.as_str()), Some("pages/lisbon-trip.md"), "found by meaning");
        assert!(!hits[0].snippets.is_empty(), "a semantic-only hit still shows a line of its section");
        // A query both sides agree on keeps its lexical winner first.
        let hits = search_with(&w, "Harbor inventory sync", "all", 8, &f, Some((s, vec![0.2, 0.98]))).unwrap();
        assert_eq!(hits.first().map(|h| h.rel.as_str()), Some("pages/harbor.md"));
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn other_wordings_are_fused() {
        let w = std::env::temp_dir().join(format!("aw-search-many-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(w.join("pages")).unwrap();
        let page = |title: &str, body: &str| format!("---\ntitle: {title}\ntype: topic\nsummary: \"\"\ntags: []\n---\n\n# {title}\n\n{body}\n");
        std::fs::write(w.join("pages/lisbon-trip.md"), page("Lisbon trip", "Flight TP 1234 on 2026-10-14; Hotel Alfama Patio.")).unwrap();
        std::fs::write(w.join("pages/harbor.md"), page("Harbor", "Harbor syncs inventory every five minutes.")).unwrap();
        let f = Filters::default();
        let one = search_many(&w, &["vacation in Portugal".into()], "all", 8, &f).unwrap();
        assert!(one.iter().all(|h| h.rel != "pages/lisbon-trip.md"), "the question's words are not on the page");
        let many = search_many(&w, &["vacation in Portugal".into(), "Lisbon trip hotel".into()], "all", 8, &f).unwrap();
        assert_eq!(many.first().map(|h| h.rel.as_str()), Some("pages/lisbon-trip.md"), "another wording finds it");
        // Aliases the curator wrote make the page findable by those words on their own.
        std::fs::write(w.join("pages/lisbon-trip.md"), page("Lisbon trip", "Flight TP 1234 on 2026-10-14; Hotel Alfama Patio.").replace("tags: []", "tags: []\naliases: [\"portugal\", \"vacation\"]"))
            .unwrap();
        assert_eq!(search(&w, "vacation in Portugal", "all", 8, &f).unwrap().first().map(|h| h.rel.as_str()), Some("pages/lisbon-trip.md"));
        let both = search_many(&w, &["Harbor inventory".into(), "Lisbon flight".into()], "all", 8, &f).unwrap();
        assert_eq!(both.iter().map(|h| h.rel.as_str()).collect::<Vec<_>>(), ["pages/harbor.md", "pages/lisbon-trip.md"], "ties keep the first wording's order");
        let _ = std::fs::remove_dir_all(&w);
    }
}
