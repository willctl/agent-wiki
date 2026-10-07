//! Ask, the window's side (src/ask.mjs): questions queued as files for the curator process, which
//! answers them; the server queues, reads, lists and cancels.
//!
//!   .curator/asks/<id>/ask.json      the question (written here)
//!   .curator/asks/<id>/claim.json    taken by the worker answering it; its mtime is a heartbeat
//!   .curator/asks/<id>/events.jsonl  what the agent searched and read, as it happens
//!   .curator/asks/<id>/result.json   the answer and its sources, or why there is none
//!   .curator/asks/<id>/cancel        present = stop
//!   .curator/asks/worker.json        the worker's heartbeat

use crate::inbox::{is_note_id, new_note_id};
use crate::reqlog::clip_str;
use crate::secrets::find_secret;
use crate::text::*;
use crate::wiki::{Result, atomic_write, read_if_exists, wiki_err, write_if_missing};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const MAX_QUESTION: usize = 2000;
const PUBLIC_RESULT: [&str; 12] = ["status", "answer", "found", "sources", "error", "errorKind", "ms", "model", "reasoningEffort", "searches", "reads", "finishedAt"];

pub fn asks_dir(wiki_dir: &Path) -> PathBuf {
    wiki_dir.join(".curator").join("asks")
}

fn tmp_dir(wiki_dir: &Path) -> PathBuf {
    wiki_dir.join(".curator").join("tmp")
}

pub fn is_ask_id(id: &str) -> bool {
    is_note_id(id)
}

struct Files {
    ask: PathBuf,
    claim: PathBuf,
    events: PathBuf,
    result: PathBuf,
    cancel: PathBuf,
}

fn files(wiki_dir: &Path, id: &str) -> Files {
    let d = asks_dir(wiki_dir).join(id);
    Files { ask: d.join("ask.json"), claim: d.join("claim.json"), events: d.join("events.jsonl"), result: d.join("result.json"), cancel: d.join("cancel") }
}

fn read_json(file: &Path) -> Option<Value> {
    read_if_exists(file).ok().flatten().and_then(|t| serde_json::from_str(&t).ok())
}

/// Complete lines only: a line being appended right now is picked up on the next read.
fn read_events(file: &Path) -> Vec<Value> {
    read_if_exists(file).ok().flatten().unwrap_or_default().split('\n').filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn write_json(wiki_dir: &Path, file: &Path, v: &Value) -> Result<()> {
    atomic_write(file, &format!("{}\n", serde_json::to_string_pretty(v).unwrap_or_default()), Some(&tmp_dir(wiki_dir)), None)
}

/// Queues a question. Returns its id.
pub fn create_ask(wiki_dir: &Path, question: &str, parent: &Value) -> Result<String> {
    let q = to_lf(question).trim().to_string();
    if q.is_empty() {
        return wiki_err("Type a question.");
    }
    if q.encode_utf16().count() > MAX_QUESTION {
        return wiki_err(format!("Keep the question under {MAX_QUESTION} characters."));
    }
    if let Some(kind) = find_secret(&q) {
        return wiki_err(format!("Refused: the question looks like it contains {kind}. Questions are kept in the wiki folder; leave secrets out."));
    }
    let mut follows = Value::Null;
    if !parent.is_null() && parent.as_str() != Some("") {
        let p = parent.as_str().unwrap_or("");
        if !is_ask_id(p) || read_json(&files(wiki_dir, p).ask).is_none() {
            return wiki_err("The question this follows up no longer exists.");
        }
        follows = Value::String(p.to_string());
    }
    let id = new_note_id(&now());
    write_json(wiki_dir, &files(wiki_dir, &id).ask, &json!({ "id": id, "question": q, "parent": follows, "createdAt": local_iso(&now()) }))?;
    Ok(id)
}

/// The worker's heartbeat, with `running` = it is alive.
pub fn worker_status(wiki_dir: &Path) -> Value {
    let Some(Value::Object(mut w)) = read_json(&asks_dir(wiki_dir).join("worker.json")) else {
        return json!({ "running": false });
    };
    let Some(hb) = w.get("heartbeatAt").and_then(Value::as_str).and_then(parse_ms) else {
        return json!({ "running": false });
    };
    let age = (now_ms() - hb) as f64 / 1000.0;
    let stopped = w.get("state").and_then(Value::as_str) == Some("stopped");
    w.insert("running".into(), Value::Bool(!stopped && age < 45.0));
    w.insert("heartbeatAgeSec".into(), json!(age.round() as i64));
    Value::Object(w)
}

fn ask_state(f: &Files, ask: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(result) = read_json(&f.result) {
        out.insert("status".into(), result.get("status").cloned().unwrap_or(Value::Null));
        if let Some(s) = result.get("startedAt") {
            out.insert("startedAt".into(), s.clone());
        }
        let mut public = Map::new();
        for k in PUBLIC_RESULT {
            if let Some(v) = result.get(k) {
                public.insert(k.into(), v.clone());
            }
        }
        out.insert("result".into(), Value::Object(public));
        return out;
    }
    let Ok(claim_meta) = fs::metadata(&f.claim) else {
        let created = ask.get("createdAt").and_then(Value::as_str).and_then(parse_ms).unwrap_or_else(now_ms);
        out.insert("status".into(), json!("queued"));
        out.insert("waitingSec".into(), json!(((now_ms() - created) as f64 / 1000.0).round() as i64));
        return out;
    };
    let age = claim_meta.modified().ok().and_then(|t| t.elapsed().ok()).map(|d| d.as_millis()).unwrap_or(0);
    let claim = read_json(&f.claim);
    out.insert("status".into(), json!(if age > 60_000 { "stalled" } else { "running" }));
    if let Some(s) = claim.as_ref().and_then(|c| c.get("startedAt")) {
        out.insert("startedAt".into(), s.clone());
    }
    out
}

/// One question: its state, events after `after`, the answer once there; with `thread`, the earlier turns.
pub fn read_ask(wiki_dir: &Path, id: &str, after: usize, thread: bool) -> Option<Value> {
    if !is_ask_id(id) {
        return None;
    }
    let f = files(wiki_dir, id);
    let ask = read_json(&f.ask)?;
    let state = ask_state(&f, &ask);
    let events = read_events(&f.events);
    let cancel = f.cancel.exists();
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("question".into(), ask.get("question").cloned().unwrap_or(Value::Null));
    out.insert("parent".into(), ask.get("parent").cloned().filter(|p| !p.is_null() && p.as_str() != Some("")).unwrap_or(Value::Null));
    out.insert("createdAt".into(), ask.get("createdAt").cloned().unwrap_or(Value::Null));
    for (k, v) in state {
        out.insert(k, v);
    }
    if cancel {
        out.insert("cancelRequested".into(), Value::Bool(true));
    }
    out.insert("eventCount".into(), json!(events.len()));
    out.insert("events".into(), Value::Array(events.into_iter().skip(after).collect()));
    if thread {
        let mut turns = Vec::new();
        let mut p = ask.get("parent").and_then(Value::as_str).map(String::from);
        let mut n = 0;
        while let Some(pid) = p.filter(|s| !s.is_empty()) {
            if n >= 10 {
                break;
            }
            n += 1;
            let Some(a) = read_ask(wiki_dir, &pid, 0, false) else { break };
            p = a.get("parent").and_then(Value::as_str).map(String::from);
            turns.insert(0, a);
        }
        out.insert("thread".into(), Value::Array(turns));
    }
    Some(Value::Object(out))
}

/// An answer as one line of plain text, for the list of recent questions.
fn plain(md: &str) -> String {
    static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([a-z0-9-]+)(?:\|([^\]]*))?\]\]").unwrap());
    static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*(?:[-*+]|[0-9]+\.)\s+").unwrap());
    static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap());
    static MARKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[`*]+").unwrap());
    let s = to_lf(md);
    let s = LINK.replace_all(&s, |c: &regex::Captures| c.get(2).filter(|m| !m.as_str().is_empty()).map_or(c[1].to_string(), |m| m.as_str().to_string()));
    let s = BULLET.replace_all(&s, "");
    let s = HEADING.replace_all(&s, "");
    MARKS.replace_all(&s, "").into_owned()
}

/// Recent conversations, newest first: one entry per thread, showing its latest turn.
pub fn list_asks(wiki_dir: &Path, max: usize) -> Vec<Value> {
    let mut ids: Vec<String> = fs::read_dir(asks_dir(wiki_dir)).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| is_ask_id(n)).collect()).unwrap_or_default();
    ids.sort();
    ids.reverse();
    ids.truncate(300);
    struct A {
        ask: Value,
        state: Map<String, Value>,
    }
    let mut asks: HashMap<String, A> = HashMap::new();
    for id in &ids {
        let f = files(wiki_dir, id);
        if let Some(ask) = read_json(&f.ask) {
            let state = ask_state(&f, &ask);
            asks.insert(id.clone(), A { ask, state });
        }
    }
    let root_of = |id: &str| -> String {
        let mut cur = id.to_string();
        for _ in 0..50 {
            let Some(p) = asks.get(&cur).and_then(|a| a.ask.get("parent")).and_then(Value::as_str).filter(|p| asks.contains_key(*p)) else { break };
            cur = p.to_string();
        }
        cur
    };
    let mut sorted: Vec<&String> = asks.keys().collect();
    sorted.sort();
    let mut threads: Vec<(String, usize, String)> = Vec::new(); // root, turns, latest
    for id in sorted {
        let root = root_of(id);
        match threads.iter_mut().find(|t| t.0 == root) {
            Some(t) => {
                t.1 += 1;
                t.2 = id.clone();
            }
            None => threads.push((root, 1, id.clone())),
        }
    }
    threads.sort_by(|a, b| b.2.cmp(&a.2));
    threads.truncate(max);
    threads
        .into_iter()
        .map(|(root, turns, latest)| {
            let a = &asks[&latest];
            let result = a.state.get("result");
            let q = a.ask.get("question").cloned().unwrap_or(Value::Null);
            let first = asks.get(&root).and_then(|r| r.ask.get("question")).cloned().unwrap_or_else(|| q.clone());
            let preview = match result {
                Some(r) if r.get("answer").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) => clip_str(&plain(r["answer"].as_str().unwrap()), 180),
                Some(r) if r.get("error").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) => clip_str(r["error"].as_str().unwrap(), 180),
                _ => String::new(),
            };
            let mut o = Map::new();
            o.insert("id".into(), json!(latest));
            o.insert("root".into(), json!(root));
            o.insert("first".into(), first);
            o.insert("question".into(), q);
            o.insert("turns".into(), json!(turns));
            o.insert("createdAt".into(), a.ask.get("createdAt").cloned().unwrap_or(Value::Null));
            o.insert("status".into(), a.state.get("status").cloned().unwrap_or(Value::Null));
            if let Some(found) = result.and_then(|r| r.get("found")) {
                o.insert("found".into(), found.clone());
            }
            o.insert("preview".into(), json!(preview));
            Value::Object(o)
        })
        .collect()
}

/// Asks the worker to stop; a question nobody has picked up yet is cancelled right here.
pub fn cancel_ask(wiki_dir: &Path, id: &str) -> Result<Option<Value>> {
    let f = files(wiki_dir, id);
    let Some(ask) = read_json(&f.ask) else { return Ok(None) };
    if !f.result.exists() {
        write_if_missing(&f.cancel, &format!("{}\n", local_iso(&now())))?;
        if !f.claim.exists() {
            let r = json!({ "status": "cancelled", "errorKind": "aborted", "error": "Stopped.", "finishedAt": local_iso(&now()) });
            write_if_missing(&f.result, &format!("{}\n", serde_json::to_string_pretty(&r).unwrap_or_default()))?;
        }
    }
    let status = ask_state(&f, &ask).get("status").cloned().unwrap_or(Value::Null);
    Ok(Some(json!({ "id": id, "status": status })))
}
