//! The window: the web app at /ui/ and the JSON API it reads, /api/* (src/ui-api.mjs). Read-only
//! apart from pausing the curator, asking questions, deciding held changes and reverting a page
//! change; those need the X-Agent-Wiki header, which a page from another site cannot send.

use crate::http::{App, Entry, Reply, read_body, status_report};
use crate::httpd::Request;
use crate::mcp::State;
use aw_core::activity::{links_in, parse_log_day};
use aw_core::text::{collate_cmp, local_iso, strip_markers, take_chars};
use aw_core::wiki::{self, DATE_RE, Page, SLUG_RE};
use aw_core::{asks, held, inbox};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

const CSP: &str =
    "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The built window: `ui` next to this program, or AGENT_WIKI_UI_DIR.
pub fn ui_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AGENT_WIKI_UI_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("ui"))).unwrap_or_else(|| PathBuf::from("ui"))
}

fn send(status: u16, body: Vec<u8>, kind: &str) -> Reply {
    Reply {
        status,
        body,
        headers: vec![
            ("Content-Type".into(), kind.into()),
            ("Cache-Control".into(), "no-store".into()),
            ("X-Content-Type-Options".into(), "nosniff".into()),
            ("Referrer-Policy".into(), "no-referrer".into()),
            ("Cross-Origin-Opener-Policy".into(), "same-origin".into()),
            ("Content-Security-Policy".into(), CSP.into()),
        ],
    }
}

fn json_reply(status: u16, v: &Value) -> Reply {
    send(status, format!("{v}\n").into_bytes(), "application/json")
}

fn content_type(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// One query parameter, decoded as URLSearchParams does ('+' is a space).
pub fn param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if decode(k) == name {
            return Some(decode(v));
        }
    }
    None
}

fn decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Math.max(lo, Math.min(hi, parseInt(v) || d)).
fn int_param(v: Option<String>, d: i64, lo: i64, hi: i64) -> i64 {
    let s = v.unwrap_or_default();
    let t = s.trim_start();
    let neg = t.starts_with('-');
    let digits: String = t.trim_start_matches(['-', '+']).chars().take_while(|c| c.is_ascii_digit()).collect();
    let n = digits.parse::<i64>().ok().map(|n| if neg { -n } else { n }).filter(|n| *n != 0).unwrap_or(d);
    n.clamp(lo, hi)
}

fn page_summary(p: &Page) -> Value {
    json!({
        "slug": p.slug, "title": p.title, "type": p.kind, "summary": p.summary, "tags": p.tags, "updated": p.updated,
        "updatedBy": p.meta.str("updated_by"), "created": p.meta.str("created"), "time": p.time,
        "words": p.body.split_whitespace().count(),
    })
}

/// The cleanup schedule for the window, with config.json's per-computer off switch.
fn cleanup_status(st: &State, wiki_dir: &Path) -> Value {
    aw_core::lint::schedule_status(wiki_dir, aw_core::curator::cleanups_off(&st.config))
}

fn page_history(wiki_dir: &Path, slug: &str) -> Vec<Value> {
    let Ok(dir) = wiki::confined_path(wiki_dir, &wiki_dir.join(".history").join("pages").join(slug)) else { return vec![] };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().filter(|e| wiki::confined_path(wiki_dir, &e.path()).is_ok()).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".md")).collect())
        .unwrap_or_default();
    names.sort();
    names.reverse();
    static AT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"_([0-9][0-9])-([0-9][0-9])-([0-9][0-9])$").unwrap());
    names.into_iter().map(|n| json!({ "file": format!(".history/pages/{slug}/{n}"), "at": AT.replace(&take_chars(&n, 19), "T$1:$2:$3").to_string() })).collect()
}

#[cfg(all(test, unix))]
#[test]
fn history_listing_does_not_follow_planted_links() {
    use std::os::unix::fs::symlink;
    let base = std::env::temp_dir().join(format!("aw-ui-history-{}", aw_core::text::random_hex(8)));
    let root = base.join("wiki");
    let outside = base.join("outside");
    std::fs::create_dir_all(root.join(".history/pages/page")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("private.md"), "private").unwrap();
    symlink(&outside, root.join(".history/pages/linked")).unwrap();
    symlink(outside.join("private.md"), root.join(".history/pages/page/private.md")).unwrap();
    assert!(page_history(&root, "linked").is_empty());
    assert!(page_history(&root, "page").is_empty());
    std::fs::write(root.join(".history/pages/page/2026-10-07_10-00-00.md"), "history").unwrap();
    assert_eq!(page_history(&root, "page").len(), 1);
    std::fs::remove_dir_all(base).unwrap();
}

fn page_detail(wiki_dir: &Path, slug: &str) -> Option<Value> {
    let pages = wiki::list_pages(wiki_dir).ok()?;
    let p = pages.iter().find(|x| x.slug == slug)?;
    let backlinks: Vec<Value> = pages.iter().filter(|x| x.slug != slug && links_in(&x.body).contains(&slug.to_string())).map(|x| json!({ "slug": x.slug, "title": x.title, "type": x.kind })).collect();
    let links: Vec<Value> = links_in(&p.body)
        .into_iter()
        .map(|s| {
            let found = pages.iter().find(|x| x.slug == s);
            json!({ "slug": s, "exists": found.is_some(), "title": found.map(|x| x.title.clone()).unwrap_or_else(|| s.clone()) })
        })
        .collect();
    let mut out = page_summary(p);
    let o = out.as_object_mut()?;
    o.insert("body".into(), json!(strip_markers(&p.body).trim()));
    o.insert("rel".into(), json!(p.rel));
    o.insert("path".into(), json!(wiki::disp(&wiki_dir.join(&p.rel))));
    o.insert("links".into(), Value::Array(links));
    o.insert("backlinks".into(), Value::Array(backlinks));
    o.insert("history".into(), Value::Array(page_history(wiki_dir, slug)));
    Some(out)
}

fn log_days(wiki_dir: &Path, days: i64, max: usize) -> Vec<Value> {
    let mut out = vec![];
    let mut count = 0;
    for date in wiki::days_back(days) {
        if count >= max {
            break;
        }
        let Some(text) = wiki::read_log_day(wiki_dir, &date).filter(|t| !t.is_empty()) else { continue };
        let entries: Vec<_> = parse_log_day(&date, &text).into_iter().take(max - count).collect();
        count += entries.len();
        out.push(json!({ "date": date, "entries": entries }));
    }
    out
}

fn hit_json(h: &wiki::Hit) -> Value {
    json!({ "kind": h.kind, "rel": h.rel, "target": h.target, "section": h.section, "heading": h.heading, "label": h.label, "score": num(h.score), "snippets": h.snippets })
}

/// A number as JSON.stringify writes it (28, not 28.0).
fn num(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9e15 { json!(x as i64) } else { json!(x) }
}

pub fn handle(app: &App, st: &State, req: &Request, method: &str, path: &str, query: &str, entry: &mut Entry) -> Reply {
    if path == "/ui" {
        let mut r = send(308, vec![], "text/plain");
        r.headers.push(("Location".into(), "/ui/".into()));
        return r;
    }
    if let Some(rest) = path.strip_prefix("/ui/") {
        if method != "GET" && method != "HEAD" {
            let mut r = send(405, b"Use GET".to_vec(), "text/plain");
            r.headers.push(("Allow".into(), "GET".into()));
            return r;
        }
        let name = if rest.is_empty() { "index.html" } else { rest };
        static NAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9._-]*$").unwrap());
        if !NAME.is_match(name) {
            return send(404, b"Not found".to_vec(), "text/plain");
        }
        let dir = ui_dir();
        let file = match wiki::confined_path(&dir, &dir.join(name)) {
            Ok(file) => file,
            Err(_) => return send(404, b"Not found".to_vec(), "text/plain"),
        };
        let Ok(body) = std::fs::read(file) else {
            let msg = if dir.exists() { "Not found" } else { "The tray window is not built: run npm run install-local." };
            return send(404, msg.as_bytes().to_vec(), "text/plain");
        };
        entry.quiet = name != "index.html";
        return send(200, body, content_type(name)); // the server leaves the body out of a HEAD answer
    }

    if path == "/api/status" {
        entry.quiet = true;
        return json_reply(200, &status_report(app, st));
    }
    let Some(wiki_dir) = st.wiki_dir.as_ref().filter(|_| st.setup_error.is_none()) else {
        return json_reply(503, &json!({ "error": st.setup_error.clone().unwrap_or_else(|| "wiki not available".into()) }));
    };
    let xh = req.header("x-agent-wiki");

    if path == "/api/curator" {
        if method != "POST" {
            return json_reply(405, &json!({ "error": "Use POST" }));
        }
        if xh.as_deref() != Some("ui") {
            return json_reply(403, &json!({ "error": "Forbidden: missing X-Agent-Wiki header" }));
        }
        let body: Option<Value> = read_body(req).ok().and_then(|b| serde_json::from_str(&b).ok());
        let Some(paused) = body.as_ref().and_then(|b| b.get("paused")).and_then(Value::as_bool) else {
            return json_reply(400, &json!({ "error": "Send JSON: {\"paused\": true|false}" }));
        };
        if let Err(e) = inbox::set_paused(wiki_dir, paused) {
            return json_reply(500, &json!({ "error": e.to_string() }));
        }
        entry.result = Some(if paused { "curator paused" } else { "curator resumed" }.into());
        return json_reply(200, &json!({ "paused": inbox::is_paused(wiki_dir) }));
    }
    if path == "/api/forget" && method == "POST" {
        if xh.as_deref() != Some("ui") {
            return json_reply(403, &json!({ "error": "Forbidden: missing X-Agent-Wiki header" }));
        }
        let body: Value = read_body(req).ok().and_then(|b| serde_json::from_str(&b).ok()).unwrap_or(Value::Null);
        let text = body["text"].as_str().unwrap_or("");
        let ignore_case = body["ignoreCase"].as_bool().unwrap_or(false);
        let logs = aw_core::paths::AppPaths::current().log_dir;
        entry.quiet = false;
        let r = if let Some(batch) = body["batch"].as_str() {
            // A request the curator recorded from a user's note.
            let approve = body["action"].as_str() == Some("approve");
            entry.result = Some(if approve { "forget request approved" } else { "forget request dismissed" }.into());
            aw_core::forget::decide_request(wiki_dir, Some(&logs), batch, body["index"].as_u64().unwrap_or(u64::MAX) as usize, approve)
        } else if body["apply"].as_bool() == Some(true) {
            entry.result = Some("forget applied".into());
            aw_core::forget::redact(wiki_dir, Some(&logs), text, ignore_case, "window")
        } else {
            entry.result = Some("forget preview".into());
            aw_core::forget::find(wiki_dir, Some(&logs), text, ignore_case).map(|found| {
                let total: usize = found.iter().map(|f| f.count).sum();
                json!({ "matches": total, "found": found.iter().map(|f| json!({ "rel": f.rel, "count": f.count, "sample": f.sample })).collect::<Vec<_>>() })
            })
        };
        return match r {
            Ok(v) => json_reply(200, &v),
            Err(wiki::Error::Wiki(m)) => json_reply(400, &json!({ "error": m })),
            Err(e) => json_reply(500, &json!({ "error": e.to_string() })),
        };
    }
    if path == "/api/settings" && method == "POST" {
        if xh.as_deref() != Some("ui") {
            return json_reply(403, &json!({ "error": "Forbidden: missing X-Agent-Wiki header" }));
        }
        let body: Value = read_body(req).ok().and_then(|b| serde_json::from_str(&b).ok()).unwrap_or(Value::Null);
        if let Some(schedule) = body["cleanupSchedule"].as_str() {
            return match aw_core::settings::set_cleanup_schedule(wiki_dir, schedule) {
                Ok(c) => {
                    entry.result = Some(format!("cleanup schedule {}", c.as_str()));
                    json_reply(200, &json!({ "cleanup": cleanup_status(st, wiki_dir) }))
                }
                Err(wiki::Error::Wiki(m)) => json_reply(400, &json!({ "error": m })),
                Err(e) => json_reply(500, &json!({ "error": e.to_string() })),
            };
        }
        let Some(mode) = body["approvals"].as_str() else {
            return json_reply(400, &json!({ "error": "Send JSON: {\"approvals\": \"auto\"|\"manual\"} or {\"cleanupSchedule\": \"<cron>\"|\"off\"}" }));
        };
        if aw_core::settings::Approvals::parse(mode).is_none() {
            return json_reply(400, &json!({ "error": "approvals: \"auto\" or \"manual\"" }));
        }
        let set = match aw_core::settings::set_approvals(wiki_dir, mode) {
            Ok(m) => m,
            Err(e) => return json_reply(500, &json!({ "error": e.to_string() })),
        };
        // Switching to automatic applies what was waiting, as the curator would have.
        let applied = match set {
            aw_core::settings::Approvals::Auto => match held::auto_apply_all(wiki_dir) {
                Ok(n) => n,
                Err(e) => return json_reply(500, &json!({ "error": e.to_string(), "approvals": set.as_str() })),
            },
            aw_core::settings::Approvals::Manual => 0,
        };
        entry.result = Some(format!("approvals {}", set.as_str()));
        return json_reply(200, &json!({ "approvals": set.as_str(), "applied": applied }));
    }
    if path == "/api/models" && method == "POST" {
        if xh.as_deref() != Some("ui") {
            return json_reply(403, &json!({ "error": "Forbidden: missing X-Agent-Wiki header" }));
        }
        let body: Value = read_body(req).ok().and_then(|b| serde_json::from_str(&b).ok()).unwrap_or(Value::Null);
        let role = body["role"].as_str().unwrap_or_default();
        if !aw_core::settings::MODEL_ROLES.contains(&role) {
            return json_reply(400, &json!({ "error": "Send JSON: {\"role\": \"curator\"|\"ask\", \"model\": <slug or null>, \"reasoningEffort\": <level or null>}" }));
        }
        let (model, effort) = (body["model"].as_str().filter(|s| !s.is_empty()), body["reasoningEffort"].as_str().filter(|s| !s.is_empty()));
        let before = aw_core::models::overview(wiki_dir, &st.config);
        if body["force"] != true
            && let Some(why) = aw_core::models::refusal(model, effort, before[role]["model"].as_str().unwrap_or_default(), Some(&before["available"]).filter(|a| a.is_object()))
        {
            return json_reply(400, &json!({ "error": why }));
        }
        return match aw_core::settings::set_model(wiki_dir, role, model, effort, "window") {
            Ok(_) => {
                entry.result = Some(format!("{role} model {} {}", model.unwrap_or("default"), effort.unwrap_or("default")));
                json_reply(200, &aw_core::models::overview(wiki_dir, &st.config))
            }
            Err(wiki::Error::Wiki(m)) => json_reply(400, &json!({ "error": m })),
            Err(e) => json_reply(500, &json!({ "error": e.to_string() })),
        };
    }
    if (path == "/api/held" || path == "/api/revert") && method == "POST" {
        if xh.as_deref() != Some("ui") {
            return json_reply(403, &json!({ "error": "Forbidden: missing X-Agent-Wiki header" }));
        }
        let body: Value = read_body(req).ok().and_then(|b| serde_json::from_str(&b).ok()).unwrap_or(Value::Null);
        let batch = body["batch"].as_str().unwrap_or("");
        let action = body["action"].as_str().unwrap_or("");
        let r = if path == "/api/held" {
            let Some(index) = body["index"].as_u64() else {
                return json_reply(400, &json!({ "error": "Send JSON: {\"batch\": \"<id>\", \"index\": n, \"action\": \"approve\"|\"reject\"|\"refile\"|\"undo\"}" }));
            };
            if action == "undo" { held::undo(wiki_dir, batch, index as usize) } else { held::decide(wiki_dir, batch, index as usize, action) }
        } else {
            let slug = body["slug"].as_str().unwrap_or("");
            if action == "ask" { held::undo_request(wiki_dir, batch, slug) } else { held::revert(wiki_dir, batch, slug) }
        };
        return match r {
            Ok(v) => {
                entry.result = Some(format!("{} {}", path.trim_start_matches("/api/"), v["status"].as_str().unwrap_or("done")));
                json_reply(200, &v)
            }
            Err(held::Error::NotFound(m)) => json_reply(404, &json!({ "error": m })),
            Err(held::Error::Conflict(m)) => json_reply(409, &json!({ "error": m, "conflict": true })),
            Err(held::Error::Wiki(wiki::Error::Wiki(m))) => json_reply(409, &json!({ "error": m })),
            Err(held::Error::Wiki(e)) => json_reply(500, &json!({ "error": e.to_string() })),
        };
    }
    if (path == "/api/ask" || path == "/api/ask/cancel") && method == "POST" {
        if xh.as_deref() != Some("ui") {
            return json_reply(403, &json!({ "error": "Forbidden: missing X-Agent-Wiki header" }));
        }
        let Some(body) = read_body(req).ok().and_then(|b| serde_json::from_str::<Value>(&b).ok()) else {
            let msg = if path == "/api/ask" { "Send JSON: {\"question\": \"...\", \"parent\": \"<id>\"?}" } else { "Send JSON: {\"id\": \"<id>\"}" };
            return json_reply(400, &json!({ "error": msg }));
        };
        if path == "/api/ask/cancel" {
            let id = body.get("id").and_then(Value::as_str).unwrap_or("");
            if !asks::is_ask_id(id) {
                return json_reply(400, &json!({ "error": "id: the id of a question" }));
            }
            entry.result = Some("ask cancelled".into());
            return match asks::cancel_ask(wiki_dir, id) {
                Ok(Some(r)) => json_reply(200, &r),
                Ok(None) => json_reply(404, &json!({ "error": "No such question" })),
                Err(e) => json_reply(500, &json!({ "error": e.to_string() })),
            };
        }
        let worker = asks::worker_status(wiki_dir);
        if worker.get("running") != Some(&Value::Bool(true)) {
            return json_reply(
                503,
                &json!({ "error": "Asking needs the curator, which runs in the Agent Wiki tray app. Start the tray (Start menu > Agent Wiki), then ask again.", "worker": worker }),
            );
        }
        let question = body.get("question").and_then(Value::as_str).unwrap_or("");
        return match asks::create_ask(wiki_dir, question, body.get("parent").unwrap_or(&Value::Null)) {
            Ok(id) => {
                entry.result = Some("ask queued".into());
                json_reply(201, &json!({ "id": id, "worker": worker }))
            }
            Err(wiki::Error::Wiki(m)) => json_reply(400, &json!({ "error": m })),
            Err(e) => json_reply(500, &json!({ "error": e.to_string() })),
        };
    }
    if method != "GET" {
        return json_reply(405, &json!({ "error": "Use GET" }));
    }
    match path {
        "/api/ask" => {
            entry.quiet = true;
            let id = param(query, "id").unwrap_or_default();
            if !asks::is_ask_id(&id) {
                return json_reply(400, &json!({ "error": "id: the id of a question" }));
            }
            let after = int_param(param(query, "after"), 0, 0, 100_000) as usize;
            match asks::read_ask(wiki_dir, &id, after, param(query, "thread").as_deref() == Some("1")) {
                Some(Value::Object(mut a)) => {
                    a.insert("worker".into(), asks::worker_status(wiki_dir));
                    json_reply(200, &Value::Object(a))
                }
                _ => json_reply(404, &json!({ "error": "No such question" })),
            }
        }
        "/api/asks" => {
            entry.quiet = true;
            let max = int_param(param(query, "max"), 30, 1, 100) as usize;
            json_reply(200, &json!({ "asks": asks::list_asks(wiki_dir, max), "worker": asks::worker_status(wiki_dir) }))
        }
        "/api/pages" => {
            let mut pages = wiki::list_pages(wiki_dir).unwrap_or_default();
            pages.sort_by_key(|p| std::cmp::Reverse(p.time));
            json_reply(200, &json!({ "pages": pages.iter().map(page_summary).collect::<Vec<_>>() }))
        }
        "/api/page" => {
            let slug = param(query, "slug").unwrap_or_default();
            if !SLUG_RE.is_match(&slug) {
                return json_reply(400, &json!({ "error": "slug: lowercase letters, digits and hyphens" }));
            }
            match page_detail(wiki_dir, &slug) {
                Some(p) => json_reply(200, &p),
                None => json_reply(404, &json!({ "error": format!("No page \"{slug}\"") })),
            }
        }
        "/api/search" => {
            let q = take_chars(&param(query, "q").unwrap_or_default(), 200);
            let scope = param(query, "scope").filter(|s| ["all", "pages", "log", "notes"].contains(&s.as_str())).unwrap_or_else(|| "all".into());
            let limit = int_param(param(query, "limit"), 20, 1, 50);
            let date = |k: &str| param(query, k).filter(|d| DATE_RE.is_match(d));
            let filters = aw_core::search::Filters { since: date("since"), until: date("until"), app: param(query, "app").filter(|a| !a.trim().is_empty()) };
            let results: Vec<Value> = if q.trim().is_empty() { vec![] } else { aw_core::search::search(wiki_dir, &q, &scope, limit, &filters).unwrap_or_default().iter().map(hit_json).collect() };
            json_reply(200, &json!({ "query": q, "terms": wiki::tokenize(&q), "results": results }))
        }
        "/api/activity" => {
            let days = int_param(param(query, "days"), 14, 1, 90);
            let max = int_param(param(query, "max"), 200, 1, 1000) as usize;
            json_reply(200, &json!({ "days": log_days(wiki_dir, days, max) }))
        }
        "/api/log" => {
            let date = param(query, "date").unwrap_or_default();
            if !DATE_RE.is_match(&date) {
                return json_reply(400, &json!({ "error": "date: YYYY-MM-DD" }));
            }
            match wiki::read_log_day(wiki_dir, &date) {
                Some(text) => json_reply(200, &json!({ "date": date, "entries": parse_log_day(&date, &text) })),
                None => json_reply(404, &json!({ "error": format!("No log for {date}") })),
            }
        }
        "/api/held" => json_reply(
            200,
            &json!({
                "changes": held::list(wiki_dir),
                "forget": aw_core::forget::requests(wiki_dir, Some(&aw_core::paths::AppPaths::current().log_dir)),
                "approvals": aw_core::settings::approvals(wiki_dir).as_str(),
                "approvalsProblem": aw_core::settings::approvals_and_problem(wiki_dir).1,
                "auto": held::recent_auto(wiki_dir, 7),
            }),
        ),
        "/api/settings" => {
            let (approvals, problem) = aw_core::settings::approvals_and_problem(wiki_dir);
            json_reply(200, &json!({ "approvals": approvals.as_str(), "problem": problem, "cleanup": cleanup_status(st, wiki_dir) }))
        }
        "/api/models" => json_reply(200, &aw_core::models::overview(wiki_dir, &st.config)),
        "/api/cleanup-preview" => {
            // What a schedule means before it is saved: in words, and its next three times.
            match aw_core::settings::CleanupSchedule::parse(&param(query, "schedule").unwrap_or_default()) {
                Ok(c) => {
                    let mut at = chrono::Local::now();
                    let mut next = vec![];
                    while let Some(t) = c.cron().and_then(|cron| cron.next(&at)).filter(|_| next.len() < 3) {
                        next.push(local_iso(&t));
                        at = t;
                    }
                    json_reply(200, &json!({ "schedule": c.as_str(), "description": c.describe(), "next": next }))
                }
                Err(why) => json_reply(400, &json!({ "error": why })),
            }
        }
        "/api/note" => {
            let id = param(query, "id").unwrap_or_default();
            if !inbox::is_note_id(&id) {
                return json_reply(400, &json!({ "error": "id: a note id" }));
            }
            match inbox::read_note(wiki_dir, &id) {
                Ok((rel, text)) => {
                    let (status, file) = text.split_once("\n\n").unwrap_or(("", &text));
                    let (meta, body) = aw_core::frontmatter::parse(file);
                    json_reply(
                        200,
                        &json!({
                            "id": id, "rel": rel, "status": status, "filed": rel.starts_with(".curator/"),
                            "app": meta.str("app"), "title": meta.str("title"), "submitted": meta.str("submitted"), "kind": meta.str("kind"),
                            "source": meta.str("source"), "body": body.trim(),
                        }),
                    )
                }
                Err(wiki::Error::Wiki(m)) => json_reply(404, &json!({ "error": m })),
                Err(e) => json_reply(500, &json!({ "error": e.to_string() })),
            }
        }
        "/api/inbox" => {
            let notes: Vec<Value> = inbox::list_notes(wiki_dir)
                .unwrap_or_default()
                .iter()
                .map(|n| {
                    json!({
                        "id": n.id, "app": n.app, "kind": n.kind, "title": n.title, "body": n.body, "submitted": n.submitted,
                        "pages": n.pages, "tags": n.tags, "status": n.status, "attempts": n.attempts, "lastError": n.last_error,
                    })
                })
                .collect();
            json_reply(200, &json!({ "notes": notes, "paused": inbox::is_paused(wiki_dir) }))
        }
        _ => {
            let _ = (collate_cmp("", ""), Map::<String, Value>::new());
            json_reply(404, &json!({ "error": "Not found" }))
        }
    }
}
