//! The inbox: notes the apps send, accepted once fsync'd to inbox/<id>.md; the curator files them
//! later (src/inbox.mjs): submit, list, queue statistics and the pause flag (the server's side); queue
//! state, archiving and the curator's heartbeat (the curator's side).

use crate::frontmatter::{self, Meta};
use crate::text::*;
use crate::wiki::{Error, Result, assert_no_secrets, atomic_write, fsync_dir, norm_page_refs, norm_tags, norm_tags_val, normalize_app, read_if_exists, wiki_err, write_synced};
use regex::Regex;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub struct CuratorPaths {
    pub inbox: PathBuf,
    pub cur: PathBuf,
    pub keys: PathBuf,
    pub state: PathBuf,
    pub done: PathBuf,
    pub audit: PathBuf,
    pub journal: PathBuf,
    pub tmp: PathBuf,
    pub status: PathBuf,
    pub paused: PathBuf,
}

pub fn curator_paths(wiki_dir: &Path) -> CuratorPaths {
    let cur = wiki_dir.join(".curator");
    CuratorPaths {
        inbox: wiki_dir.join("inbox"),
        keys: cur.join("keys"),
        state: cur.join("state"),
        done: cur.join("done"),
        audit: cur.join("audit"),
        journal: cur.join("journal"),
        tmp: cur.join("tmp"),
        status: cur.join("status.json"),
        paused: cur.join("paused"),
        cur,
    }
}

static ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}_[0-9]{2}-[0-9]{2}-[0-9]{2}-[0-9]{3}-[a-z0-9]{6}$").unwrap());

pub fn is_note_id(id: &str) -> bool {
    ID_RE.is_match(id)
}

pub fn new_note_id(now: &Time) -> String {
    format!("{}-{}", history_stamp(now), random_hex(6))
}

pub fn month_dir(id: &str) -> PathBuf {
    let b = id.as_bytes();
    if id.len() >= 7 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-' && b[5..7].iter().all(u8::is_ascii_digit) { PathBuf::from(&id[..4]).join(&id[5..7]) } else { PathBuf::from("undated") }
}

fn read_json(file: &Path) -> Option<Value> {
    read_if_exists(file).ok().flatten().and_then(|t| serde_json::from_str(&t).ok())
}

/// Where a note id lives now: "inbox", "done" or None.
pub fn note_location(wiki_dir: &Path, id: &str) -> Option<&'static str> {
    let p = curator_paths(wiki_dir);
    if crate::wiki::confined_path(wiki_dir, &p.inbox.join(format!("{id}.md"))).is_ok_and(|p| p.exists()) {
        return Some("inbox");
    }
    if crate::wiki::confined_path(wiki_dir, &p.done.join(month_dir(id)).join(format!("{id}.md"))).is_ok_and(|p| p.exists()) {
        return Some("done");
    }
    None
}

/// Where a note's content came from (M2): user (the person said it), observed (the app checked it),
/// external (a web page, email, document or another tool's output), agent (the app's own conclusion).
pub const SOURCES: [&str; 4] = ["user", "observed", "external", "agent"];

/// A note's source as given, or "agent" (the default) when missing or unknown.
pub fn norm_source(s: &str) -> &'static str {
    let s = s.trim().to_lowercase();
    SOURCES.iter().find(|x| **x == s).copied().unwrap_or("agent")
}

#[derive(Default)]
pub struct NoteInput {
    pub kind: String,
    pub source: String,
    pub app: String,
    pub title: String,
    pub body: String,
    pub tags: Value,
    pub pages: Value,
    pub page: Option<PageInput>,
    pub idempotency_key: Option<String>,
}

#[derive(Default)]
pub struct PageInput {
    pub slug: Option<String>,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub summary: Option<String>,
    pub mode: Option<String>,
}

pub struct Submitted {
    pub id: String,
    pub rel: String,
    pub duplicate: bool,
    pub status: &'static str,
}

/// Validates, refuses secrets, deduplicates and durably queues a note.
pub fn submit_note(wiki_dir: &Path, input: &NoteInput, transport: &str, client: &str) -> Result<Submitted> {
    let kind = if input.kind == "page" { "page" } else { "log" };
    let app = normalize_app(&input.app);
    let title = one_line(&input.title);
    if title.is_empty() {
        return wiki_err("`title` is required.");
    }
    let body = to_lf(&input.body).trim().to_string();
    if kind == "page" && body.is_empty() {
        return wiki_err("`content` is required.");
    }
    let tags = norm_tags(&input.tags);
    let pages = norm_page_refs(&input.pages);
    let page = (kind == "page").then(|| {
        let p = input.page.as_ref();
        let get = |f: fn(&PageInput) -> Option<String>| p.and_then(f).unwrap_or_default();
        json!({
            "slug": get(|p| p.slug.clone()).trim().to_lowercase(),
            "title": one_line(&p.and_then(|p| p.title.clone()).unwrap_or_else(|| title.clone())),
            "type": get(|p| p.kind.clone()).trim().to_lowercase(),
            "summary": one_line(&get(|p| p.summary.clone())),
            "mode": if get(|p| p.mode.clone()) == "replace" { "replace" } else { "append" },
        })
    });
    let key = input.idempotency_key.as_deref().map(|k| take_chars(&one_line(k), 200)).unwrap_or_default();
    let (slug, summary) = page.as_ref().map(|p| (p["slug"].as_str().unwrap_or("").to_string(), p["summary"].as_str().unwrap_or("").to_string())).unwrap_or_default();
    let mut fields: Vec<(&str, &str)> = vec![];
    let tags_s = tags.join(" ");
    let pages_s = pages.join(" ");
    fields.extend([("title", title.as_str()), ("body", body.as_str()), ("tags", tags_s.as_str()), ("pages", pages_s.as_str()), ("key", key.as_str())]);
    if page.is_some() {
        fields.push(("slug", &slug));
        fields.push(("summary", &summary));
    }
    assert_no_secrets(&fields)?;

    let now = now();
    // The canonical content: JSON.stringify([kind, app, title, body, tags, pages, page ?? null]).
    let canonical = Value::Array(vec![json!(kind), json!(app), json!(title), json!(body), json!(tags), json!(pages), page.clone().unwrap_or(Value::Null)]).to_string();
    let key_hash = hash_text(&if key.is_empty() { format!("c:{}:{canonical}", local_date(&now)) } else { format!("k:{app}:{key}") }, 32);
    let p = curator_paths(wiki_dir);
    let keys = crate::wiki::confined_path(wiki_dir, &p.keys)?;
    fs::create_dir_all(&keys)?;
    let key_file = crate::wiki::confined_path(wiki_dir, &p.keys.join(format!("{key_hash}.json")))?;

    let mut id: Option<String> = None;
    let mut attempt = 0;
    while attempt < 2 && id.is_none() {
        attempt += 1;
        let candidate = new_note_id(&now);
        match write_synced(&key_file, &format!("{}\n", json!({ "id": candidate, "at": local_iso(&now) })), true) {
            Ok(()) => {
                fsync_dir(&p.keys);
                id = Some(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut prev = read_json(&key_file);
                // The first submitter may still be writing the key file: give it a moment.
                for _ in 0..50 {
                    if prev.as_ref().and_then(|v| v.get("id")).is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    prev = read_json(&key_file);
                }
                let age = fs::metadata(&key_file).ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).map(|d| d.as_millis() as i64);
                let Some(prev_id) = prev.as_ref().and_then(|v| v.get("id")).and_then(Value::as_str).map(String::from) else {
                    if age.is_some_and(|a| a < 10_000) {
                        return wiki_err("The same note is being saved right now; try again in a moment.");
                    }
                    let _ = fs::remove_file(&key_file); // unreadable and old: a crash while writing it
                    continue;
                };
                if !is_note_id(&prev_id) {
                    return wiki_err("The note's idempotency record is invalid; repair or remove that record before retrying.");
                }
                // Same note sent again. Wait briefly in case the first submit is still writing.
                for _ in 0..20 {
                    if let Some(where_) = note_location(wiki_dir, &prev_id) {
                        return Ok(Submitted { rel: format!("inbox/{prev_id}.md"), id: prev_id, duplicate: true, status: if where_ == "done" { "filed" } else { "pending" } });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if age.is_some_and(|a| a < 10_000) {
                    return Ok(Submitted { rel: format!("inbox/{prev_id}.md"), id: prev_id, duplicate: true, status: "pending" });
                }
                id = Some(prev_id); // the first submit died between the key and the note: finish it
            }
            Err(e) => return Err(e.into()),
        }
    }
    let Some(id) = id else {
        return wiki_err("Could not queue the note (idempotency key conflict); try again.");
    };

    let mut meta = Meta::default();
    meta.set("id", id.clone());
    meta.set("kind", kind);
    meta.set("app", app.clone());
    meta.set("source", norm_source(&input.source));
    meta.set("title", title.clone());
    meta.set("tags", tags);
    meta.set("pages", pages);
    meta.set("submitted", local_iso(&now));
    if !transport.is_empty() {
        meta.set("transport", transport);
    }
    if !client.is_empty() {
        meta.set("client", take_chars(&one_line(client), 80));
    }
    if !key.is_empty() {
        meta.set("idempotency_key", key.clone());
    }
    meta.set("key", key_hash);
    if let Some(pg) = &page {
        for (k, f) in [("page_slug", "slug"), ("page_title", "title"), ("page_type", "type"), ("page_summary", "summary"), ("page_mode", "mode")] {
            meta.set(k, pg[f].as_str().unwrap_or("").to_string());
        }
    }
    let content = frontmatter::serialize(&meta, if body.is_empty() { &title } else { &body });
    let file = crate::wiki::confined_path(wiki_dir, &p.inbox.join(format!("{id}.md")))?;
    let tmp = crate::wiki::confined_path(wiki_dir, &p.tmp)?;
    atomic_write(&file, &content, Some(&tmp), None)?;
    fault_point("submit-after-write");
    Ok(Submitted { rel: format!("inbox/{id}.md"), id, duplicate: false, status: "pending" })
}

/// Test hook: AGENT_WIKI_FAULT=<point> makes the process hang there so a test can kill it mid-operation.
pub fn fault_point(name: &str) {
    if std::env::var("AGENT_WIKI_FAULT").ok().as_deref() == Some(name) {
        eprintln!("[agent-wiki] FAULT {name}: hanging");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Note {
    pub id: String,
    pub rel: String,
    pub kind: String,
    pub app: String,
    /// user | observed | external | agent (see SOURCES).
    pub source: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub pages: Vec<String>,
    pub page: Option<Value>,
    pub client: String,
    pub submitted: String,
    pub date: String,
    pub time: String,
    pub ms: i64,
    pub mtime_ms: i64,
    pub hash: String,
    /// Dropped into inbox/ by hand (no id in its frontmatter).
    pub by_hand: bool,
    pub attempts: i64,
    pub last_error: String,
    pub next_at: i64,
    pub isolate: bool,
    pub status: String,
}

fn note_from_text(id: &str, text: &str, mtime: i64) -> Note {
    let (meta, body) = frontmatter::parse(text);
    let submitted = meta.str("submitted");
    let ms = parse_ms(&submitted).unwrap_or(mtime);
    let d = from_ms(ms);
    static H: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#\s+(.+)$").unwrap());
    let heading = H.captures(&body).map(|c| c[1].to_string()).unwrap_or_default();
    let page = (meta.str("kind") == "page").then(|| {
        json!({
            "slug": meta.str("page_slug"), "title": meta.str("page_title"), "type": meta.str("page_type"),
            "summary": meta.str("page_summary"), "mode": if meta.str("page_mode") == "replace" { "replace" } else { "append" },
        })
    });
    let title = [meta.str("title"), heading, id.to_string()].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
    let app = meta.str("app");
    Note {
        id: id.to_string(),
        rel: format!("inbox/{id}.md"),
        kind: if meta.str("kind") == "page" { "page".into() } else { "log".into() },
        app: normalize_app(if app.is_empty() { "human" } else { &app }),
        source: norm_source(&meta.str("source")).to_string(),
        title: one_line(&title),
        body: body.trim().to_string(),
        tags: norm_tags_val(meta.get("tags")),
        pages: match meta.get("pages") {
            Some(frontmatter::Val::List(l)) => norm_page_refs(&Value::Array(l.iter().cloned().map(Value::String).collect())),
            Some(frontmatter::Val::Str(s)) => norm_page_refs(&Value::String(s.clone())),
            None => vec![],
        },
        page,
        client: meta.str("client"),
        submitted: local_iso(&d),
        date: local_date(&d),
        time: local_hm(&d),
        ms,
        mtime_ms: mtime,
        hash: hash_text(text, 16),
        by_hand: !meta.has("id") || meta.str("id").is_empty(),
        attempts: 0,
        last_error: String::new(),
        next_at: 0,
        isolate: false,
        status: "pending".into(),
    }
}

/// Pending notes (oldest first), each with its queue state: pending | retrying | dead.
pub fn list_notes(wiki_dir: &Path) -> Result<Vec<Note>> {
    let p = curator_paths(wiki_dir);
    let rd = match fs::read_dir(crate::wiki::confined_path(wiki_dir, &p.inbox)?) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut notes = Vec::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with('.') {
            continue;
        }
        let Some(id) = n.strip_suffix(".md") else { continue };
        let Ok(path) = crate::wiki::confined_path(wiki_dir, &e.path()) else { continue };
        let Some((len, mtime)) = crate::readcache::stamp(&e) else { continue };
        let Some(text) = crate::readcache::read(&path, len, mtime) else { continue }; // filed or removed mid-read
        let mut note = note_from_text(id, &to_lf(&text), (mtime / 1_000_000) as i64);
        let state = crate::wiki::confined_path(wiki_dir, &p.state.join(format!("{id}.json"))).ok().and_then(|p| read_json(&p)).unwrap_or(Value::Null);
        note.attempts = state.get("attempts").and_then(Value::as_i64).unwrap_or(0);
        note.last_error = state.get("lastError").and_then(Value::as_str).unwrap_or("").to_string();
        note.next_at = state.get("nextAt").and_then(Value::as_f64).map(|x| x as i64).unwrap_or(0);
        note.isolate = state.get("isolate").and_then(Value::as_bool).unwrap_or(false);
        let dead = state.get("dead").and_then(Value::as_bool).unwrap_or(false);
        note.status = if dead {
            "dead"
        } else if note.attempts > 0 {
            "retrying"
        } else {
            "pending"
        }
        .into();
        notes.push(note);
    }
    notes.sort_by(|a, b| a.ms.cmp(&b.ms).then_with(|| collate_cmp(&a.id, &b.id)));
    Ok(notes)
}

/// Notes the curator has filed (.curator/done/YYYY/MM), each with its rel there.
pub fn filed_notes(wiki_dir: &Path) -> Vec<Note> {
    let Ok(done) = crate::wiki::confined_path(wiki_dir, &curator_paths(wiki_dir).done) else { return vec![] };
    let mut notes = vec![];
    for y in fs::read_dir(&done).into_iter().flatten().flatten() {
        if crate::wiki::confined_path(wiki_dir, &y.path()).is_err() {
            continue;
        }
        for m in fs::read_dir(y.path()).into_iter().flatten().flatten() {
            if crate::wiki::confined_path(wiki_dir, &m.path()).is_err() {
                continue;
            }
            for e in fs::read_dir(m.path()).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let Some(id) = name.strip_suffix(".md").filter(|id| is_note_id(id)) else { continue };
                let Ok(path) = crate::wiki::confined_path(wiki_dir, &e.path()) else { continue };
                let Some((len, mtime)) = crate::readcache::stamp(&e) else { continue };
                let Some(text) = crate::readcache::read(&path, len, mtime) else { continue };
                let mut n = note_from_text(id, &to_lf(&text), (mtime / 1_000_000) as i64);
                n.rel = format!(".curator/done/{}/{}/{name}", y.file_name().to_string_lossy(), m.file_name().to_string_lossy());
                notes.push(n);
            }
        }
    }
    notes
}

/// A note wherever it is (inbox or filed), as (rel, text): text is a status line, then the note file.
pub fn read_note(wiki_dir: &Path, id: &str) -> Result<(String, String)> {
    if !is_note_id(id) {
        return wiki_err(format!("\"note:{id}\" is not a note id (they look like note:2026-10-03_15-22-10-118-a1b2c3)."));
    }
    let p = curator_paths(wiki_dir);
    let month = month_dir(id);
    let (rel, file, status) = match note_location(wiki_dir, id) {
        Some("inbox") => (format!("inbox/{id}.md"), p.inbox.join(format!("{id}.md")), "Pending: the curator has not filed this note yet.".to_string()),
        Some(_) => {
            let audit_file = crate::wiki::confined_path(wiki_dir, &p.audit.join(&month).join(format!("{id}.json")))?;
            let audit = read_json(&audit_file).unwrap_or(Value::Null);
            let mut s = format!("Filed by the curator{}", audit["at"].as_str().map(|at| format!(" at {at}")).unwrap_or_default());
            if let Some(d) = audit["disposition"].as_str() {
                s.push_str(&format!(": {d}"));
            }
            if let Some(r) = audit["reason"].as_str().filter(|r| !r.is_empty()) {
                s.push_str(&format!(" ({r})"));
            }
            let pages: Vec<String> = audit["changes"].as_array().into_iter().flatten().filter_map(|c| c["slug"].as_str()).map(|s| format!("[[{s}]]")).collect();
            if !pages.is_empty() {
                s.push_str(&format!("; pages changed: {}", pages.join(", ")));
            }
            s.push('.');
            let rel_month = month.to_string_lossy().replace('\\', "/");
            (format!(".curator/done/{rel_month}/{id}.md"), p.done.join(&month).join(format!("{id}.md")), s)
        }
        None => return wiki_err(format!("No note {id}: it is not in the inbox or among the filed notes.")),
    };
    let text = read_if_exists(&crate::wiki::confined_path(wiki_dir, &file)?)?.map(|t| to_lf(&t)).unwrap_or_default();
    Ok((rel, format!("{status}\n\n{text}")))
}

pub fn queue_stats(wiki_dir: &Path) -> Value {
    let notes = list_notes(wiki_dir).unwrap_or_default();
    let count = |s: &str| notes.iter().filter(|n| n.status == s).count();
    json!({
        "pending": notes.len() - count("dead"),
        "retrying": count("retrying"),
        "dead": count("dead"),
        "oldestPendingAt": notes.iter().find(|n| n.status != "dead").map(|n| n.submitted.clone()),
        "newestAt": notes.last().map(|n| n.submitted.clone()),
    })
}

pub fn read_curator_status(wiki_dir: &Path) -> Option<Value> {
    crate::wiki::confined_path(wiki_dir, &curator_paths(wiki_dir).status).ok().and_then(|p| read_json(&p))
}

pub fn is_paused(wiki_dir: &Path) -> bool {
    curator_paths(wiki_dir).paused.exists()
}

pub fn set_paused(wiki_dir: &Path, paused: bool) -> Result<()> {
    let p = curator_paths(wiki_dir);
    let flag = crate::wiki::confined_path(wiki_dir, &p.paused)?;
    if paused {
        fs::create_dir_all(flag.parent().unwrap())?;
        fs::write(&flag, format!("paused at {}\n", local_iso(&now())))?;
    } else {
        match fs::remove_file(&flag) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- queue state (curator side)

fn write_pretty(wiki_dir: &Path, file: &Path, v: &Value) -> Result<()> {
    let file = crate::wiki::confined_path(wiki_dir, file)?;
    let tmp = crate::wiki::confined_path(wiki_dir, &curator_paths(wiki_dir).tmp)?;
    atomic_write(
        &file,
        &format!(
            "{}
",
            serde_json::to_string_pretty(v).unwrap_or_default()
        ),
        Some(&tmp),
        None,
    )
}

/// A note's queue state ({} when it has none).
pub fn read_state(wiki_dir: &Path, id: &str) -> serde_json::Map<String, Value> {
    let Ok(file) = crate::wiki::confined_path(wiki_dir, &curator_paths(wiki_dir).state.join(format!("{id}.json"))) else { return serde_json::Map::new() };
    match read_json(&file) {
        Some(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    }
}

pub fn write_state(wiki_dir: &Path, id: &str, state: &Value) -> Result<()> {
    write_pretty(wiki_dir, &curator_paths(wiki_dir).state.join(format!("{id}.json")), state)
}

pub fn clear_state(wiki_dir: &Path, id: &str) {
    if let Ok(file) = crate::wiki::confined_path(wiki_dir, &curator_paths(wiki_dir).state.join(format!("{id}.json"))) {
        let _ = fs::remove_file(file);
    }
}

/// Moves a filed note to the archive and writes its audit record. Idempotent.
pub fn archive_note(wiki_dir: &Path, id: &str, audit: Option<&Value>) -> Result<()> {
    let p = curator_paths(wiki_dir);
    let dest = crate::wiki::confined_path(wiki_dir, &p.done.join(month_dir(id)).join(format!("{id}.md")))?;
    let src = crate::wiki::confined_path(wiki_dir, &p.inbox.join(format!("{id}.md")))?;
    if let Some(a) = audit {
        write_pretty(wiki_dir, &p.audit.join(month_dir(id)).join(format!("{id}.json")), a)?;
    }
    let dest_dir = dest.parent().unwrap_or(&p.done).to_path_buf();
    fs::create_dir_all(&dest_dir)?;
    let mut i = 0u64;
    loop {
        match fs::rename(&src, &dest) {
            Ok(()) => {
                fsync_dir(&dest_dir);
                fsync_dir(&p.inbox);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break, // already archived (recovery re-run)
            Err(e) if i < 20 && (e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33) | Some(16))) => {
                std::thread::sleep(std::time::Duration::from_millis(20 + i * 20));
                i += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
    clear_state(wiki_dir, id);
    Ok(())
}

/// Resets dead (or, with `only_dead` false, all failed) notes so the curator tries them again. Returns how many.
pub fn retry_notes(wiki_dir: &Path, only_dead: bool) -> Result<usize> {
    let mut n = 0;
    for note in list_notes(wiki_dir)? {
        if if only_dead { note.status != "dead" } else { note.attempts == 0 } {
            continue;
        }
        clear_state(wiki_dir, &note.id);
        n += 1;
    }
    Ok(n)
}

pub fn write_curator_status(wiki_dir: &Path, status: &Value) -> Result<()> {
    write_pretty(wiki_dir, &curator_paths(wiki_dir).status, status)
}

/// Test hook: AGENT_WIKI_TOUCH=<point> and AGENT_WIKI_TOUCH_FILE=<page file> append a line to that file
/// once, at that point, the way an editor saving a page at the worst moment would.
pub fn touch_point(name: &str) {
    static TOUCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if std::env::var("AGENT_WIKI_TOUCH").ok().as_deref() != Some(name) {
        return;
    }
    let Some(file) = std::env::var_os("AGENT_WIKI_TOUCH_FILE").filter(|f| !f.is_empty()) else { return };
    if TOUCHED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = f.write_all(
            b"
Saved in an editor during the commit.
",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_idempotency_record_cannot_escape_the_inbox() {
        let root = std::env::temp_dir().join(format!("aw-key-boundary-{}-{}", std::process::id(), random_hex(6)));
        let wiki = root.join("wiki");
        let p = curator_paths(&wiki);
        fs::create_dir_all(&p.keys).unwrap();
        let key = p.keys.join(format!("{}.json", hash_text("k:audit:retry", 32)));
        fs::write(&key, json!({ "id": "../../escaped" }).to_string()).unwrap();
        fs::File::options().write(true).open(&key).unwrap().set_modified(std::time::UNIX_EPOCH).unwrap();
        let input = NoteInput { app: "audit".into(), title: "Synthetic note".into(), body: "Harmless test content".into(), idempotency_key: Some("retry".into()), ..Default::default() };
        assert!(submit_note(&wiki, &input, "", "").is_err());
        assert!(!root.join("escaped.md").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn inbox_reads_and_submission_reject_linked_files_and_directories() {
        let root = std::env::temp_dir().join(format!("aw-inbox-links-{}-{}", std::process::id(), random_hex(6)));
        let wiki = root.join("wiki");
        let outside = root.join("outside");
        let p = curator_paths(&wiki);
        fs::create_dir_all(&p.inbox).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let id = "2026-10-03_12-00-00-000-abcdef";
        let external = outside.join(format!("{id}.md"));
        fs::write(&external, "outside synthetic content").unwrap();
        std::os::unix::fs::symlink(&external, p.inbox.join(format!("{id}.md"))).unwrap();
        assert!(list_notes(&wiki).unwrap().is_empty());
        assert!(read_note(&wiki, id).is_err());
        fs::create_dir_all(&p.done).unwrap();
        std::os::unix::fs::symlink(&outside, p.done.join("2026")).unwrap();
        assert!(filed_notes(&wiki).is_empty());
        std::os::unix::fs::symlink(&outside, &p.keys).unwrap();
        let input = NoteInput { app: "audit".into(), title: "Synthetic note".into(), ..Default::default() };
        assert!(submit_note(&wiki, &input, "", "").is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        fs::remove_file(&p.keys).unwrap();
        fs::remove_file(p.inbox.join(format!("{id}.md"))).unwrap();
        fs::remove_dir(&p.inbox).unwrap();
        std::os::unix::fs::symlink(&outside, &p.inbox).unwrap();
        assert!(submit_note(&wiki, &input, "", "").is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_note_is_read_and_found_wherever_it_is() {
        let wiki = std::env::temp_dir().join(format!("aw-notes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&wiki);
        let p = curator_paths(&wiki);
        let (pending, filed) = ("2026-10-03_15-22-10-118-a1b2c3", "2026-09-30_09-00-00-000-ffffff");
        fs::create_dir_all(&p.inbox).unwrap();
        fs::write(p.inbox.join(format!("{pending}.md")), "---\nid: x\napp: codex\ntitle: Pending one\n---\n\nquokka facts\n").unwrap();
        let done = p.done.join("2026").join("09");
        fs::create_dir_all(&done).unwrap();
        fs::write(done.join(format!("{filed}.md")), "---\nid: y\napp: claude-code\ntitle: Filed one\nsubmitted: 2026-09-30T09:00:00-05:00\n---\n\nwombat facts\n").unwrap();
        let audit = p.audit.join("2026").join("09");
        fs::create_dir_all(&audit).unwrap();
        fs::write(audit.join(format!("{filed}.json")), r#"{"at":"2026-09-30T09:01:00-05:00","disposition":"integrated","reason":"filed into zoo","changes":[{"slug":"zoo"}]}"#).unwrap();

        let (rel, text) = read_note(&wiki, pending).unwrap();
        assert_eq!(rel, format!("inbox/{pending}.md"));
        assert!(text.starts_with("Pending: ") && text.contains("quokka facts"), "{text}");
        let (rel, text) = read_note(&wiki, filed).unwrap();
        assert_eq!(rel, format!(".curator/done/2026/09/{filed}.md"));
        assert!(text.starts_with("Filed by the curator at 2026-09-30T09:01:00-05:00: integrated (filed into zoo); pages changed: [[zoo]]."), "{text}");
        assert!(read_note(&wiki, "2026-01-01_00-00-00-000-000000").is_err());
        assert!(read_note(&wiki, "../x").is_err());

        let filed_notes = filed_notes(&wiki);
        assert_eq!(filed_notes.len(), 1);
        assert_eq!(filed_notes[0].id, filed);
        assert_eq!(filed_notes[0].rel, format!(".curator/done/2026/09/{filed}.md"));
        assert_eq!(filed_notes[0].app, "claude-code");
        let _ = fs::remove_dir_all(&wiki);
    }
}
