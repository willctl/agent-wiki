//! Changes that wait for the person (M2): page changes the curator's trust gate held and the scheduled
//! cleanups, which the window approves, rejects or sends back to the curator; and reverting one page
//! change of a batch. With approvals set to "auto" (settings.rs, the default), the curator applies
//! them at once instead (auto_apply), logs each one for the tray's notification, and the window can
//! undo it.
//!
//! Held changes live in .curator/held/<batch>.json until every change in the file is decided, then
//! move to .curator/held/done/. Decisions run in the server, under the wiki write lock, and keep the
//! previous page version in .history/ like every other write.

use crate::curator::{HeldOp, apply_edits, page_write};
use crate::frontmatter;
use crate::inbox::{self, curator_paths, is_note_id, month_dir};
use crate::text::*;
use crate::wiki::{self, SLUG_RE, atomic_write, read_if_exists};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(flatten)]
    pub held: HeldOp,
    /// pending | approved | rejected | refiled | undone
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Approved without the person (approvals "auto").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    /// After an approval: the page's hash as written, and the .history copy of what it replaced (none
    /// for a new page) with that copy's hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undone_at: Option<String>,
    /// Why approvals "auto" could not apply it (it waits for the person instead); logged once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_failed: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct File {
    pub v: i64,
    pub batch: String,
    pub at: String,
    pub changes: Vec<Entry>,
}

/// Why a decision did not happen: unknown change, the page moved on (the message says what to do), or the wiki failed.
#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Conflict(String),
    Wiki(wiki::Error),
}

impl From<wiki::Error> for Error {
    fn from(e: wiki::Error) -> Self {
        Error::Wiki(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Wiki(e.into())
    }
}

fn held_dir(wiki_dir: &Path) -> PathBuf {
    curator_paths(wiki_dir).cur.join("held")
}

fn held_file(wiki_dir: &Path, batch: &str) -> PathBuf {
    held_dir(wiki_dir).join(format!("{batch}.json"))
}

fn page_file(wiki_dir: &Path, slug: &str) -> wiki::Result<PathBuf> {
    wiki::confined_path(wiki_dir, &wiki_dir.join("pages").join(format!("{slug}.md")))
}

/// History references come from editable JSON, not from a trusted journal in memory.
fn valid_history_rel(slug: &str, rel: &str) -> bool {
    let prefix = format!(".history/pages/{slug}/");
    let Some(name) = rel.strip_prefix(&prefix) else { return false };
    name.ends_with(".md") && name.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn read_history(wiki_dir: &Path, slug: &str, rel: &str) -> wiki::Result<Option<String>> {
    if !valid_history_rel(slug, rel) {
        return wiki::wiki_err("Refused: the earlier version must be in this page's .history folder.");
    }
    let path = wiki::confined_path(wiki_dir, &wiki_dir.join(rel))?;
    read_if_exists(&path)
}

fn write_json(wiki_dir: &Path, file: &Path, v: &impl Serialize) -> wiki::Result<()> {
    let file = wiki::confined_path(wiki_dir, file)?;
    let tmp = wiki::confined_path(wiki_dir, &curator_paths(wiki_dir).tmp)?;
    atomic_write(&file, &format!("{}\n", serde_json::to_string_pretty(v).unwrap_or_default()), Some(&tmp), None)
}

/// Records a batch's held changes (called by the curator's commit, under the write lock). Idempotent:
/// a recovery that runs again keeps the file, and any decisions, as they are.
pub fn record(wiki_dir: &Path, batch: &str, at: &str, held: &[HeldOp]) -> wiki::Result<()> {
    let file = wiki::confined_path(wiki_dir, &held_file(wiki_dir, batch))?;
    let done = wiki::confined_path(wiki_dir, &held_dir(wiki_dir).join("done").join(format!("{batch}.json")))?;
    if file.exists() || done.exists() {
        return Ok(());
    }
    fs::create_dir_all(held_dir(wiki_dir))?;
    let f = File {
        v: 1,
        batch: batch.into(),
        at: at.into(),
        changes: held
            .iter()
            .map(|h| Entry {
                held: h.clone(),
                status: "pending".into(),
                decided_at: None,
                outcome: None,
                auto: false,
                applied_hash: None,
                replaced: None,
                replaced_hash: None,
                undone_at: None,
                auto_failed: None,
            })
            .collect(),
    };
    write_json(wiki_dir, &file, &f)
}

/// A held file, if it is one: its batch matches its name, and every change names a real page slug
/// (the same one its plan operation does) and real note ids. A file someone planted or damaged
/// could otherwise point a write outside pages/, and approvals "auto" needs no person to apply it.
fn parse_file(path: &Path, batch: &str) -> Option<File> {
    let f: File = serde_json::from_str(&read_if_exists(path).ok().flatten()?).ok()?;
    let fits = |e: &Entry| {
        let w = &e.held.write;
        SLUG_RE.is_match(&w.slug)
            && e.held.op["slug"].as_str() == Some(w.slug.as_str())
            && w.note_ids.iter().all(|id| is_note_id(id))
            && e.replaced.as_deref().is_none_or(|rel| valid_history_rel(&w.slug, rel))
            && e.replaced.is_some() == e.replaced_hash.is_some()
    };
    (f.batch == batch && is_note_id(batch) && f.changes.iter().all(fits)).then_some(f)
}

fn load(wiki_dir: &Path, batch: &str) -> Option<File> {
    parse_file(&wiki::confined_path(wiki_dir, &held_file(wiki_dir, batch)).ok()?, batch)
}

/// A batch's file wherever it is now: waiting (held/) or all decided (held/done/).
fn load_any(wiki_dir: &Path, batch: &str) -> Option<(PathBuf, File)> {
    [held_file(wiki_dir, batch), held_dir(wiki_dir).join("done").join(format!("{batch}.json"))].into_iter().find_map(|p| {
        let p = wiki::confined_path(wiki_dir, &p).ok()?;
        parse_file(&p, batch).map(|f| (p, f))
    })
}

fn pending_files(wiki_dir: &Path) -> Vec<File> {
    let mut names: Vec<String> = fs::read_dir(held_dir(wiki_dir)).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".json")).collect();
    names.sort();
    names.iter().filter_map(|n| load(wiki_dir, n.trim_end_matches(".json"))).collect()
}

/// How many changes wait for the person (the /status attention state).
pub fn pending_count(wiki_dir: &Path) -> usize {
    pending_files(wiki_dir).iter().map(|f| f.changes.iter().filter(|c| c.status == "pending").count()).sum()
}

fn body_of(text: &str) -> String {
    frontmatter::parse(text).1.trim().to_string()
}

/// The page's header fields that a change touches, as "field: value" lines (for the diff).
fn header_lines(text: &str) -> Vec<String> {
    let (meta, _) = frontmatter::parse(text);
    ["title", "type", "summary"].iter().map(|k| format!("{k}: {}", meta.str(k))).chain(["tags", "aliases"].iter().map(|k| format!("{k}: {}", wiki::norm_tags_val(meta.get(k)).join(", ")))).collect()
}

/// A change as the window shows it: what, why it waits, the notes behind it, and a line diff.
fn view(f: &File, index: usize, e: &Entry, wiki_dir: &Path) -> Value {
    let w = &e.held.write;
    let base = w.base_text.as_deref().unwrap_or("");
    let cur = page_file(wiki_dir, &w.slug).and_then(|p| read_if_exists(&p)).ok().flatten();
    let stale = match &w.base_hash {
        None => cur.is_some(),
        Some(h) => cur.as_deref().map(|c| hash_text(c, 16)).as_deref() != Some(h.as_str()),
    };
    let (old_body, new_body) = (body_of(base), body_of(&w.text));
    let diff: Vec<Value> = line_diff(&old_body, &new_body).into_iter().map(|(k, l)| json!([k.to_string(), l])).collect();
    let (old_head, new_head) = (if base.is_empty() { vec![] } else { header_lines(base) }, header_lines(&w.text));
    let header: Vec<Value> = new_head.iter().filter(|l| !old_head.contains(l)).map(|l| json!(l)).collect();
    json!({
        "batch": f.batch, "index": index, "at": f.at, "slug": w.slug, "title": w.title, "action": w.action, "kind": e.held.kind,
        "reason": w.reason, "reasons": e.held.reasons, "notes": e.held.notes, "status": e.status, "autoFailed": e.auto_failed,
        "stale": stale, "header": header, "diff": diff,
    })
}

/// The changes waiting for the person, oldest first.
pub fn list(wiki_dir: &Path) -> Vec<Value> {
    pending_files(wiki_dir).iter().flat_map(|f| f.changes.iter().enumerate().filter(|(_, c)| c.status == "pending").map(move |(i, c)| (f, i, c))).map(|(f, i, c)| view(f, i, c, wiki_dir)).collect()
}

fn check_batch(batch: &str) -> Result<(), Error> {
    if is_note_id(batch) { Ok(()) } else { Err(Error::NotFound(format!("no batch \"{}\"", take_chars(batch, 40)))) }
}

fn log_line(wiki_dir: &Path, app: &str, text: &str) -> wiki::Result<()> {
    let now = now();
    wiki::append_to_log(wiki_dir, &now, &format!("- {} · {app} · {text}", local_hm(&now)), true).map(|_| ())
}

/// Keeps the page as it is now in .history/ before it is overwritten or removed.
fn keep_history(wiki_dir: &Path, slug: &str, text: &str, suffix: &str) -> wiki::Result<String> {
    let rel = format!(".history/pages/{slug}/{}-{suffix}.md", history_stamp(&now()));
    let file = wiki::confined_path(wiki_dir, &wiki_dir.join(&rel))?;
    fs::create_dir_all(file.parent().unwrap_or(wiki_dir))?;
    wiki::write_if_missing(&file, text)?;
    Ok(rel)
}

/// Approves, rejects or sends back ("refile": the notes go back to the inbox, so the curator plans
/// them again on the current pages) one held change.
pub fn decide(wiki_dir: &Path, batch: &str, index: usize, action: &str) -> Result<Value, Error> {
    check_batch(batch)?;
    if !["approve", "reject", "refile"].contains(&action) {
        return Err(Error::NotFound(format!("unknown action \"{}\"", take_chars(action, 20))));
    }
    let mut out: Result<Value, Error> = Err(Error::NotFound("no such change".into()));
    wiki::with_lock(wiki_dir, || {
        out = decide_locked(wiki_dir, batch, index, action, false);
        Ok(())
    })?;
    out
}

fn message(e: Error) -> String {
    match e {
        Error::NotFound(m) | Error::Conflict(m) => m,
        Error::Wiki(e) => e.to_string(),
    }
}

/// The page as it is now, if the held change still fits it (what approving checks first).
fn check_fits(wiki_dir: &Path, e: &Entry) -> Result<Option<String>, Error> {
    let w = &e.held.write;
    let cur = read_if_exists(&page_file(wiki_dir, &w.slug)?)?;
    let op = &e.held.op;
    let moved_on = "The page changed since this was proposed. Send it back to the curator to propose it again on the current page, or reject it.";
    match (&cur, op["action"].as_str()) {
        (Some(_), Some("create")) => Err(Error::Conflict("A page with this name was created since. Reject this change, or send it back to the curator.".into())),
        (None, Some("patch" | "replace")) => Err(Error::Conflict("The page no longer exists. Reject this change, or send it back to the curator.".into())),
        // A patch applies wherever its exact-match edits still do; a replace only to the page it was planned on.
        (Some(c), Some("patch")) if apply_edits(&frontmatter::parse(c).1, op["edits"].as_array().map(Vec::as_slice).unwrap_or(&[])).is_err() => Err(Error::Conflict(moved_on.into())),
        (Some(c), Some("replace")) if Some(hash_text(c, 16)) != w.base_hash => Err(Error::Conflict(moved_on.into())),
        _ => Ok(cur),
    }
}

/// Applies a batch's waiting changes without the person (approvals automatic). The caller holds the
/// write lock and read the mode under it, so the log and the pages agree on what happened. A cleanup
/// goes in whole or not at all (a split is a patch plus a new page). A change that no longer fits its
/// page keeps waiting, and the log says so once. Returns the slugs it wrote.
pub fn apply_auto_locked(wiki_dir: &Path, batch: &str) -> wiki::Result<Vec<String>> {
    let Some(f) = load(wiki_dir, batch) else { return Ok(vec![]) };
    let pending: Vec<usize> = f.changes.iter().enumerate().filter(|(_, e)| e.status == "pending").map(|(i, _)| i).collect();
    let cleanup = f.changes.iter().any(|e| e.held.kind == "lint");
    let mut misfits: Vec<(usize, String)> = vec![];
    if cleanup {
        for &i in &pending {
            if let Err(e) = check_fits(wiki_dir, &f.changes[i]) {
                misfits.push((i, message(e)));
            }
        }
    }
    let mut applied = vec![];
    if misfits.is_empty() {
        for &i in &pending {
            match decide_locked(wiki_dir, batch, i, "approve", true) {
                Ok(v) => applied.extend(v["slug"].as_str().map(String::from).filter(|_| v["changed"].as_bool() == Some(true))),
                Err(Error::Wiki(e)) => return Err(e),
                Err(e) => misfits.push((i, message(e))),
            }
        }
    }
    if !misfits.is_empty()
        && let Some(mut f) = load(wiki_dir, batch)
    {
        for (i, why) in misfits {
            let Some(e) = f.changes.get_mut(i).filter(|e| e.auto_failed.is_none()) else { continue };
            let what = if cleanup { "Cleanup" } else { "Change" };
            log_line(wiki_dir, "curator", &format!("{what} waiting for your OK: {} [[{}]] (not applied automatically: {})", one_line(&e.held.write.title), e.held.write.slug, one_line(&why)))?;
            e.auto_failed = Some(why);
        }
        write_json(wiki_dir, &held_file(wiki_dir, batch), &f)?;
    }
    Ok(applied)
}

/// Applies everything waiting (the curator's start, a switch back to automatic), one batch at a time
/// under the write lock, and only while approvals are still automatic: "Ask me first" stops it
/// between batches. Returns how many changes it applied.
pub fn auto_apply_all(wiki_dir: &Path) -> wiki::Result<usize> {
    let mut n = 0;
    for f in pending_files(wiki_dir).into_iter().filter(|f| f.changes.iter().any(|c| c.status == "pending")) {
        n += wiki::with_lock(wiki_dir, || {
            if crate::settings::approvals(wiki_dir) != crate::settings::Approvals::Auto {
                return Ok(0);
            }
            apply_auto_locked(wiki_dir, &f.batch).map(|a| a.len())
        })?;
    }
    Ok(n)
}

/// Changes applied automatically since `since_ms`, newest first, read from the held files themselves
/// (a decision rewrites its file, so older files are skipped by their time).
fn auto_applied(wiki_dir: &Path, since_ms: i64) -> Vec<(std::rc::Rc<File>, usize, i64)> {
    let mut out = vec![];
    for dir in [held_dir(wiki_dir), held_dir(wiki_dir).join("done")] {
        for d in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = d.file_name().to_string_lossy().to_string();
            let Some(batch) = name.strip_suffix(".json") else { continue };
            let modified = d.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|t| t.as_millis() as i64);
            if modified.is_some_and(|t| t < since_ms) {
                continue;
            }
            let Ok(path) = wiki::confined_path(wiki_dir, &d.path()) else { continue };
            let Some(f) = parse_file(&path, batch).map(std::rc::Rc::new) else { continue };
            for (i, e) in f.changes.iter().enumerate() {
                if let Some(at) = e.decided_at.as_deref().and_then(parse_ms).filter(|t| e.auto && e.applied_hash.is_some() && *t >= since_ms) {
                    out.push((f.clone(), i, at));
                }
            }
        }
    }
    out.sort_by_key(|x| std::cmp::Reverse(x.2));
    out
}

/// Why an approved change cannot be undone now, or None.
fn undo_problem(wiki_dir: &Path, e: &Entry) -> Option<String> {
    let Some(applied) = e.applied_hash.as_deref().filter(|_| e.status == "approved") else {
        return Some(format!("This change is {}, so there is nothing to undo.", e.status));
    };
    let cur = page_file(wiki_dir, &e.held.write.slug).and_then(|p| read_if_exists(&p)).ok().flatten();
    if cur.as_deref().map(|c| hash_text(c, 16)).as_deref() != Some(applied) {
        return Some("The page changed after this change, so it cannot simply be undone. Edit the page, or ask the curator.".into());
    }
    let earlier = match e.replaced.as_deref().map(|rel| read_history(wiki_dir, &e.held.write.slug, rel)).transpose() {
        Ok(t) => t,
        Err(e) => return Some(e.to_string()),
    };
    match (earlier, &e.replaced_hash) {
        (Some(None), _) => Some("The earlier version is missing from .history/.".into()),
        (Some(Some(t)), Some(h)) if &hash_text(&t, 16) != h => Some("The earlier version in .history/ was changed.".into()),
        _ => None,
    }
}

/// What was applied automatically in the last `days`, newest first: each change as the Inbox shows
/// it, with when, whether it was undone, and whether Undo can still work.
pub fn recent_auto(wiki_dir: &Path, days: i64) -> Vec<Value> {
    auto_applied(wiki_dir, now_ms() - days * 86_400_000)
        .into_iter()
        .map(|(f, i, _)| {
            let e = &f.changes[i];
            let mut v = view(&f, i, e, wiki_dir);
            v["at"] = json!(e.decided_at);
            v["undoneAt"] = json!(e.undone_at);
            v["undoable"] = json!(undo_problem(wiki_dir, e).is_none());
            v
        })
        .collect()
}

/// For /status and the tray's notification: the changes applied automatically in the last 7 days
/// (how many, the newest, and their times, newest first), so a tray announces each one once.
pub fn auto_summary(wiki_dir: &Path) -> Value {
    let a = auto_applied(wiki_dir, now_ms() - 7 * 86_400_000);
    let last = a.first().map(|(f, i, _)| {
        let e = &f.changes[*i];
        json!({ "at": e.decided_at, "batch": f.batch, "index": i, "slug": e.held.write.slug, "title": one_line(&e.held.write.title), "kind": if e.held.kind.is_empty() { "held" } else { e.held.kind.as_str() } })
    });
    json!({ "count": a.len(), "last": last, "times": a.iter().take(100).map(|x| x.2).collect::<Vec<_>>() })
}

/// Puts a page back: the current text goes to .history/, then the earlier text replaces it (only if
/// the page is still `cur_hash`), or the page is removed when it did not exist before.
fn restore_page(wiki_dir: &Path, slug: &str, cur: &str, cur_hash: &str, earlier: Option<&str>, suffix: &str) -> Result<(), Error> {
    keep_history(wiki_dir, slug, cur, suffix)?;
    let file = page_file(wiki_dir, slug)?;
    match earlier {
        Some(text) => atomic_write(&file, text, None, Some(Some(cur_hash)))?,
        None => fs::remove_file(&file)?,
    }
    Ok(())
}

/// Undoes an approved change (automatic or not) if the page is still what the approval wrote: the
/// page it replaced comes back from .history/ (a page the change created is removed). A cleanup is
/// undone as a whole, every page it changed, so a split never leaves a link to a removed page.
pub fn undo(wiki_dir: &Path, batch: &str, index: usize) -> Result<Value, Error> {
    check_batch(batch)?;
    let mut out: Result<Value, Error> = Err(Error::NotFound("no such change".into()));
    wiki::with_lock(wiki_dir, || {
        out = (|| {
            let Some((path, mut f)) = load_any(wiki_dir, batch) else { return Err(Error::NotFound(format!("no changes for batch {batch}"))) };
            let Some(e) = f.changes.get(index) else { return Err(Error::NotFound("no such change".into())) };
            let cleanup = e.held.kind == "lint";
            let targets: Vec<usize> = if cleanup { (0..f.changes.len()).filter(|&i| f.changes[i].status == "approved" && f.changes[i].applied_hash.is_some()).collect() } else { vec![index] };
            if let Some(why) = undo_problem(wiki_dir, e) {
                return Err(Error::Conflict(why));
            }
            for &i in &targets {
                if let Some(why) = undo_problem(wiki_dir, &f.changes[i]) {
                    return Err(Error::Conflict(format!("{}: {why}", f.changes[i].held.write.slug)));
                }
            }
            let mut slugs = vec![];
            for &i in targets.iter().rev() {
                let c = &f.changes[i];
                let slug = c.held.write.slug.clone();
                let cur = read_if_exists(&page_file(wiki_dir, &slug)?)?.unwrap_or_default();
                let earlier = match &c.replaced {
                    Some(rel) => read_history(wiki_dir, &slug, rel)?,
                    None => None,
                };
                restore_page(wiki_dir, &slug, &cur, c.applied_hash.as_deref().unwrap_or_default(), earlier.as_deref(), "undone")?;
                log_line(wiki_dir, "window", &format!("Undid {}: {} [[{slug}]]", if cleanup { "a cleanup" } else { "a change" }, one_line(&c.held.write.title)))?;
                slugs.push(slug);
            }
            wiki::refresh_index(wiki_dir, None)?;
            for &i in &targets {
                f.changes[i].status = "undone".into();
                f.changes[i].undone_at = Some(local_iso(&now()));
            }
            write_json(wiki_dir, &path, &f)?;
            Ok(json!({ "status": "undone", "slugs": slugs }))
        })();
        Ok(())
    })?;
    out
}

fn decide_locked(wiki_dir: &Path, batch: &str, index: usize, action: &str, auto: bool) -> Result<Value, Error> {
    let Some(mut f) = load(wiki_dir, batch) else { return Err(Error::NotFound(format!("no held changes for batch {batch}"))) };
    let Some(e) = f.changes.get(index).cloned() else { return Err(Error::NotFound("no such change".into())) };
    if e.status != "pending" {
        return Err(Error::Conflict(format!("This change was already {}.", e.status)));
    }
    let w = &e.held.write;
    let title = one_line(&w.title);
    let (mut applied_hash, mut replaced, mut replaced_hash) = (None, None, None);
    let outcome = match action {
        "approve" => {
            let file = page_file(wiki_dir, &w.slug)?;
            let cur = check_fits(wiki_dir, &e)?;
            match page_write(&e.held.op, cur.as_deref(), batch, &now()) {
                None => "approved (the page already said this)".to_string(),
                Some(nw) => {
                    if let Some(c) = &cur {
                        replaced = Some(keep_history(wiki_dir, &w.slug, c, batch.get(batch.len().saturating_sub(6)..).unwrap_or("held"))?);
                        replaced_hash = Some(hash_text(c, 16));
                    }
                    atomic_write(&file, &nw.text, None, Some(cur.as_deref().map(|c| hash_text(c, 16)).as_deref()))?;
                    wiki::refresh_index(wiki_dir, None)?;
                    let verb = match (e.held.kind == "lint", auto) {
                        (true, true) => "Cleanup applied automatically",
                        (true, false) => "Cleanup approved",
                        (false, true) => "Change applied automatically",
                        (false, false) => "Change approved",
                    };
                    log_line(wiki_dir, "curator", &format!("{verb}: {title} [[{}]]", w.slug))?;
                    applied_hash = Some(nw.new_hash.clone());
                    format!("approved; the page is now {}", nw.new_hash)
                }
            }
        }
        "reject" => {
            log_line(wiki_dir, "curator", &format!("Change rejected: {title} [[{}]]", w.slug))?;
            "rejected".to_string()
        }
        _ => {
            let p = curator_paths(wiki_dir);
            let mut back = 0;
            for id in &w.note_ids {
                let from = wiki::confined_path(wiki_dir, &p.done.join(month_dir(id)).join(format!("{id}.md")))?;
                let to = wiki::confined_path(wiki_dir, &p.inbox.join(format!("{id}.md")))?;
                if from.exists() && !to.exists() {
                    fs::create_dir_all(&p.inbox)?;
                    fs::rename(&from, &to)?;
                    back += 1;
                }
            }
            log_line(wiki_dir, "curator", &format!("Change sent back to the curator: {title} [[{}]]", w.slug))?;
            format!("{back} note(s) back in the inbox")
        }
    };
    let status = match action {
        "approve" => "approved",
        "reject" => "rejected",
        _ => "refiled",
    };
    f.changes[index].status = status.into();
    f.changes[index].decided_at = Some(local_iso(&now()));
    f.changes[index].outcome = Some(outcome.clone());
    f.changes[index].auto = auto;
    f.changes[index].applied_hash = applied_hash;
    f.changes[index].replaced = replaced;
    f.changes[index].replaced_hash = replaced_hash;
    let changed = f.changes[index].applied_hash.is_some();
    let file = held_file(wiki_dir, batch);
    write_json(wiki_dir, &file, &f)?;
    if f.changes.iter().all(|c| c.status != "pending") {
        let done = wiki::confined_path(wiki_dir, &held_dir(wiki_dir).join("done"))?;
        fs::create_dir_all(&done)?;
        fs::rename(&file, done.join(format!("{batch}.json")))?;
    }
    Ok(json!({ "status": status, "outcome": outcome, "slug": w.slug, "changed": changed }))
}

// ---------------------------------------------------------------- revert

/// The audit record's view of one page change of a batch.
struct Change {
    log_title: String,
    action: String,
    base_hash: Option<String>,
    new_hash: String,
    history: Option<String>,
}

fn find_change(wiki_dir: &Path, batch: &str, slug: &str) -> Option<Change> {
    let audit = curator_paths(wiki_dir).audit;
    // A note's audit record is filed under the note's month, which is the batch's month or earlier.
    let month = month_dir(batch);
    let mut dirs = vec![audit.join(&month)];
    if let (Some(y), Some(m)) = (batch.get(..4).and_then(|y| y.parse::<i32>().ok()), batch.get(5..7).and_then(|m| m.parse::<u32>().ok())) {
        let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
        dirs.push(audit.join(format!("{py:04}")).join(format!("{pm:02}")));
    }
    for d in dirs {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(path) = wiki::confined_path(wiki_dir, &e.path()) else { continue };
            let Some(a) = read_if_exists(&path).ok().flatten().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { continue };
            if a["batch"].as_str() != Some(batch) {
                continue;
            }
            for c in a["changes"].as_array().into_iter().flatten() {
                if c["slug"].as_str() == Some(slug) && c["held"].as_bool() != Some(true) {
                    let s = |k: &str| c[k].as_str().map(String::from);
                    if c["history"].as_str().is_some_and(|rel| !valid_history_rel(slug, rel)) {
                        continue;
                    }
                    let log_title = a["log"][0].as_str().unwrap_or(slug).to_string();
                    return Some(Change { log_title, action: s("action").unwrap_or_default(), base_hash: s("baseHash"), new_hash: s("newHash")?, history: s("history") });
                }
            }
        }
    }
    None
}

/// Undoes one page change of a curator batch, if the page is still exactly what that batch wrote.
/// Otherwise Conflict: the window then offers undo_request.
pub fn revert(wiki_dir: &Path, batch: &str, slug: &str) -> Result<Value, Error> {
    check_batch(batch)?;
    if !SLUG_RE.is_match(slug) {
        return Err(Error::NotFound("no such page".into()));
    }
    let Some(Change { action, base_hash, new_hash, history, .. }) = find_change(wiki_dir, batch, slug) else {
        return Err(Error::NotFound(format!("batch {batch} has no recorded change to {slug}")));
    };
    let mut out: Result<Value, Error> = Err(Error::NotFound("no such change".into()));
    wiki::with_lock(wiki_dir, || {
        out = (|| {
            let file = page_file(wiki_dir, slug)?;
            let Some(cur) = read_if_exists(&file)? else { return Err(Error::Conflict("The page no longer exists.".into())) };
            if hash_text(&cur, 16) != new_hash {
                return Err(Error::Conflict("The page changed after that change, so it cannot simply be restored. Ask the curator to undo it on the current page instead.".into()));
            }
            let title = one_line(&frontmatter::parse(&cur).0.str("title"));
            let earlier = if action == "created" {
                None
            } else {
                let base = history.as_deref().map(|h| read_history(wiki_dir, slug, h)).transpose()?.flatten();
                let Some(base) = base.filter(|b| Some(hash_text(b, 16)) == base_hash) else {
                    return Err(Error::Conflict("The earlier version is missing from .history/, so the change cannot be restored.".into()));
                };
                Some(base)
            };
            restore_page(wiki_dir, slug, &cur, &new_hash, earlier.as_deref(), "reverted")?;
            wiki::refresh_index(wiki_dir, None)?;
            log_line(wiki_dir, "window", &format!("Reverted the curator's change: {} [[{slug}]]", if title.is_empty() { slug } else { &title }))?;
            Ok(json!({ "reverted": slug, "action": action }))
        })();
        Ok(())
    })?;
    out
}

/// When a change cannot be restored as a whole: a note from the person asking the curator to undo it
/// on the current page, with what the change did.
pub fn undo_request(wiki_dir: &Path, batch: &str, slug: &str) -> Result<Value, Error> {
    check_batch(batch)?;
    if !SLUG_RE.is_match(slug) {
        return Err(Error::NotFound("no such page".into()));
    }
    let Some(Change { log_title, action, history, .. }) = find_change(wiki_dir, batch, slug) else {
        return Err(Error::NotFound(format!("batch {batch} has no recorded change to {slug}")));
    };
    let before = history.as_deref().map(|rel| read_history(wiki_dir, slug, rel)).transpose()?.flatten().map(|t| body_of(&t)).unwrap_or_default();
    // What the batch wrote: the next .history copy after it, or the page now if nothing came after.
    let mut body = format!("Undo the curator's change to [[{slug}]] from batch {batch} ({log_title}).");
    if action == "created" {
        body.push_str(" That batch created the page.");
    } else if !before.is_empty() {
        body.push_str(&format!("\n\nThe page before that change:\n\n```markdown\n{}\n```", take_chars(&before, 6000)));
    }
    let note =
        inbox::NoteInput { kind: "log".into(), source: "user".into(), app: "window".into(), title: format!("Undo the curator's change to {slug}"), body, pages: json!([slug]), ..Default::default() };
    let r = inbox::submit_note(wiki_dir, &note, "ui", "agent-wiki window")?;
    Ok(json!({ "note": r.id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curator::PageWrite;

    fn wiki(name: &str) -> PathBuf {
        let w = std::env::temp_dir().join(format!("aw-held-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&w);
        fs::create_dir_all(w.join("pages")).unwrap();
        fs::create_dir_all(w.join("log")).unwrap();
        w
    }

    fn page(title: &str, body: &str) -> String {
        format!("---\ntitle: {title}\ntype: topic\nsummary: \"\"\ntags: []\n---\n\n{body}\n")
    }

    /// A held patch of page `p` from batch `batch`, recorded the way the curator records it.
    fn hold(w: &Path, batch: &str, base: &str, find: &str, replace: &str) {
        let op = json!({ "slug": "p", "action": "patch", "base_hash": hash_text(base, 16), "title": null, "type": null, "summary": null, "tags": null, "content": null, "edits": [{ "find": find, "replace": replace }], "note_ids": ["2026-10-03_10-00-00-000-aaaaaa"], "reason": "r" });
        let write: PageWrite = page_write(&op, Some(base), batch, &now()).unwrap();
        let notes = vec![json!({ "id": "2026-10-03_10-00-00-000-aaaaaa", "app": "codex", "title": "t", "source": "external" })];
        record(w, batch, "2026-10-03T10:00:00-05:00", &[HeldOp { write, reasons: vec!["it comes from external content".into()], op, notes, kind: String::new() }]).unwrap();
    }

    fn today_log(w: &Path) -> String {
        fs::read_to_string(w.join("log").join(&local_date(&now())[..4]).join(format!("{}.md", local_date(&now())))).unwrap_or_default()
    }

    fn apply(w: &Path, batch: &str) -> Vec<String> {
        wiki::with_lock(w, || apply_auto_locked(w, batch)).unwrap()
    }

    #[test]
    fn automatic_approval_is_logged_and_can_be_undone() {
        let w = wiki("auto");
        let base = page("P", "# P\n\nPort 8443.");
        fs::write(w.join("pages/p.md"), &base).unwrap();
        let b = "2026-10-06_10-00-01-000-aaaaaa";
        hold(&w, b, &base, "Port 8443.", "Port 9443.");
        assert_eq!(apply(&w, b), ["p"]);
        assert_eq!(pending_count(&w), 0);
        assert!(fs::read_to_string(w.join("pages/p.md")).unwrap().contains("Port 9443."));
        let s = auto_summary(&w);
        assert_eq!((s["count"].as_u64(), s["last"]["slug"].as_str(), s["times"].as_array().map(Vec::len)), (Some(1), Some("p"), Some(1)));
        let r = recent_auto(&w, 7);
        assert_eq!((r[0]["status"].as_str(), r[0]["undoable"].as_bool()), (Some("approved"), Some(true)));
        assert!(r[0]["diff"].as_array().unwrap().iter().any(|d| d[0] == "+" && d[1] == "Port 9443."), "the Inbox can show what changed");
        assert_eq!(undo(&w, b, 0).unwrap()["status"], "undone");
        assert_eq!(fs::read_to_string(w.join("pages/p.md")).unwrap(), base, "the page as it was");
        let r = recent_auto(&w, 7);
        assert_eq!((r[0]["status"].as_str(), r[0]["undoable"].as_bool()), (Some("undone"), Some(false)));
        assert!(matches!(undo(&w, b, 0), Err(Error::Conflict(_))), "once");
        let log = today_log(&w);
        assert!(log.contains("Change applied automatically: P [[p]]") && log.contains("Undid a change: P [[p]]"), "{log}");
        let _ = fs::remove_dir_all(&w);
    }

    #[test]
    fn a_change_that_no_longer_fits_waits_and_says_so_once() {
        let w = wiki("misfit");
        let base = page("P", "# P\n\nPort 8443.");
        fs::write(w.join("pages/p.md"), page("P", "# P\n\nRewritten by hand.")).unwrap();
        let b = "2026-10-06_10-00-02-000-bbbbbb";
        hold(&w, b, &base, "Port 8443.", "Port 9443.");
        assert!(apply(&w, b).is_empty());
        assert!(apply(&w, b).is_empty(), "tried again: still waiting");
        assert_eq!(pending_count(&w), 1);
        assert!(list(&w)[0]["autoFailed"].as_str().is_some_and(|m| m.contains("page changed")));
        assert_eq!(today_log(&w).matches("Change waiting for your OK: P [[p]] (not applied automatically").count(), 1, "logged once");
        let _ = fs::remove_dir_all(&w);
    }

    #[test]
    fn a_cleanup_goes_in_and_comes_out_whole() {
        let w = wiki("split");
        let base = page("P", "# P\n\n## Long part\n\nDetails.");
        fs::write(w.join("pages/p.md"), &base).unwrap();
        let b = "2026-10-06_10-00-03-000-cccccc";
        let patch = json!({ "slug": "p", "action": "patch", "edits": [{ "find": "## Long part\n\nDetails.", "replace": "See [[q]]." }], "note_ids": [], "reason": "split" });
        let create = json!({ "slug": "q", "action": "create", "title": "Q", "type": "topic", "summary": "q", "tags": [], "content": "# Q\n\nDetails.", "note_ids": [], "reason": "split" });
        let held: Vec<HeldOp> = [(&patch, Some(base.as_str())), (&create, None)]
            .into_iter()
            .map(|(op, cur)| HeldOp { write: page_write(op, cur, b, &now()).unwrap(), reasons: vec!["a cleanup proposes it: split".into()], op: op.clone(), notes: vec![], kind: "lint".into() })
            .collect();
        record(&w, b, "2026-10-06T10:00:03-05:00", &held).unwrap();
        fs::write(w.join("pages/q.md"), page("Q", "# Q\n\nSomeone else's page.")).unwrap();
        assert!(apply(&w, b).is_empty(), "the new page's name is taken: nothing goes in, not even the patch");
        assert_eq!(fs::read_to_string(w.join("pages/p.md")).unwrap(), base);
        assert_eq!(pending_count(&w), 2);
        fs::remove_file(w.join("pages/q.md")).unwrap();
        assert_eq!(apply(&w, b), ["p", "q"]);
        assert!(fs::read_to_string(w.join("pages/p.md")).unwrap().contains("See [[q]]."));
        assert_eq!(undo(&w, b, 1).unwrap()["slugs"], json!(["q", "p"]), "undoing one page of a cleanup undoes all of it");
        assert_eq!(fs::read_to_string(w.join("pages/p.md")).unwrap(), base);
        assert!(!w.join("pages/q.md").exists());
        let _ = fs::remove_dir_all(&w);
    }

    #[test]
    fn a_planted_held_file_is_ignored() {
        let w = wiki("planted");
        let b = "2026-10-06_10-00-04-000-dddddd";
        let base = page("P", "# P\n\nPort 8443.");
        fs::write(w.join("pages/p.md"), &base).unwrap();
        hold(&w, b, &base, "Port 8443.", "Port 9443.");
        let file = w.join(".curator/held").join(format!("{b}.json"));
        for (from, to) in [("\"slug\": \"p\"", "\"slug\": \"../../outside\""), (b, "2026-10-06_10-00-04-000-eeeeee")] {
            let text = fs::read_to_string(&file).unwrap();
            fs::write(&file, text.replacen(from, to, 1)).unwrap();
            assert_eq!(pending_count(&w), 0, "{to}");
            assert_eq!(auto_apply_all(&w).unwrap(), 0);
            fs::write(&file, text).unwrap();
        }
        assert!(!w.join("outside.md").exists() && !w.parent().unwrap().join("outside.md").exists());
        let _ = fs::remove_dir_all(&w);
    }

    #[test]
    fn edited_history_references_cannot_read_other_files() {
        let w = wiki("history-paths");
        let batch = "2026-10-06_10-00-04-000-dddddd";
        let base = page("P", "# P\n\nPort 8443.");
        fs::write(w.join("pages/p.md"), &base).unwrap();
        hold(&w, batch, &base, "Port 8443.", "Port 9443.");
        let mut f = load(&w, batch).unwrap();
        f.changes[0].status = "approved".into();
        f.changes[0].applied_hash = Some(hash_text(&base, 16));
        let outside = w.with_extension("outside.md");
        let private = "Synthetic external fixture, never a wiki page.";
        fs::write(&outside, private).unwrap();
        let audit = w.join(".curator/audit/2026/10");
        fs::create_dir_all(&audit).unwrap();
        let relative_escape = format!("../{}", outside.file_name().unwrap().to_string_lossy());
        for rel in [relative_escape, outside.to_string_lossy().into_owned(), ".history/pages/q/old.md".into(), ".history/pages/p/../old.md".into(), ".history/pages/p/..\\old.md".into()] {
            f.changes[0].replaced = Some(rel.clone());
            f.changes[0].replaced_hash = Some(hash_text(private, 16));
            write_json(&w, &held_file(&w, batch), &f).unwrap();
            assert!(undo(&w, batch, 0).is_err(), "undo accepted {rel}");
            let rec = json!({ "batch": batch, "changes": [{ "slug": "p", "action": "patched", "baseHash": hash_text(private, 16), "newHash": hash_text(&base, 16), "history": rel }] });
            fs::write(audit.join("fixture.json"), rec.to_string()).unwrap();
            assert!(revert(&w, batch, "p").is_err(), "revert accepted {rel}");
            assert!(undo_request(&w, batch, "p").is_err(), "undo request accepted {rel}");
            assert_eq!(fs::read_to_string(w.join("pages/p.md")).unwrap(), base);
            assert!(inbox::list_notes(&w).unwrap().is_empty());
        }
        // Even a correctly scoped history reference must carry the hash checked before restoring.
        f.changes[0].replaced = Some(".history/pages/p/old.md".into());
        f.changes[0].replaced_hash = None;
        write_json(&w, &held_file(&w, batch), &f).unwrap();
        assert!(undo(&w, batch, 0).is_err());
        fs::remove_file(outside).unwrap();
        let _ = fs::remove_dir_all(&w);
    }

    #[cfg(unix)]
    #[test]
    fn history_links_cannot_supply_undo_or_refile_content() {
        let w = wiki("history-links");
        let batch = "2026-10-06_10-00-04-000-dddddd";
        let base = page("P", "# P\n\nPort 8443.");
        fs::write(w.join("pages/p.md"), &base).unwrap();
        hold(&w, batch, &base, "Port 8443.", "Port 9443.");
        let outside = w.with_extension("outside.md");
        let private = "Synthetic external fixture.";
        fs::write(&outside, private).unwrap();
        fs::create_dir_all(w.join(".history/pages/p")).unwrap();
        let rel = ".history/pages/p/link.md";
        std::os::unix::fs::symlink(&outside, w.join(rel)).unwrap();
        let mut f = load(&w, batch).unwrap();
        let e = &mut f.changes[0];
        e.status = "approved".into();
        e.applied_hash = Some(hash_text(&base, 16));
        e.replaced = Some(rel.into());
        e.replaced_hash = Some(hash_text(private, 16));
        assert!(undo_problem(&w, e).is_some());
        write_json(&w, &held_file(&w, batch), &f).unwrap();
        assert!(undo(&w, batch, 0).is_err());
        let audit = w.join(".curator/audit/2026/10");
        fs::create_dir_all(&audit).unwrap();
        fs::write(
            audit.join("fixture.json"),
            json!({ "batch": batch, "changes": [{ "slug": "p", "action": "patched", "baseHash": hash_text(private, 16), "newHash": hash_text(&base, 16), "history": rel }] }).to_string(),
        )
        .unwrap();
        assert!(revert(&w, batch, "p").is_err());
        assert!(undo_request(&w, batch, "p").is_err());
        assert_eq!(fs::read_to_string(w.join("pages/p.md")).unwrap(), base);
        assert!(inbox::list_notes(&w).unwrap().is_empty());
        fs::remove_file(outside).unwrap();
        let _ = fs::remove_dir_all(&w);
    }

    #[cfg(unix)]
    #[test]
    fn approval_refuses_linked_page_and_history_destinations() {
        let w = wiki("write-links");
        let batch = "2026-10-06_10-00-04-000-dddddd";
        let base = page("P", "# P\n\nPort 8443.");
        fs::write(w.join("pages/p.md"), &base).unwrap();
        hold(&w, batch, &base, "Port 8443.", "Port 9443.");
        let outside = w.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("p.md"), &base).unwrap();
        fs::rename(w.join("pages"), w.join("original-pages")).unwrap();
        std::os::unix::fs::symlink(&outside, w.join("pages")).unwrap();
        assert!(decide(&w, batch, 0, "approve").is_err());
        assert_eq!(fs::read_to_string(outside.join("p.md")).unwrap(), base);
        fs::remove_file(w.join("pages")).unwrap();
        fs::rename(w.join("original-pages"), w.join("pages")).unwrap();
        std::os::unix::fs::symlink(&outside, w.join(".history")).unwrap();
        assert!(decide(&w, batch, 0, "approve").is_err());
        assert_eq!(fs::read_to_string(w.join("pages/p.md")).unwrap(), base);
        assert!(!outside.join("pages").exists(), "no history directories created outside the wiki");
        let _ = fs::remove_dir_all(&w);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn approve_reject_and_send_back() {
        let w = wiki("decide");
        let base = page("P", "# P\n\nPort 8443.");
        fs::write(w.join("pages/p.md"), &base).unwrap();
        let b1 = "2026-10-03_10-00-01-000-111111";
        hold(&w, b1, &base, "Port 8443.", "Port 9443.");
        assert_eq!(pending_count(&w), 1);
        let l = list(&w);
        assert_eq!(l[0]["slug"], "p");
        assert_eq!(l[0]["stale"], false);
        assert!(l[0]["diff"].as_array().unwrap().iter().any(|d| d[0] == "+" && d[1] == "Port 9443."));
        let r = decide(&w, b1, 0, "approve").unwrap();
        assert_eq!(r["status"], "approved");
        assert!(fs::read_to_string(w.join("pages/p.md")).unwrap().contains("Port 9443."));
        assert_eq!(pending_count(&w), 0);
        assert!(w.join(".curator/held/done").join(format!("{b1}.json")).exists(), "a decided file moves out of the way");
        assert!(fs::read_dir(w.join(".history/pages/p")).unwrap().count() == 1, "the previous version is kept");
        assert!(matches!(decide(&w, b1, 0, "approve"), Err(Error::NotFound(_))));

        // A patch whose base moved on still applies when its edit does; a replace would not.
        let now_text = fs::read_to_string(w.join("pages/p.md")).unwrap();
        let b2 = "2026-10-03_10-00-02-000-222222";
        hold(&w, b2, &now_text, "Port 9443.", "Port 9443 (load balancer).");
        fs::write(w.join("pages/p.md"), now_text.replace("# P\n", "# P\n\nIntro added by hand.\n")).unwrap();
        assert_eq!(list(&w)[0]["stale"], true);
        decide(&w, b2, 0, "approve").unwrap();
        let t = fs::read_to_string(w.join("pages/p.md")).unwrap();
        assert!(t.contains("Intro added by hand.") && t.contains("Port 9443 (load balancer)."), "{t}");

        let b3 = "2026-10-03_10-00-03-000-333333";
        hold(&w, b3, &t, "Port 9443 (load balancer).", "Port 1.");
        assert_eq!(decide(&w, b3, 0, "reject").unwrap()["status"], "rejected");
        assert!(!fs::read_to_string(w.join("pages/p.md")).unwrap().contains("Port 1."));

        let b4 = "2026-10-03_10-00-04-000-444444";
        hold(&w, b4, &t, "nothing like this", "x");
        let done = w.join(".curator/done/2026/10");
        fs::create_dir_all(&done).unwrap();
        fs::write(done.join("2026-10-03_10-00-00-000-aaaaaa.md"), "---\nid: x\n---\n\nnote\n").unwrap();
        assert!(matches!(decide(&w, b4, 0, "approve"), Err(Error::Conflict(_))), "an edit that no longer applies");
        assert_eq!(decide(&w, b4, 0, "refile").unwrap()["status"], "refiled");
        assert!(w.join("inbox/2026-10-03_10-00-00-000-aaaaaa.md").exists(), "the note is back in the inbox");
        assert!(matches!(decide(&w, "../../etc", 0, "approve"), Err(Error::NotFound(m)) if m.starts_with("no batch")));
        let log = fs::read_to_string(w.join("log").join(&local_date(&now())[..4]).join(format!("{}.md", local_date(&now())))).unwrap();
        assert!(log.contains("Change approved: P [[p]]") && log.contains("Change rejected: P [[p]]") && log.contains("sent back to the curator"), "{log}");
        let _ = fs::remove_dir_all(&w);
    }

    #[test]
    fn revert_restores_only_an_untouched_page() {
        let w = wiki("revert");
        let (before, after) = (page("P", "# P\n\nv1"), page("P", "# P\n\nv2"));
        fs::write(w.join("pages/p.md"), &after).unwrap();
        fs::create_dir_all(w.join(".history/pages/p")).unwrap();
        fs::write(w.join(".history/pages/p/h1.md"), &before).unwrap();
        let batch = "2026-10-03_11-00-00-000-bbbbbb";
        let audit = w.join(".curator/audit/2026/09");
        fs::create_dir_all(&audit).unwrap();
        let rec = json!({ "batch": batch, "log": ["Changed P"], "changes": [{ "slug": "p", "action": "patched", "baseHash": hash_text(&before, 16), "newHash": hash_text(&after, 16), "history": ".history/pages/p/h1.md" }] });
        fs::write(audit.join("2026-09-30_09-00-00-000-cccccc.json"), rec.to_string()).unwrap();

        fs::write(w.join("pages/p.md"), after.replace("v2", "v2 and a human edit")).unwrap();
        assert!(matches!(revert(&w, batch, "p"), Err(Error::Conflict(_))), "edited since: refuse");
        let asked = undo_request(&w, batch, "p").unwrap();
        let note = fs::read_to_string(w.join("inbox").join(format!("{}.md", asked["note"].as_str().unwrap()))).unwrap();
        assert!(note.contains("source: user") && note.contains("Undo the curator's change to [[p]]") && note.contains("v1"), "{note}");

        fs::write(w.join("pages/p.md"), &after).unwrap();
        revert(&w, batch, "p").unwrap();
        assert_eq!(fs::read_to_string(w.join("pages/p.md")).unwrap(), before);
        assert!(matches!(revert(&w, batch, "p"), Err(Error::Conflict(_))), "already reverted");
        assert!(matches!(revert(&w, batch, "other"), Err(Error::NotFound(_))));
        let _ = fs::remove_dir_all(&w);
    }
}
