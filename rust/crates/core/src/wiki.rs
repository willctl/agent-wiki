//! The wiki: config resolution, the skeleton, atomic writes, pages and the generated index, the
//! daily log, full-text search, reads and wiki_start (src/wiki.mjs). Formats are byte-for-byte the
//! Node runtime's, so both can work on one wiki.

use crate::frontmatter::{self, Meta, Val};
use crate::inbox;
use crate::lock::{self, AcquireError, LockOpts};
use crate::paths::{AppPaths, Env};
use crate::secrets::find_secret;
use crate::text::*;
use chrono::{Duration as CDuration, Local};
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

pub const DEFAULT_PROTOCOL: &str = include_str!("../../../../protocol/PROTOCOL.md");
pub const PAGE_TYPES: [&str; 7] = ["project", "person", "preference", "decision", "howto", "reference", "topic"];
pub const DEFAULT_HTTP_PORT: u16 = 47821;

pub static SLUG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,79}$").unwrap());
pub static DATE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$").unwrap());

fn type_heading(t: &str) -> String {
    match t {
        "project" => "Projects".into(),
        "person" => "People".into(),
        "preference" => "Preferences".into(),
        "decision" => "Decisions".into(),
        "howto" => "How-tos".into(),
        "reference" => "Reference".into(),
        "topic" => "Topics".into(),
        other => {
            let mut c = other.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

// ---------------------------------------------------------------- errors

#[derive(Debug)]
pub enum Error {
    /// A message meant for the model (an isError tool result).
    Wiki(String),
    /// atomic_write found the file changed since it was read.
    Stale(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Wiki(m) | Error::Stale(m) => f.write_str(m),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn wiki_err<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Wiki(m.into()))
}

// ---------------------------------------------------------------- config

/// The config.json in use: the standard one, or, until install-local has moved it, the one in ~/.agent-wiki.
pub fn config_path(env: &Env) -> PathBuf {
    let p = AppPaths::from_env(env);
    if !p.portable && !env.contains_key("AGENT_WIKI_CONFIG_DIR") && !p.config_file().exists() {
        let legacy = p.legacy_home.join("config.json");
        if legacy.exists() {
            return legacy;
        }
    }
    p.config_file()
}

/// config.json as a JSON object (empty when missing).
pub fn read_config(env: &Env) -> Result<Value> {
    let file = config_path(env);
    let raw = match fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Object(Default::default())),
        Err(e) => return Err(e.into()),
    };
    match serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}')) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Ok(Value::Object(Default::default())),
        Err(e) => wiki_err(format!("Agent Wiki config {} is not valid JSON: {e}", display_path(&file.to_string_lossy()))),
    }
}

/// env AGENT_WIKI_DIR > wikiDir in config.json > ~/AgentWiki. Returns (wiki dir, where it came from).
pub fn resolve_wiki_dir(env: &Env) -> Result<(PathBuf, String)> {
    if let Some(d) = env.get("AGENT_WIKI_DIR").filter(|d| !d.is_empty()) {
        return Ok((absolute(Path::new(d)), "AGENT_WIKI_DIR".into()));
    }
    let cfg = read_config(env)?;
    if let Some(d) = cfg.get("wikiDir").and_then(Value::as_str).filter(|d| !d.trim().is_empty()) {
        return Ok((absolute(Path::new(d)), config_path(env).to_string_lossy().into_owned()));
    }
    Ok((crate::paths::home_dir(env).join("AgentWiki"), "default".into()))
}

/// path.resolve: absolute, with `.` and `..` resolved lexically.
pub fn absolute(p: &Path) -> PathBuf {
    let base = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) };
    normalize(&base)
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn disp(p: &Path) -> String {
    display_path(&p.to_string_lossy())
}

// ---------------------------------------------------------------- names, tags, pages

pub fn normalize_app(app: &str) -> String {
    static NON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9._-]+").unwrap());
    static DASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-{2,}").unwrap());
    static EDGES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-.]+|[-.]+$").unwrap());
    let s = app.to_lowercase();
    let s = NON.replace_all(s.trim(), "-");
    let s = DASHES.replace_all(&s, "-");
    let s = EDGES.replace_all(&s, "");
    let s = take_chars(&s, 40);
    if s.is_empty() { "unknown".into() } else { s }
}

pub fn slugify(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    static NON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").unwrap());
    let s: String = title.nfkd().filter(|c| !('\u{0300}'..='\u{036f}').contains(c)).collect::<String>().to_lowercase();
    let s = NON.replace_all(&s, "-");
    let s = s.trim_start_matches('-');
    let s = take_chars(s, 80);
    s.trim_end_matches('-').to_string()
}

/// Tags from a JSON value: an array, or a comma-separated string. Lowercase, no leading #, unique.
pub fn norm_tags(tags: &Value) -> Vec<String> {
    let list: Vec<String> = match tags {
        Value::Null => return vec![],
        Value::String(s) if s.is_empty() => return vec![],
        Value::Array(a) => a.iter().map(frontmatter::js_string).collect(),
        other => frontmatter::js_string(other).split(',').map(String::from).collect(),
    };
    let mut out: Vec<String> = Vec::new();
    for t in list {
        let v = one_line(&t);
        let v = v.trim_start_matches('#').trim().to_lowercase();
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

pub fn norm_tags_val(v: Option<&Val>) -> Vec<String> {
    match v {
        None => vec![],
        Some(Val::Str(s)) => norm_tags(&Value::String(s.clone())),
        Some(Val::List(l)) => norm_tags(&Value::Array(l.iter().cloned().map(Value::String).collect())),
    }
}

pub fn norm_page_refs(pages: &Value) -> Vec<String> {
    norm_tags(pages)
        .into_iter()
        .map(|p| {
            let p = p.strip_prefix("[[").unwrap_or(&p).to_string();
            let p = p.strip_suffix("]]").unwrap_or(&p).to_string();
            let p = p.split('|').next().unwrap_or("").to_string();
            if SLUG_RE.is_match(&p) { p } else { slugify(&p) }
        })
        .filter(|p| !p.is_empty())
        .collect()
}

pub fn assert_no_secrets(fields: &[(&str, &str)]) -> Result<()> {
    for (name, value) in fields {
        if let Some(kind) = find_secret(value) {
            return wiki_err(format!(
                "Refused: `{name}` looks like it contains {kind}. Never store secrets in the wiki. Record WHERE the secret lives instead (for example \"API key is in the 1Password vault 'Work'\"), then try again."
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- fs helpers

/// Checks a wiki path, including the existing parents of a file about to be created.
/// The configured root may be a link; links and Windows reparse points below it are refused.
/// This does not defend against another process changing the filesystem during an operation.
pub fn confined_path(wiki_dir: &Path, path: &Path) -> Result<PathBuf> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return wiki_err("Refused a parent traversal in a wiki path.");
    }
    let root = absolute(wiki_dir);
    let real_root = fs::canonicalize(&root)?;
    let abs = absolute(path);
    let rel = abs.strip_prefix(&root).or_else(|_| abs.strip_prefix(&real_root)).map_err(|_| Error::Wiki("Refused a path outside the wiki folder.".into()))?;
    let mut checked = real_root;
    for part in rel.components() {
        if !matches!(part, Component::Normal(_)) {
            return wiki_err("Refused an invalid wiki path component.");
        }
        checked.push(part);
        match fs::symlink_metadata(&checked) {
            Ok(m) => {
                #[cfg(windows)]
                let linked = {
                    use std::os::windows::fs::MetadataExt;
                    m.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let linked = m.file_type().is_symlink();
                if linked {
                    return wiki_err("Refused a link or reparse point inside the wiki folder.");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(checked)
}

pub fn read_if_exists(file: &Path) -> Result<Option<String>> {
    match fs::read(file) {
        Ok(b) => Ok(Some(to_lf(&String::from_utf8_lossy(&b)))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Makes a rename or a new file in `dir` durable: on POSIX that takes an fsync of the folder.
pub fn fsync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Writes and fsyncs a file in place. `create_new`: fail if it exists (the idempotency keys).
pub fn write_synced(file: &Path, content: &str, create_new: bool) -> std::io::Result<()> {
    let mut f = if create_new { fs::OpenOptions::new().write(true).create_new(true).open(file)? } else { fs::File::create(file)? };
    f.write_all(content.as_bytes())?;
    f.sync_all()
}

fn retryable(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33))
}

/// Temp file + fsync + rename, LF only: readers see the old or the new file, never a partial one.
/// `expect_hash`: write only if the file still has that content hash (None = it must not exist).
pub fn atomic_write(file: &Path, content: &str, tmp_dir: Option<&Path>, expect_hash: Option<Option<&str>>) -> Result<()> {
    let parent = file.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let base = format!("{}.{}.{}.tmp", file.file_name().unwrap_or_default().to_string_lossy(), std::process::id(), random_hex(6));
    let tmp = match tmp_dir {
        Some(d) => {
            fs::create_dir_all(d)?;
            d.join(base)
        }
        None => parent.join(base),
    };
    write_synced(&tmp, &to_lf(content), false)?;
    let mut i = 0u64;
    loop {
        if let Some(expect) = expect_hash {
            let cur = read_if_exists(file)?;
            if cur.as_deref().map(|c| hash_text(c, 16)).as_deref() != expect {
                let _ = fs::remove_file(&tmp);
                return Err(Error::Stale(format!("{} changed just before it was written", disp(file))));
            }
        }
        match fs::rename(&tmp, file) {
            Ok(()) => {
                fsync_dir(parent);
                crate::readcache::forget(file);
                return Ok(());
            }
            Err(e) if i < 20 && retryable(&e) => {
                std::thread::sleep(std::time::Duration::from_millis(20 + i * 20));
                i += 1;
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
    }
}

pub fn write_if_missing(file: &Path, content: &str) -> Result<bool> {
    match fs::OpenOptions::new().write(true).create_new(true).open(file) {
        Ok(mut f) => {
            f.write_all(to_lf(content).as_bytes())?;
            drop(f);
            fsync_dir(file.parent().unwrap_or(Path::new(".")));
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Runs `f` holding the wiki's write lock; a busy lock is a message for the model.
pub fn with_lock<T>(wiki_dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let mut guard = match lock::acquire(wiki_dir, "write", &LockOpts::default()) {
        Ok(g) => g,
        Err(AcquireError::Busy(b)) => return wiki_err(format!("The wiki is busy ({}). Try again shortly.", b.0)),
        Err(AcquireError::Io(e)) => return Err(e.into()),
    };
    let r = f();
    guard.release();
    r
}

// ---------------------------------------------------------------- skeleton

const README: &str = "# Agent Wiki

This folder is a shared, plain-Markdown memory used by AI apps on this PC
(Claude desktop, Claude Code, ChatGPT desktop, Codex) through the local
`agent-wiki` MCP server. You own it: open it in any editor or in Obsidian.

- `PROTOCOL.md`: the rules every AI session follows. Edit it freely; servers
  read it at startup and `wiki_start` returns the current text.
- `index.md`: generated from page frontmatter. Do not edit it by hand.
- `pages/<slug>.md`: one page per project, person, preference, decision,
  how-to, reference or topic.
- `log/YYYY/YYYY-MM-DD.md`: the daily, append-only activity log.
- `.history/`: earlier versions of pages that were rewritten.
- `.locks/`: transient write locks. Safe to delete when no app is running.
- `inbox/`: notes the apps sent that the curator has not organized yet.
- `.curator/`: the curator's queue state, journal, archive and audit trail.
";

/// Creates any missing part of the skeleton; never overwrites user content.
pub fn ensure_wiki(wiki_dir: &Path) -> Result<()> {
    fs::create_dir_all(wiki_dir)?;
    for d in ["pages", "log", ".history", ".locks", "inbox"] {
        fs::create_dir_all(confined_path(wiki_dir, &wiki_dir.join(d))?)?;
    }
    write_if_missing(&confined_path(wiki_dir, &wiki_dir.join("PROTOCOL.md"))?, DEFAULT_PROTOCOL)?;
    write_if_missing(&confined_path(wiki_dir, &wiki_dir.join("README.md"))?, README)?;
    write_if_missing(&confined_path(wiki_dir, &wiki_dir.join(".gitattributes"))?, "* text=auto eol=lf\n")?;
    write_if_missing(&confined_path(wiki_dir, &wiki_dir.join(".gitignore"))?, ".locks/\n*.tmp\n")?;
    if !wiki_dir.join("index.md").exists() {
        refresh_index(wiki_dir, None)?;
    }
    Ok(())
}

/// The bundled PROTOCOL.md of earlier releases (hash_text, 16 characters). A wiki whose copy is one of
/// these never edited it, so an install may bring it up to date. The test below fails when
/// protocol/PROTOCOL.md changes and says which hash to move here.
const OLD_PROTOCOLS: [&str; 5] = ["18164903345112d0", "192abaacb1b60064", "09a27d24ebee04ca", "2323d98efcb27d61", "654998a3f71913a9"];
/// The hash of this release's protocol/PROTOCOL.md (the test below pins it).
#[cfg(test)]
const CURRENT_PROTOCOL: &str = "958d048836748db0";

/// Replaces an unedited PROTOCOL.md of an earlier release with this release's, keeping the old copy
/// in .history/. An edited one is the user's and stays. Returns whether it replaced it.
pub fn refresh_protocol(wiki_dir: &Path) -> Result<bool> {
    let file = confined_path(wiki_dir, &wiki_dir.join("PROTOCOL.md"))?;
    let Some(cur) = read_if_exists(&file)? else { return Ok(false) };
    let hash = hash_text(&cur, 16);
    if !OLD_PROTOCOLS.contains(&hash.as_str()) || to_lf(&cur) == to_lf(DEFAULT_PROTOCOL) {
        return Ok(false);
    }
    let hist = confined_path(wiki_dir, &wiki_dir.join(".history").join(format!("PROTOCOL-{}.md", history_stamp(&now()))))?;
    fs::create_dir_all(hist.parent().unwrap())?;
    write_if_missing(&hist, &cur)?;
    atomic_write(&file, &to_lf(DEFAULT_PROTOCOL), None, Some(Some(&hash)))?;
    Ok(true)
}

pub fn read_protocol(wiki_dir: &Path) -> String {
    confined_path(wiki_dir, &wiki_dir.join("PROTOCOL.md")).ok().and_then(|p| read_if_exists(&p).ok().flatten()).unwrap_or_else(|| DEFAULT_PROTOCOL.to_string())
}

// ---------------------------------------------------------------- pages and the index

#[derive(Clone, Debug)]
pub struct Page {
    pub slug: String,
    pub rel: String,
    pub title: String,
    pub kind: String,
    pub summary: String,
    pub tags: Vec<String>,
    /// Other words a person would search for this page with (the curator writes them).
    pub aliases: Vec<String>,
    pub updated: String,
    pub time: i64,
    pub meta: Meta,
    pub body: String,
}

fn first_heading(body: &str) -> String {
    static H: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#\s+(.+)$").unwrap());
    H.captures(body).map(|c| c[1].trim().to_string()).unwrap_or_default()
}

pub fn page_from_text(slug: &str, text: &str, mtime: i64) -> Page {
    let (meta, body) = frontmatter::parse(text);
    let title = one_line(&non_empty(&[meta.str("title"), first_heading(&body), slug.to_string()]));
    let updated = meta.str("updated");
    Page {
        slug: slug.to_string(),
        rel: format!("pages/{slug}.md"),
        title,
        kind: non_empty(&[meta.str("type"), "topic".into()]).to_lowercase(),
        summary: one_line(&meta.str("summary")),
        tags: norm_tags_val(meta.get("tags")),
        aliases: norm_tags_val(meta.get("aliases")),
        time: parse_ms(&updated).unwrap_or(mtime),
        updated,
        meta,
        body,
    }
}

fn non_empty(options: &[String]) -> String {
    options.iter().find(|s| !s.is_empty()).cloned().unwrap_or_default()
}

pub fn list_pages(wiki_dir: &Path) -> Result<Vec<Page>> {
    let dir = confined_path(wiki_dir, &wiki_dir.join("pages"))?;
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut pages = Vec::new();
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        let Some(slug) = n.strip_suffix(".md") else { continue };
        if confined_path(wiki_dir, &e.path()).is_err() {
            continue;
        }
        let Some((len, mtime)) = crate::readcache::stamp(&e) else { continue };
        let Some(text) = crate::readcache::read(&e.path(), len, mtime) else { continue }; // deleted or replaced mid-read
        pages.push(page_from_text(slug, &text, (mtime / 1_000_000) as i64));
    }
    pages.sort_by(|a, b| collate_cmp(&a.slug, &b.slug));
    Ok(pages)
}

fn link_text(s: &str) -> String {
    s.replace('|', "/").replace("]]", ")")
}

pub fn render_index(pages: &[Page]) -> String {
    let mut groups: Vec<(String, Vec<&Page>)> = Vec::new();
    for p in pages {
        match groups.iter_mut().find(|(t, _)| *t == p.kind) {
            Some((_, l)) => l.push(p),
            None => groups.push((p.kind.clone(), vec![p])),
        }
    }
    let mut order: Vec<String> = PAGE_TYPES.iter().filter(|t| groups.iter().any(|(g, _)| g == *t)).map(|t| t.to_string()).collect();
    let mut extra: Vec<String> = groups.iter().map(|(g, _)| g.clone()).filter(|g| !PAGE_TYPES.contains(&g.as_str())).collect();
    extra.sort();
    order.extend(extra);
    let mut out = vec!["<!-- GENERATED by agent-wiki from page frontmatter. Do not edit: it is rewritten on every page change. -->".to_string(), String::new(), "# Index".into(), String::new()];
    if pages.is_empty() {
        out.push("_No pages yet._".into());
        out.push(String::new());
    }
    for t in order {
        out.push(format!("## {}", type_heading(&t)));
        out.push(String::new());
        let mut list = groups.iter().find(|(g, _)| *g == t).map(|(_, l)| l.clone()).unwrap_or_default();
        list.sort_by(|a, b| collate_cmp(&a.title, &b.title));
        for p in list {
            let summary = if p.summary.is_empty() { String::new() } else { format!(" - {}", p.summary) };
            out.push(format!("- [[{}|{}]]{summary}", p.slug, link_text(&p.title)));
        }
        out.push(String::new());
    }
    out.join("\n")
}

fn with_one_newline(s: &str) -> String {
    format!("{}\n", s.trim_end_matches('\n'))
}

/// Regenerates index.md, writing only if it changed.
pub fn refresh_index(wiki_dir: &Path, pages: Option<&[Page]>) -> Result<bool> {
    let owned;
    let pages = match pages {
        Some(p) => p,
        None => {
            owned = list_pages(wiki_dir)?;
            &owned
        }
    };
    let next = with_one_newline(&to_lf(&render_index(pages)));
    let file = confined_path(wiki_dir, &wiki_dir.join("index.md"))?;
    if read_if_exists(&file)?.as_deref() == Some(next.as_str()) {
        return Ok(false);
    }
    atomic_write(&file, &next, None, None)?;
    Ok(true)
}

pub struct UpsertInput {
    pub app: String,
    pub title: String,
    pub slug: Option<String>,
    pub kind: Option<String>,
    pub summary: Option<String>,
    pub tags: Option<Value>,
    pub content: String,
    pub mode: Option<String>,
}

pub struct UpsertResult {
    pub action: &'static str,
    pub rel: String,
    pub history_rel: Option<String>,
}

/// Direct-mode page write (writeMode "direct"): create, append a dated section, or replace.
pub fn upsert_page(wiki_dir: &Path, input: &UpsertInput) -> Result<UpsertResult> {
    let app = normalize_app(&input.app);
    let title = one_line(&input.title);
    if title.is_empty() {
        return wiki_err("`title` is required.");
    }
    let slug = match &input.slug {
        Some(s) if !s.is_empty() => s.trim().to_lowercase(),
        _ => slugify(&title),
    };
    if !SLUG_RE.is_match(&slug) {
        return wiki_err(format!("Invalid slug \"{slug}\". Use 1-80 lowercase letters, digits and hyphens, e.g. \"atlas\"."));
    }
    let mode = input.mode.clone().unwrap_or_else(|| "append".into());
    if mode != "append" && mode != "replace" {
        return wiki_err("`mode` must be \"append\" or \"replace\".");
    }
    let kind = input.kind.as_ref().map(|t| t.trim().to_lowercase());
    static TYPE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9-]{0,30}$").unwrap());
    if let Some(t) = kind.as_ref().filter(|t| !t.is_empty())
        && !TYPE_RE.is_match(t)
    {
        return wiki_err(format!("Invalid type \"{t}\"."));
    }
    let summary = input.summary.as_ref().map(|s| one_line(s));
    let tags = input.tags.as_ref().map(norm_tags);
    let content = to_lf(&input.content).trim().to_string();
    if content.is_empty() {
        return wiki_err("`content` is required.");
    }
    let tags_joined = tags.clone().unwrap_or_default().join(" ");
    assert_no_secrets(&[("title", &title), ("summary", summary.as_deref().unwrap_or("")), ("tags", &tags_joined), ("content", &content), ("slug", &slug)])?;
    with_lock(wiki_dir, || {
        let now = now();
        let stamp = local_iso(&now);
        let file = confined_path(wiki_dir, &wiki_dir.join("pages").join(format!("{slug}.md")))?;
        let existing = read_if_exists(&file)?;
        let mut history_rel = None;
        let action;
        let (mut meta, body);
        match &existing {
            None => {
                action = "created";
                meta = Meta::default();
                meta.set("title", title.clone());
                meta.set("type", kind.clone().filter(|k| !k.is_empty()).unwrap_or_else(|| "topic".into()));
                meta.set("summary", summary.clone().unwrap_or_default());
                meta.set("tags", tags.clone().unwrap_or_default());
                meta.set("created", stamp.clone());
                meta.set("updated", stamp.clone());
                meta.set("updated_by", app.clone());
                body = if content.starts_with("# ") || content.starts_with("#\t") { content.clone() } else { format!("# {title}\n\n{content}") };
            }
            Some(existing) => {
                let (parsed, pbody) = frontmatter::parse(existing);
                meta = parsed;
                if let Some(k) = kind.clone().filter(|k| !k.is_empty()) {
                    meta.set("type", k);
                }
                if meta.str("type").is_empty() {
                    meta.set("type", "topic");
                }
                if let Some(s) = summary.clone().filter(|s| !s.is_empty()) {
                    meta.set("summary", s);
                }
                if !meta.has("summary") {
                    meta.set("summary", "");
                }
                if mode == "replace" {
                    action = "replaced";
                    let dir = confined_path(wiki_dir, &wiki_dir.join(".history").join("pages").join(&slug))?;
                    fs::create_dir_all(&dir)?;
                    let base = history_stamp(&now);
                    let mut i = 1;
                    loop {
                        let name = if i == 1 { format!("{base}.md") } else { format!("{base}-{i}.md") };
                        if write_if_missing(&dir.join(&name), existing)? {
                            history_rel = Some(format!(".history/pages/{slug}/{name}"));
                            break;
                        }
                        i += 1;
                    }
                    meta.set("title", title.clone());
                    let t = tags.clone().unwrap_or_else(|| norm_tags_val(meta.get("tags")));
                    meta.set("tags", t);
                    body = if content.starts_with("# ") || content.starts_with("#\t") { content.clone() } else { format!("# {title}\n\n{content}") };
                } else {
                    action = "updated";
                    if meta.str("title").is_empty() {
                        meta.set("title", title.clone());
                    }
                    let mut t = norm_tags_val(meta.get("tags"));
                    for x in tags.clone().unwrap_or_default() {
                        if !t.contains(&x) {
                            t.push(x);
                        }
                    }
                    meta.set("tags", t);
                    body = format!("{}\n\n### {} {} ({app})\n\n{content}", pbody.trim_end(), local_date(&now), local_hm(&now));
                }
                if meta.str("created").is_empty() {
                    meta.set("created", stamp.clone());
                }
                meta.set("updated", stamp.clone());
                meta.set("updated_by", app.clone());
            }
        }
        atomic_write(&file, &frontmatter::serialize(&meta, &body), None, None)?;
        refresh_index(wiki_dir, None)?;
        let verb = match action {
            "created" => "Page created",
            "updated" => "Page updated",
            _ => "Page rewritten",
        };
        append_to_log(wiki_dir, &now, &format!("- {} · {app} · {verb}: {} [[{slug}]]", local_hm(&now), one_line(&meta.str("title"))), true)?;
        Ok(UpsertResult { action, rel: format!("pages/{slug}.md"), history_rel })
    })
}

// ---------------------------------------------------------------- the log

pub fn log_rel(date: &str) -> String {
    format!("log/{}/{date}.md", date.get(..4).filter(|y| y.bytes().all(|b| b.is_ascii_digit())).unwrap_or("undated"))
}

fn log_file(wiki_dir: &Path, date: &str) -> PathBuf {
    let mut p = wiki_dir.to_path_buf();
    for part in log_rel(date).split('/') {
        p.push(part);
    }
    p
}

static COMPACT_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^- [0-9]{2}:[0-9]{2} · ").unwrap());

pub fn append_to_log(wiki_dir: &Path, now: &Time, chunk: &str, compact: bool) -> Result<String> {
    let date = local_date(now);
    let file = confined_path(wiki_dir, &log_file(wiki_dir, &date))?;
    let mut text = read_if_exists(&file)?.unwrap_or_default();
    if text.trim().is_empty() {
        text = format!("# {date}\n");
    }
    let last_line = text.trim_end().rsplit('\n').next().unwrap_or("").to_string();
    let sep = if compact && COMPACT_LINE.is_match(&last_line) { "\n" } else { "\n\n" };
    atomic_write(&file, &format!("{}{sep}{}\n", text.trim_end(), chunk.trim()), None, None)?;
    Ok(log_rel(&date))
}

/// Direct-mode log entry (writeMode "direct"). Returns (rel, heading).
pub fn append_log(wiki_dir: &Path, app: &str, title: &str, body: &str, tags: &Value, pages: &Value) -> Result<(String, String)> {
    let app = normalize_app(app);
    let title = one_line(title);
    if title.is_empty() {
        return wiki_err("`title` is required.");
    }
    let body = to_lf(body).trim().to_string();
    let tags = norm_tags(tags);
    let pages = norm_page_refs(pages);
    assert_no_secrets(&[("title", &title), ("body", &body), ("tags", &tags.join(" ")), ("pages", &pages.join(" "))])?;
    with_lock(wiki_dir, || {
        let now = now();
        let heading = format!("## {} · {app} · {title}", local_hm(&now));
        let mut meta_lines = vec![];
        if !tags.is_empty() {
            meta_lines.push(format!("tags: {}", tags.join(", ")));
        }
        if !pages.is_empty() {
            meta_lines.push(format!("pages: {}", pages.iter().map(|p| format!("[[{p}]]")).collect::<Vec<_>>().join(", ")));
        }
        let mut chunk = heading.clone();
        if !meta_lines.is_empty() {
            chunk.push_str(&format!("\n\n{}", meta_lines.join("  \n")));
        }
        if !body.is_empty() {
            chunk.push_str(&format!("\n\n{body}"));
        }
        let rel = append_to_log(wiki_dir, &now, &chunk, false)?;
        Ok((rel, heading))
    })
}

pub fn read_log_day(wiki_dir: &Path, date: &str) -> Option<String> {
    read_if_exists(&confined_path(wiki_dir, &log_file(wiki_dir, date)).ok()?).ok().flatten()
}

/// The last `n` local dates, newest first.
pub fn days_back(n: i64) -> Vec<String> {
    let today = Local::now().date_naive();
    (0..n).map(|i| (today - CDuration::days(i)).format("%Y-%m-%d").to_string()).collect()
}

/// Last `days` of log, chronological, truncated from the oldest end to `cap` characters.
pub fn recent_log(wiki_dir: &Path, days: i64, cap: usize) -> String {
    let mut dates = days_back(days);
    dates.reverse();
    let texts: Vec<String> = dates
        .iter()
        .filter_map(|d| read_log_day(wiki_dir, d))
        .filter(|t| !t.trim().is_empty())
        .map(|t| strip_markers(&t).split('\n').filter(|l| !l.starts_with("sources: note:")).collect::<Vec<_>>().join("\n").trim().to_string())
        .collect();
    let joined = texts.join("\n\n");
    let len = joined.chars().count();
    if len <= cap {
        return joined;
    }
    let mut cut: String = joined.chars().skip(len - cap).collect();
    static BOUNDARY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n(?:#{1,2} |- [0-9]{2}:[0-9]{2} · )").unwrap());
    if let Some(m) = BOUNDARY.find(&cut) {
        cut = cut[m.start() + 1..].to_string();
    }
    format!("(older entries truncated; read a day with wiki_read(\"YYYY-MM-DD\"))\n\n{cut}")
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Headline {
    pub date: String,
    pub time: String,
    pub text: String,
}

/// Newest log headlines first.
pub fn recent_headlines(wiki_dir: &Path, days: i64, max: usize) -> Vec<Headline> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:## |- )([0-9]{2}:[0-9]{2}) · (.+)$").unwrap());
    let mut out = Vec::new();
    for date in days_back(days) {
        let Some(text) = read_log_day(wiki_dir, &date) else { continue };
        let mut day: Vec<Headline> = text.split('\n').filter_map(|l| LINE.captures(l)).map(|c| Headline { date: date.clone(), time: c[1].to_string(), text: c[2].trim().to_string() }).collect();
        day.reverse();
        out.extend(day);
        if out.len() >= max {
            break;
        }
    }
    out.truncate(max);
    out
}

// ---------------------------------------------------------------- search

const STOPWORDS: &str = "a an and are as at be by can could did do does for from had has have how i in into is it its me my of on or our should so than that the their them then there these this those to was we were what when where which who why will with would you your about any all";

pub fn tokenize(query: &str) -> Vec<String> {
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}][\p{L}\p{N}._/-]*").unwrap());
    static TRAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[._/-]+$").unwrap());
    let lower = to_lf(query).to_lowercase();
    let mut uniq: Vec<String> = Vec::new();
    for m in TOKEN.find_iter(&lower) {
        let t = TRAIL.replace(m.as_str(), "").to_string();
        if (t.chars().count() > 1 || t.chars().any(|c| c.is_ascii_digit())) && !uniq.contains(&t) {
            uniq.push(t);
        }
    }
    let stop: HashSet<&str> = STOPWORDS.split(' ').collect();
    let kept: Vec<String> = uniq.iter().filter(|t| !stop.contains(t.as_str())).cloned().collect();
    if kept.is_empty() { uniq } else { kept }
}

/// (date, rel, file, size, modification time) of every log day.
pub(crate) fn list_log_files(wiki_dir: &Path) -> Vec<(String, String, PathBuf, u64, i128)> {
    let mut out = Vec::new();
    let Ok(base) = confined_path(wiki_dir, &wiki_dir.join("log")) else { return out };
    let Ok(years) = fs::read_dir(&base) else { return out };
    for y in years.flatten() {
        let yn = y.file_name().to_string_lossy().to_string();
        if !(yn.len() == 4 && yn.chars().all(|c| c.is_ascii_digit())) {
            continue;
        }
        let Ok(year) = confined_path(wiki_dir, &y.path()) else { continue };
        let Ok(files) = fs::read_dir(year) else { continue };
        for f in files.flatten() {
            if confined_path(wiki_dir, &f.path()).is_err() {
                continue;
            }
            let n = f.file_name().to_string_lossy().to_string();
            if let Some(date) = n.strip_suffix(".md").filter(|d| DATE_RE.is_match(d))
                && let Some((len, mtime)) = crate::readcache::stamp(&f)
            {
                out.push((date.to_string(), format!("log/{yn}/{n}"), f.path(), len, mtime));
            }
        }
    }
    out
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Hit {
    pub kind: &'static str,
    pub rel: String,
    /// What wiki_read takes: a page slug, a log date or note:<id>.
    pub target: String,
    pub label: String,
    /// The anchor of the section that matched best ("" for a page's intro or a note), and its heading.
    pub section: String,
    pub heading: String,
    pub score: f64,
    pub snippets: Vec<String>,
}

impl Hit {
    /// target#section, as wiki_read takes it.
    pub fn read_target(&self) -> String {
        if self.section.is_empty() { self.target.clone() } else { format!("{}#{}", self.target, self.section) }
    }
}

/// Section-level BM25 search (see search.rs), without filters.
pub fn search(wiki_dir: &Path, query: &str, scope: &str, limit: i64) -> Result<Vec<Hit>> {
    crate::search::search(wiki_dir, query, scope, limit, &crate::search::Filters::default())
}

/// A number as JavaScript prints it (43.3, 28).
pub fn js_number(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e21 { format!("{}", x as i64) } else { format!("{x}") }
}

pub fn format_search_results(results: &[Hit]) -> String {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let heading = if r.heading.is_empty() { String::new() } else { format!(" › {}", r.heading) };
            let mut s = format!("{}. {} - {}{heading} (read: \"{}\", score {})", i + 1, r.rel, r.label, r.read_target(), js_number(r.score));
            for sn in &r.snippets {
                s.push_str(&format!("\n   > {sn}"));
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- read

fn is_inside(root: &Path, p: &Path) -> bool {
    p.starts_with(root)
}

/// `abs` with its last part matched case-insensitively in its folder (case-sensitive filesystems).
fn case_insensitive(abs: &Path) -> Option<PathBuf> {
    let dir = abs.parent()?;
    let want = abs.file_name()?.to_string_lossy().to_lowercase();
    let names: Vec<String> = fs::read_dir(dir).ok()?.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    let hit = names.iter().find(|n| n.to_lowercase() == want).or_else(|| names.iter().find(|n| n.to_lowercase() == format!("{want}.md")))?;
    Some(dir.join(hit))
}

/// Accepts a log date, a page slug, [[slug]], or a path relative to the wiki root. Returns (rel, text).
/// The wiki folder with links resolved, once per process (resolving costs a file open on Windows).
fn canonical_root(root: &Path) -> std::io::Result<PathBuf> {
    static ROOTS: LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, PathBuf>>> = LazyLock::new(Default::default);
    if let Some(r) = ROOTS.lock().unwrap_or_else(|e| e.into_inner()).get(root) {
        return Ok(r.clone());
    }
    let r = fs::canonicalize(root)?;
    ROOTS.lock().unwrap_or_else(|e| e.into_inner()).insert(root.to_path_buf(), r.clone());
    Ok(r)
}

pub fn read_target(wiki_dir: &Path, target: &str) -> Result<(String, String)> {
    read_section(wiki_dir, target, None)
}

/// read_target, limited to one section when `section` (or a "#section" on the target) names one:
/// the page's header line, then that section. Sections are a page's ## and ### headings and a log
/// day's entries, as search results cite them.
pub fn read_section(wiki_dir: &Path, target: &str, section: Option<&str>) -> Result<(String, String)> {
    let (target, from_target) = match target.split_once('#') {
        Some((t, s)) if !t.trim().is_empty() => (t, Some(s.trim())),
        _ => (target, None),
    };
    let want = section.map(str::trim).filter(|s| !s.is_empty()).or(from_target.filter(|s| !s.is_empty()));
    let (rel, text) = read_whole(wiki_dir, target)?;
    let Some(want) = want else { return Ok((rel, text)) };
    if !(rel.starts_with("pages/") || rel.starts_with("log/")) {
        return wiki_err(format!("Sections are for pages and log days; read \"{target}\" whole."));
    }
    let Some((heading, body)) = crate::search::section_of(&rel, &text, want) else {
        let names = crate::search::anchors_of(&rel, &text);
        return wiki_err(format!("No section \"{want}\" in {rel}. Its sections: {}.", if names.is_empty() { "(none)".to_string() } else { names.join(", ") }));
    };
    let head = if rel.starts_with("pages/") {
        let p = page_from_text(rel.trim_start_matches("pages/").trim_end_matches(".md"), &text, 0);
        format!("# {} [{}]{}", p.title, p.kind, if p.summary.is_empty() { String::new() } else { format!(" - {}", p.summary) })
    } else {
        format!("# {}", rel.rsplit('/').next().unwrap_or("").trim_end_matches(".md"))
    };
    Ok((format!("{rel}#{}", crate::search::anchor(want)), format!("{head}\n\n## {heading}\n\n{}", body.trim())))
}

fn read_whole(wiki_dir: &Path, target: &str) -> Result<(String, String)> {
    let mut t = target.trim().replace('\\', "/");
    t = t.strip_prefix("[[").unwrap_or(&t).to_string();
    t = t.strip_suffix("]]").unwrap_or(&t).to_string();
    t = t.split('|').next().unwrap_or("").trim().to_string();
    if t.is_empty() {
        return wiki_err("`target` is required: a page slug, a log date (YYYY-MM-DD) or a path.");
    }
    if let Some(id) = t.strip_prefix("note:") {
        return inbox::read_note(wiki_dir, id.trim());
    }
    let root = absolute(wiki_dir);
    let rel = if DATE_RE.is_match(&t) {
        let r = log_rel(&t);
        if !root.join(&r).exists() {
            return wiki_err(format!("No log entries for {t}."));
        }
        r
    } else if SLUG_RE.is_match(&t) && root.join("pages").join(format!("{t}.md")).exists() {
        format!("pages/{t}.md")
    } else if SLUG_RE.is_match(&t.to_lowercase()) && root.join("pages").join(format!("{}.md", t.to_lowercase())).exists() {
        format!("pages/{}.md", t.to_lowercase())
    } else {
        t.clone()
    };
    let mut abs = normalize(&root.join(&rel));
    if !is_inside(&root, &abs) {
        return wiki_err(format!("Refused: \"{target}\" is outside the wiki folder."));
    }
    if !abs.exists() && !abs.to_string_lossy().to_lowercase().ends_with(".md") {
        let with_md = PathBuf::from(format!("{}.md", abs.display()));
        if with_md.exists() {
            abs = with_md;
        }
    }
    if !abs.exists()
        && let Some(found) = case_insensitive(&abs)
    {
        abs = found;
    }
    if !abs.exists() {
        return wiki_err(format!("Not found: \"{target}\". Use wiki_search, or a page slug from the index (wiki_read(\"index.md\"))."));
    }
    let real_root = canonical_root(&root)?;
    let real = fs::canonicalize(&abs)?;
    if !is_inside(&real_root, &real) {
        return wiki_err(format!("Refused: \"{target}\" resolves outside the wiki folder."));
    }
    let rel_out = abs.strip_prefix(&root).map(disp).unwrap_or_default();
    let rel_out = if rel_out.is_empty() { ".".to_string() } else { rel_out };
    if abs.is_dir() {
        let mut entries: Vec<String> = fs::read_dir(&abs)?
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with('.') && rel_out != "." {
                    return None;
                }
                Some(if e.path().is_dir() { format!("{n}/") } else { n })
            })
            .collect();
        entries.sort();
        let list: Vec<String> = entries.iter().map(|e| format!("- {e}")).collect();
        return Ok((rel_out.clone(), format!("Directory {rel_out}/:\n{}", list.join("\n"))));
    }
    let text = to_lf(&String::from_utf8_lossy(&fs::read(&abs)?));
    Ok((rel_out, text))
}

// ---------------------------------------------------------------- wiki_start

pub fn start_context(wiki_dir: &Path, app: &str, topic: &str) -> Result<String> {
    let now = now();
    let protocol = read_protocol(wiki_dir);
    let pages = list_pages(wiki_dir)?;
    let recent = recent_log(wiki_dir, 7, 5000);
    let notes = inbox::list_notes(wiki_dir).unwrap_or_default();
    let mut out = vec![
        "# Agent Wiki: session start".to_string(),
        String::new(),
        format!("Now: {} ({}). Wiki folder: {}. You are: {}.", local_iso(&now), now.format("%A"), disp(wiki_dir), normalize_app(app)),
        String::new(),
        "---".into(),
        String::new(),
        protocol.trim().to_string(),
        String::new(),
        "---".into(),
        String::new(),
        format!("## Page index ({})", pages.len()),
        String::new(),
    ];
    if pages.is_empty() {
        out.push("(no pages yet)".into());
    }
    for p in &pages {
        let summary = if p.summary.is_empty() { String::new() } else { format!(" - {}", p.summary) };
        out.push(format!("- {} [{}] {}{summary}", p.slug, p.kind, p.title));
    }
    let t = one_line(topic);
    if !t.is_empty() {
        out.push(String::new());
        out.push(format!("## Related to \"{t}\""));
        out.push(String::new());
        let hits = search(wiki_dir, &t, "all", 5)?;
        out.push(if hits.is_empty() { "(nothing related found)".into() } else { format_search_results(&hits) });
    }
    if !notes.is_empty() {
        out.push(String::new());
        out.extend(render_pending_notes(&notes, 20, 4000));
    }
    out.push(String::new());
    out.push("## Recent activity (last 7 days)".into());
    out.push(String::new());
    out.push(if recent.is_empty() { "(no activity logged yet)".into() } else { recent });
    out.push(String::new());
    out.push("Read a page with wiki_read(\"<slug>\") or a day with wiki_read(\"YYYY-MM-DD\").".into());
    Ok(out.join("\n"))
}

/// wiki_start section: notes the curator has not organized yet.
pub fn render_pending_notes(notes: &[inbox::Note], max: usize, cap: usize) -> Vec<String> {
    let mut out = vec![format!("## Pending notes ({}, sent by apps, not yet organized into pages)", notes.len()), String::new()];
    let mut used = 0;
    for n in notes.iter().take(max) {
        let body = one_line(&n.body);
        let snippet = if body.chars().count() > 240 { format!("{}...", take_chars(&body, 237)) } else { body };
        let label = match n.status.as_str() {
            "dead" => "NOT CURATED: failed, see the tray".to_string(),
            s => s.to_string(),
        };
        let title = if n.title.is_empty() { "(untitled)".to_string() } else { n.title.clone() };
        let line = format!("- {} {} · {} · {title} [{label}] (note:{}){}", n.date, n.time, n.app, n.id, if snippet.is_empty() { String::new() } else { format!(": {snippet}") });
        let len = line.encode_utf16().count();
        if used + len > cap {
            break;
        }
        used += len;
        out.push(line);
    }
    if notes.len() > max {
        out.push(format!("- ...and {} more", notes.len() - max));
    }
    out.push(String::new());
    out.push("Read one in full with wiki_read(\"note:<id>\"). Do not send them again: they are already saved.".into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confined_paths_reject_escape_and_allow_new_children() {
        let root = std::env::temp_dir().join(format!("aw-confined-{}", random_hex(8)));
        fs::create_dir_all(&root).unwrap();
        assert!(confined_path(&root, &root.join("pages/new.md")).is_ok());
        assert!(confined_path(&root, &root.join("../outside.md")).is_err());
        assert!(confined_path(&root, &root.with_extension("outside")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn linked_pages_and_log_directories_never_enter_search() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("aw-linked-read-{}", random_hex(8)));
        let root = base.join("wiki");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("pages")).unwrap();
        fs::create_dir_all(root.join("log")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("2026-10-07.md"), "private outside text").unwrap();
        symlink(outside.join("2026-10-07.md"), root.join("pages/linked.md")).unwrap();
        symlink(&outside, root.join("log/2026")).unwrap();
        assert!(list_pages(&root).unwrap().is_empty());
        assert!(list_log_files(&root).is_empty());
        assert!(read_log_day(&root, "2026-10-07").is_none());
        assert!(confined_path(&root, &root.join("log/2026/new.md")).is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn an_edited_protocol_keeps_the_old_hash_for_upgrades() {
        let h = hash_text(DEFAULT_PROTOCOL, 16);
        assert_eq!(
            h, CURRENT_PROTOCOL,
            "protocol/PROTOCOL.md changed. In rust/crates/core/src/wiki.rs, add \"{CURRENT_PROTOCOL}\" to OLD_PROTOCOLS (so installs update unedited copies) and set CURRENT_PROTOCOL to \"{h}\"."
        );
        assert!(!OLD_PROTOCOLS.contains(&CURRENT_PROTOCOL), "CURRENT_PROTOCOL must not be in OLD_PROTOCOLS");
    }

    #[test]
    fn dates_and_ids_take_only_ascii_digits() {
        // Devanagari digits are digits to Unicode \d but 3 bytes each, so byte slicing would split them.
        let date = "१२३४-०१-०१";
        assert!(!DATE_RE.is_match(date));
        assert!(!crate::inbox::is_note_id(&format!("{date}_10-00-00-000-abcdef")));
        assert!(DATE_RE.is_match("2026-10-06") && crate::inbox::is_note_id("2026-10-06_10-00-00-000-abcdef"));
    }

    #[test]
    fn no_regex_in_the_crates_uses_unicode_digit_classes() {
        // A lint: \d in the regex crate matches every Unicode digit. Write [0-9].
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let mut found = vec![];
        let mut stack = vec![crates];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs" || x == "json") {
                    let text = fs::read_to_string(&p).unwrap_or_default();
                    for (i, line) in text.lines().enumerate() {
                        let in_regex = line.contains("Regex::new") || line.contains("\"pattern\"");
                        if in_regex && line.contains(r"\d") {
                            found.push(format!("{}:{}", p.display(), i + 1));
                        }
                    }
                }
            }
        }
        assert!(found.is_empty(), "\\d matches any Unicode digit (then byte slicing panics): write [0-9] in {found:#?}");
    }
}
