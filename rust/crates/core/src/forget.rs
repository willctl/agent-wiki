//! Forget (M6): find every copy of a piece of text and redact it, after showing where it is.
//!
//! "Forget" on a page alone would leave copies behind: the page's .history/ versions, the notes the
//! curator filed (.curator/done/), its audit records, held changes, Ask's questions and answers, the
//! log, the inbox, and the request logs (which keep clipped tool arguments). This walks all of them.
//! It is also the remedy for a secret that got past the guard.
//!
//! Text inside JSON files may be escaped, so both the plain and the JSON-escaped forms are matched.
//! Log files that other processes keep open for appending are rewritten in place, so their next line
//! still lands in the same file. The tombstone records when and how much was redacted, never what.

use crate::text::*;
use crate::wiki::{self, Result, atomic_write, wiki_err};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// The shortest text that may be forgotten: anything shorter would match all over the wiki.
pub const MIN_CHARS: usize = 4;
const REDACTED: &str = "[redacted]";

pub struct Found {
    /// The file, relative to the wiki (or "logs/<name>" for the request logs).
    pub rel: String,
    pub count: usize,
    /// One line around the first match, with every match masked.
    pub sample: String,
}

/// The patterns to look for: the text as typed, and as it appears inside a JSON string.
fn patterns(text: &str, ignore_case: bool) -> Result<Vec<Regex>> {
    let t = text.trim();
    if t.chars().count() < MIN_CHARS {
        return wiki_err(format!("Give at least {MIN_CHARS} characters to forget."));
    }
    let mut forms = vec![t.to_string()];
    let json = serde_json::to_string(t).unwrap_or_default();
    let json = json.trim_matches('"').to_string();
    if json != t {
        forms.push(json);
    }
    forms.iter().map(|f| RegexBuilder::new(&regex::escape(f)).case_insensitive(ignore_case).build().map_err(|e| wiki::Error::Wiki(e.to_string()))).collect()
}

/// Every text file that may hold a copy: the whole wiki (but not .locks or a git history), and the
/// request and process logs.
fn files(wiki_dir: &Path, logs_dir: Option<&Path>) -> Vec<(String, PathBuf, bool)> {
    let mut out = vec![];
    let mut stack = vec![wiki_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if wiki::confined_path(wiki_dir, &p).is_err() {
                continue;
            }
            if p.is_dir() {
                // Not the pending requests themselves: they are removed once decided.
                if name != ".locks" && name != ".git" && name != "tmp" && !(name == "forget" && dir.ends_with(".curator")) {
                    stack.push(p);
                }
            } else if [".md", ".json", ".jsonl", ".txt"].iter().any(|x| name.ends_with(x)) {
                let rel = p.strip_prefix(wiki_dir).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or(name);
                out.push((rel, p, false));
            }
        }
    }
    if let Some(logs) = logs_dir.filter(|l| !l.starts_with(wiki_dir)) {
        for e in fs::read_dir(logs).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if wiki::confined_path(logs, &e.path()).is_ok() && e.path().is_file() && (name.ends_with(".jsonl") || name.ends_with(".log")) {
                out.push((format!("logs/{name}"), e.path(), true));
            }
        }
    }
    out.sort();
    out
}

/// About 200 characters of the line around its first match, with every match masked.
fn mask(line: &str, pats: &[Regex]) -> String {
    let mut s = line.trim().to_string();
    for p in pats {
        s = p.replace_all(&s, "█████").into_owned();
    }
    let chars: Vec<char> = s.chars().collect();
    let at = chars.iter().position(|c| *c == '█').unwrap_or(0);
    let start = at.saturating_sub(80);
    let end = (start + 200).min(chars.len());
    format!("{}{}{}", if start > 0 { "..." } else { "" }, chars[start..end].iter().collect::<String>(), if end < chars.len() { "..." } else { "" })
}

/// Where the text is, before anything changes.
pub fn find(wiki_dir: &Path, logs_dir: Option<&Path>, text: &str, ignore_case: bool) -> Result<Vec<Found>> {
    let pats = patterns(text, ignore_case)?;
    let mut out = vec![];
    for (rel, path, _) in files(wiki_dir, logs_dir) {
        let Ok(content) = fs::read_to_string(&path) else { continue };
        let count: usize = pats.iter().map(|p| p.find_iter(&content).count()).sum();
        if count == 0 {
            continue;
        }
        let line = content.lines().find(|l| pats.iter().any(|p| p.is_match(l))).unwrap_or("");
        out.push(Found { rel, count, sample: mask(line, &pats) });
    }
    Ok(out)
}

/// Redacts every copy and leaves a tombstone. Returns the files changed and how many matches.
pub fn redact(wiki_dir: &Path, logs_dir: Option<&Path>, text: &str, ignore_case: bool, by: &str) -> Result<Value> {
    wiki::with_lock(wiki_dir, || redact_locked(wiki_dir, logs_dir, text, ignore_case, by))
}

/// The caller holds the wiki lock through redaction and any request decision.
fn redact_locked(wiki_dir: &Path, logs_dir: Option<&Path>, text: &str, ignore_case: bool, by: &str) -> Result<Value> {
    let pats = patterns(text, ignore_case)?;
    let mut changed = 0;
    let mut total = 0;
    let mut failed = vec![];
    for (rel, path, in_place) in files(wiki_dir, logs_dir) {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                failed.push(format!("{rel}: {e}"));
                continue;
            }
        };
        let count: usize = pats.iter().map(|p| p.find_iter(&content).count()).sum();
        if count == 0 {
            continue;
        }
        let mut next = content.clone();
        for p in &pats {
            next = p.replace_all(&next, REDACTED).into_owned();
        }
        // Open log files are rewritten in place (appenders keep writing to them); the rest atomically.
        let r = if in_place { fs::write(&path, &next).map_err(wiki::Error::from) } else { atomic_write(&path, &next, None, None) };
        match r {
            Ok(()) => {
                changed += 1;
                total += count;
                crate::readcache::forget(&path);
            }
            Err(e) => failed.push(format!("{rel}: {e}")),
        }
    }
    wiki::refresh_index(wiki_dir, None)?;
    // Hybrid search's vectors encode their text's meaning: drop them all, and the curator embeds the
    // redacted pages again (embed.rs; a few cents for a whole wiki).
    if let Ok(vectors) = wiki::confined_path(wiki_dir, &wiki_dir.join(".curator").join("vectors")) {
        let _ = fs::remove_dir_all(vectors);
    }
    let now = now();
    let tomb = json!({ "at": local_iso(&now), "by": by, "files": changed, "matches": total, "ignoreCase": ignore_case, "failed": failed.len() });
    let dir = wiki::confined_path(wiki_dir, &wiki_dir.join(".curator").join("forgotten"))?;
    fs::create_dir_all(&dir)?;
    atomic_write(&dir.join(format!("{}.json", history_stamp(&now))), &format!("{}\n", serde_json::to_string_pretty(&tomb).unwrap_or_default()), None, None)?;
    let outcome = if failed.is_empty() {
        format!("Forgot a piece of text: {total} match(es) redacted in {changed} file(s)")
    } else {
        format!("Forget incomplete: {total} match(es) redacted in {changed} file(s); {} file(s) failed", failed.len())
    };
    let _ = wiki::append_to_log(wiki_dir, &now, &format!("- {} · {by} · {outcome}", local_hm(&now)), true);
    Ok(json!({ "files": changed, "matches": total, "failed": failed }))
}

// ---------------------------------------------------------------- requests from the curator

fn requests_dir(wiki_dir: &Path) -> PathBuf {
    wiki_dir.join(".curator").join("forget")
}

/// Records what a batch's user notes asked to forget, for the person to approve (idempotent).
pub fn record_request(wiki_dir: &Path, batch: &str, at: &str, items: &[Value]) -> Result<()> {
    if !crate::inbox::is_note_id(batch) {
        return wiki_err("invalid forget request batch");
    }
    // The curator calls this while committing under the wiki lock.
    let file = wiki::confined_path(wiki_dir, &requests_dir(wiki_dir).join(format!("{batch}.json")))?;
    if file.exists() {
        return Ok(());
    }
    fs::create_dir_all(file.parent().unwrap())?;
    atomic_write(&file, &format!("{}\n", serde_json::to_string_pretty(&json!({ "batch": batch, "at": at, "items": items })).unwrap_or_default()), None, None)
}

fn load_requests(wiki_dir: &Path) -> Vec<(String, Value)> {
    let Ok(dir) = wiki::confined_path(wiki_dir, &requests_dir(wiki_dir)) else { return vec![] };
    let mut names: Vec<String> =
        fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.strip_suffix(".json").is_some_and(crate::inbox::is_note_id)).collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|n| {
            wiki::confined_path(wiki_dir, &requests_dir(wiki_dir).join(&n))
                .ok()
                .and_then(|p| fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str(&t).ok())
                .map(|v| (n.trim_end_matches(".json").to_string(), v))
        })
        .collect()
}

/// How many forget requests wait for the person.
pub fn pending_count(wiki_dir: &Path) -> usize {
    load_requests(wiki_dir).iter().map(|(_, v)| v["items"].as_array().into_iter().flatten().filter(|v| !v.is_null()).count()).sum()
}

/// The waiting requests, each with where its text is now.
pub fn requests(wiki_dir: &Path, logs_dir: Option<&Path>) -> Vec<Value> {
    let mut out = vec![];
    for (batch, v) in load_requests(wiki_dir) {
        for (i, item) in v["items"].as_array().into_iter().flatten().enumerate() {
            if item.is_null() {
                continue;
            }
            let text = item["text"].as_str().unwrap_or("");
            let found = find(wiki_dir, logs_dir, text, false).unwrap_or_default();
            out.push(json!({
                "batch": batch, "index": i, "at": v["at"], "text": text, "noteIds": item["noteIds"],
                "matches": found.iter().map(|f| f.count).sum::<usize>(), "files": found.iter().map(|f| f.rel.clone()).collect::<Vec<_>>(),
            }));
        }
    }
    out
}

/// Approves (redacts everywhere) or dismisses one request. Either way the request, the last copy of
/// its text that this code keeps, is removed.
pub fn decide_request(wiki_dir: &Path, logs_dir: Option<&Path>, batch: &str, index: usize, approve: bool) -> Result<Value> {
    if !crate::inbox::is_note_id(batch) {
        return wiki_err("no such request");
    }
    wiki::with_lock(wiki_dir, || decide_request_locked(wiki_dir, logs_dir, batch, index, approve))
}

fn decide_request_locked(wiki_dir: &Path, logs_dir: Option<&Path>, batch: &str, index: usize, approve: bool) -> Result<Value> {
    let file = wiki::confined_path(wiki_dir, &requests_dir(wiki_dir).join(format!("{batch}.json")))?;
    let Some(mut v) = fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return wiki_err("no such request") };
    let Some(items) = v["items"].as_array_mut() else { return wiki_err("no such request") };
    if index >= items.len() || items[index].is_null() {
        return wiki_err("no such request");
    }
    let item = items[index].clone();
    let result = if approve {
        let mut r = redact_locked(wiki_dir, logs_dir, item["text"].as_str().unwrap_or(""), false, "window")?;
        if r["failed"].as_array().is_some_and(|f| !f.is_empty()) {
            return wiki_err("Some files could not be redacted. The request is still pending; resolve the file access problem and retry.");
        }
        r["status"] = json!("forgotten");
        r
    } else {
        let now = now();
        let _ = wiki::append_to_log(wiki_dir, &now, &format!("- {} · window · Dismissed a request to forget a piece of text", local_hm(&now)), true);
        json!({ "status": "dismissed" })
    };
    // Preserve positions until the entire batch is decided: another window may still hold an index.
    items[index] = Value::Null;
    if items.iter().all(Value::is_null) {
        fs::remove_file(&file)?
    } else {
        atomic_write(&file, &format!("{}\n", serde_json::to_string_pretty(&v).unwrap_or_default()), None, None)?
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_forget_keeps_the_request_and_decisions_keep_stable_indices() {
        let wiki = std::env::temp_dir().join(format!("aw-forget-decisions-{}-{}", std::process::id(), random_hex(6)));
        fs::create_dir_all(&wiki).unwrap();
        let batch = "2026-10-03_12-00-00-000-abcdef";
        record_request(&wiki, batch, "", &[json!({ "text": "abc" }), json!({ "text": "second synthetic phrase" })]).unwrap();
        assert!(decide_request(&wiki, None, batch, 0, true).is_err());
        assert_eq!(pending_count(&wiki), 2, "failed validation must not consume the request");
        assert_eq!(decide_request(&wiki, None, batch, 0, false).unwrap()["status"], "dismissed");
        assert_eq!(requests(&wiki, None)[0]["index"], 1, "another open window can keep using its original index");
        assert_eq!(decide_request(&wiki, None, batch, 1, false).unwrap()["status"], "dismissed");
        assert_eq!(pending_count(&wiki), 0);
        fs::remove_dir_all(wiki).unwrap();
    }

    #[test]
    fn unreadable_text_reports_partial_redaction_and_keeps_the_request() {
        let wiki = std::env::temp_dir().join(format!("aw-forget-read-error-{}-{}", std::process::id(), random_hex(6)));
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join("readable.txt"), "synthetic-marker keep").unwrap();
        fs::write(wiki.join("unreadable.txt"), b"synthetic-marker\xff").unwrap();
        let result = redact(&wiki, None, "synthetic-marker", false, "test").unwrap();
        assert_eq!(result["files"], 1);
        assert_eq!(result["failed"].as_array().unwrap().len(), 1);
        assert!(result["failed"][0].as_str().unwrap().starts_with("unreadable.txt:"));
        assert_eq!(fs::read_to_string(wiki.join("readable.txt")).unwrap(), "[redacted] keep");
        assert_eq!(fs::read(wiki.join("unreadable.txt")).unwrap(), b"synthetic-marker\xff");
        let log = wiki::read_log_day(&wiki, &local_date(&now())).unwrap();
        assert!(log.contains("Forget incomplete") && log.contains("1 file(s) failed"), "{log}");
        let batch = "2026-10-03_12-00-00-000-abcdef";
        record_request(&wiki, batch, "", &[json!({ "text": "synthetic-marker" })]).unwrap();
        assert!(decide_request(&wiki, None, batch, 0, true).is_err());
        assert_eq!(pending_count(&wiki), 1, "a partial read failure must leave the request available for retry");
        fs::remove_file(wiki.join("unreadable.txt")).unwrap();
        assert_eq!(decide_request(&wiki, None, batch, 0, true).unwrap()["status"], "forgotten");
        assert_eq!(pending_count(&wiki), 0);
        fs::remove_dir_all(wiki).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn forgetting_never_follows_links_outside_the_wiki_or_logs() {
        let root = std::env::temp_dir().join(format!("aw-forget-links-{}-{}", std::process::id(), random_hex(6)));
        let wiki = root.join("wiki");
        let outside = root.join("outside");
        let logs = root.join("logs");
        for p in [&wiki, &outside, &logs] {
            fs::create_dir_all(p).unwrap();
        }
        let victim = outside.join("victim.txt");
        fs::write(&victim, "synthetic-marker keep").unwrap();
        std::os::unix::fs::symlink(&outside, wiki.join("linked")).unwrap();
        std::os::unix::fs::symlink(&wiki, wiki.join("cycle")).unwrap();
        std::os::unix::fs::symlink(&victim, logs.join("linked.log")).unwrap();
        assert!(find(&wiki, Some(&logs), "synthetic-marker", false).unwrap().is_empty());
        redact(&wiki, Some(&logs), "synthetic-marker", false, "test").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "synthetic-marker keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_and_redacts_every_copy() {
        let w = std::env::temp_dir().join(format!("aw-forget-{}", std::process::id()));
        let logs = std::env::temp_dir().join(format!("aw-forget-logs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&w);
        for d in ["pages", ".history/pages/p", ".curator/done/2026/10", ".curator/audit/2026/10", ".curator/asks", "log/2026", ".locks"] {
            fs::create_dir_all(w.join(d)).unwrap();
        }
        fs::create_dir_all(&logs).unwrap();
        let secret = "hunter2-\"quoted\" passphrase";
        fs::write(w.join("pages/p.md"), format!("---\ntitle: P\n---\n\n# P\n\nIt is {secret}.\n")).unwrap();
        fs::write(w.join(".history/pages/p/old.md"), format!("old {secret}")).unwrap();
        fs::write(w.join(".curator/done/2026/10/n.md"), format!("note {secret} and again {secret}")).unwrap();
        fs::write(w.join(".curator/audit/2026/10/n.json"), json!({ "title": format!("about {secret}") }).to_string()).unwrap();
        fs::write(w.join("log/2026/2026-10-03.md"), "# 2026-10-03\n\nnothing here\n").unwrap();
        fs::write(w.join(".locks/x.json"), secret).unwrap();
        fs::write(logs.join("requests-2026-10-03.jsonl"), format!("{}\n", json!({ "args": { "body": secret } }))).unwrap();

        assert!(find(&w, None, "abc", false).is_err(), "too short");
        let found = find(&w, Some(&logs), secret, false).unwrap();
        let rels: Vec<&str> = found.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, [".curator/audit/2026/10/n.json", ".curator/done/2026/10/n.md", ".history/pages/p/old.md", "logs/requests-2026-10-03.jsonl", "pages/p.md"]);
        assert_eq!(found.iter().find(|f| f.rel.ends_with("n.md")).unwrap().count, 2);
        assert!(found.iter().all(|f| !f.sample.contains("hunter2")), "samples are masked");
        assert_eq!(find(&w, None, "HUNTER2-", true).unwrap().len(), 4, "ignoring case (and leaving the logs out)");

        let r = redact(&w, Some(&logs), secret, false, "window").unwrap();
        assert_eq!(r["files"], 5);
        assert_eq!(r["matches"], 6);
        assert!(find(&w, Some(&logs), secret, false).unwrap().is_empty());
        assert!(fs::read_to_string(w.join("pages/p.md")).unwrap().contains("It is [redacted]."));
        assert!(serde_json::from_str::<Value>(&fs::read_to_string(w.join(".curator/audit/2026/10/n.json")).unwrap()).is_ok(), "JSON stays valid");
        assert_eq!(fs::read_to_string(w.join(".locks/x.json")).unwrap(), secret, "locks are not wiki content");
        let tomb = fs::read_dir(w.join(".curator/forgotten")).unwrap().next().unwrap().unwrap().path();
        let t = fs::read_to_string(tomb).unwrap();
        assert!(t.contains("\"matches\": 6") && !t.contains("hunter2"), "{t}");

        // A request from the curator waits, shows where its text is, and goes away when decided.
        fs::write(w.join("pages/p.md"), "# P\n\nOld phone 555-0100.\n").unwrap();
        record_request(&w, "2026-10-03_12-00-00-000-dddddd", "2026-10-03T12:00:00-05:00", &[json!({ "text": "555-0100", "noteIds": ["x"] }), json!({ "text": "nothing like it", "noteIds": ["x"] })])
            .unwrap();
        assert_eq!(pending_count(&w), 2);
        let reqs = requests(&w, None);
        assert_eq!((reqs[0]["matches"].as_u64(), reqs[1]["matches"].as_u64()), (Some(1), Some(0)));
        assert_eq!(decide_request(&w, None, "2026-10-03_12-00-00-000-dddddd", 1, false).unwrap()["status"], "dismissed");
        assert_eq!(decide_request(&w, None, "2026-10-03_12-00-00-000-dddddd", 0, true).unwrap()["status"], "forgotten");
        assert_eq!(pending_count(&w), 0);
        assert!(!w.join(".curator/forget/2026-10-03_12-00-00-000-dddddd.json").exists(), "no copy of the text is kept");
        assert!(fs::read_to_string(w.join("pages/p.md")).unwrap().contains("Old phone [redacted]."));
        let _ = fs::remove_dir_all(&w);
        let _ = fs::remove_dir_all(&logs);
    }
}
