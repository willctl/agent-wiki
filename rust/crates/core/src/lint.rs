//! The lint (M4): the curator, when it has nothing to file, reviews one page at a time with the index
//! and the recent log, and proposes a cleanup in the same edit-plan form, as a held change of kind
//! "lint". With approvals automatic (the default) it is applied at once, with a notification and
//! Undo; with "Ask me first" it waits for the person in the window, because models are weak at
//! judging what has quietly gone stale.
//!
//! When: on the cleanup schedule (.curator/settings.json cleanupSchedule: daily at 03:00 by default,
//! any cron schedule, or off), each page only if it changed since its last review; and soon after a
//! page grows past the size cap. A scheduled time missed while the computer slept is caught up when
//! the curator is next idle. The first pass after install is the one-time tidy of pages written before
//! the current-state rules. config.json curator.lint "off" turns cleanups off on one computer.

use crate::cron::Cron;
use crate::curator::{self, Ctx, CuratorCfg, HeldOp, ShownPage, page_write, plan_schema, size_problems, to_json_indent, validate_plan};
use crate::text::*;
use crate::wiki::{self, atomic_write, read_if_exists};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const LINT_INSTRUCTIONS: &str = r###"You maintain a personal wiki that one person's AI apps (Claude, ChatGPT, Codex) read at the start of every conversation. A curator files new notes into it; your job is to tidy one page so it stays correct and easy to use. Return an edit plan as JSON. Code checks it, then applies it or shows it to the person first, depending on their setting; either way the person sees each change and can undo it. Propose only changes that keep every fact correct.

Look for, on the page below:
- contradictions within the page, or with the recent log;
- stale items: "pending", "untested", "open" or "next" items that the recent log or the page itself shows are done;
- time-relative wording ("today", "tomorrow", "this week", "first run pending", "as of 15:58"): it refers to the page's last update ("updated" in the input), so state it with that date, or move it to History with that date. Times of day are noise on a page: leave them out;
- facts that later log entries changed without the page saying so: change them in place and add a "## History" line (date, what changed, old value);
- verification steps, test counts, timings and status narration: they belong in the log (they are there already), so take them off the page;
- a summary that no longer matches the page;
- a page over the size cap (pageCap; its size is "chars"): move self-contained sections to new pages (creates in this plan) and leave a one-line summary with a [[link]] in their place;
- the findings that code reports in the input: broken [[links]] (fix the slug if the index has the page meant, otherwise drop the brackets);
- missing or thin aliases: the other words a person would search for this page with (broader and narrower terms, everyday words for technical ones and the reverse, abbreviations both ways, a place's country, a person's role, what a thing is for), up to 12, only words not already in the title, summary or tags. Search matches words, so a page without them is found only by its own words. Set aliases (the whole list) with a patch; the body can stay as it is.

Rules
- Keep every fact: move it (to History, a new page or nowhere if the log has it), never drop it silently. Never invent facts, dates or reasons; use only the page, the index and the log.
- Keep History lines as they are. Keep the page's structure unless it is the problem.
- Small, separate edits are better than a rewrite: prefer patch over replace.
- A page that is fine needs no change: return an empty pages list.

Output: JSON matching the schema.
- notes: [] (there are no notes). log: []. forget: [].
- pages: operations on this page (patch or replace, with its hash as base_hash) and creates for split-off pages. note_ids: []. reason: one line naming the finding the change fixes.
  - patch: edits [{find, replace}], each find copied exactly from the current body and occurring exactly once.
  - create: a new slug, title, type, summary, tags, and the full body in content.
- summary: one sentence on what you found, or "No changes needed.""###;

/// What the lint remembers: when the last scheduled pass finished, and each page's hash when it was last reviewed.
#[derive(Default)]
struct State {
    last_pass: Option<String>,
    pages: HashMap<String, String>,
    pause_until: i64,
}

fn state_file(wiki_dir: &Path) -> PathBuf {
    wiki_dir.join(".curator").join("lint.json")
}

fn load(wiki_dir: &Path) -> State {
    let v: Value = read_if_exists(&state_file(wiki_dir)).ok().flatten().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    State {
        last_pass: v["lastPass"].as_str().map(String::from),
        pages: v["pages"].as_object().map(|o| o.iter().filter_map(|(k, h)| h.as_str().map(|h| (k.clone(), h.to_string()))).collect()).unwrap_or_default(),
        pause_until: v["pauseUntil"].as_i64().unwrap_or(0),
    }
}

fn save(wiki_dir: &Path, s: &State) {
    let pages: Map<String, Value> = s.pages.iter().map(|(k, h)| (k.clone(), json!(h))).collect();
    let v = json!({ "lastPass": s.last_pass, "pages": pages, "pauseUntil": s.pause_until });
    let _ = std::fs::create_dir_all(wiki_dir.join(".curator"));
    let _ = atomic_write(&state_file(wiki_dir), &format!("{}\n", serde_json::to_string_pretty(&v).unwrap_or_default()), None, None);
}

/// Pages with a change waiting for the person: the lint leaves them alone until it is decided.
fn pending_slugs(wiki_dir: &Path) -> HashSet<String> {
    crate::held::list(wiki_dir).iter().filter_map(|c| c["slug"].as_str().map(String::from)).collect()
}

/// A pass is due when a scheduled time has passed since the last pass finished, or none has run.
fn pass_due(st: &State, cron: &Cron) -> bool {
    st.last_pass.as_deref().and_then(parse_ms).is_none_or(|t| cron.prev(&chrono::Local::now()).is_some_and(|p| p.timestamp_millis() > t))
}

/// The next page to review now, if any: one that grew past the cap since its last review, else (when
/// a scheduled pass is due) one that changed since its last review. Marks a pass finished when nothing
/// is left to review in it.
pub fn next_due(wiki_dir: &Path, cfg: &CuratorCfg) -> Option<String> {
    if cfg.cleanups_off {
        return None;
    }
    let schedule = crate::settings::cleanup_schedule_and_problem(wiki_dir).0;
    let cron = schedule.cron()?;
    let mut st = load(wiki_dir);
    if st.pause_until > now_ms() {
        return None;
    }
    let waiting = pending_slugs(wiki_dir);
    let pages: Vec<(String, String, usize)> = wiki::list_pages(wiki_dir)
        .ok()?
        .into_iter()
        .filter(|p| !waiting.contains(&p.slug))
        .filter_map(|p| read_if_exists(&wiki_dir.join(&p.rel)).ok().flatten().map(|t| (p.slug.clone(), hash_text(&t, 16), p.body.encode_utf16().count())))
        .collect();
    let changed = |slug: &str, hash: &str| st.pages.get(slug).map(String::as_str) != Some(hash);
    if let Some((slug, ..)) = pages.iter().find(|(s, h, chars)| *chars > cfg.page_cap_chars && changed(s, h)) {
        return Some(slug.clone());
    }
    if !pass_due(&st, cron) {
        return None;
    }
    if let Some((slug, ..)) = pages.iter().find(|(s, h, _)| changed(s, h)) {
        return Some(slug.clone());
    }
    st.last_pass = Some(local_iso(&now()));
    save(wiki_dir, &st);
    None
}

/// What code can check by itself: links to pages that do not exist, a page nothing links to, the cap.
pub fn findings(page: &ShownPage, all: &[wiki::Page], cap: usize) -> Vec<String> {
    static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]*)?\]\]").unwrap());
    let known: HashSet<&str> = all.iter().map(|p| p.slug.as_str()).collect();
    let mut out = vec![];
    let mut broken: Vec<String> = LINK.captures_iter(&page.body).map(|c| c[1].trim().to_string()).filter(|s| !known.contains(s.as_str())).collect();
    broken.dedup();
    if !broken.is_empty() {
        out.push(format!("broken links (no such page): {}", broken.iter().map(|s| format!("[[{s}]]")).collect::<Vec<_>>().join(", ")));
    }
    let linked = all.iter().filter(|p| p.slug != page.slug).any(|p| p.body.contains(&format!("[[{}]]", page.slug)) || p.body.contains(&format!("[[{}|", page.slug)));
    if !linked && all.len() > 1 {
        out.push("no other page links here (an orphan); fine if it stands alone".into());
    }
    let chars = page.body.encode_utf16().count();
    if chars > cap {
        out.push(format!("the page is {chars} characters, over the cap of {cap}"));
    }
    out
}

pub fn build_prompt(ctx: &Ctx, page: &ShownPage, found: &[String], recent_log: &str, repair: Option<&[String]>) -> String {
    let input = json!({
        "now": ctx.now,
        "pageCap": ctx.page_cap,
        "findings": found,
        "index": ctx.index,
        "recentLog": recent_log,
        "page": {
            "slug": page.slug, "hash": page.hash, "title": page.title, "type": page.kind, "summary": page.summary, "tags": page.tags, "aliases": page.aliases,
            "updated": crate::frontmatter::parse(&page.text).0.str("updated"), "chars": page.body.encode_utf16().count(), "body": page.body,
        },
    });
    let mut text = format!("{LINT_INSTRUCTIONS}\n\n<input>\n{}\n</input>\n", to_json_indent(&input, 1));
    if let Some(problems) = repair {
        text.push_str("\nYour previous plan was rejected; nothing was shown to the person. Fix these problems and return the whole corrected plan:\n");
        text.push_str(&format!("{}\n", problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")));
    }
    text
}

/// The page as the lint sees it, and the context validation needs (no notes; only this page shown).
pub fn context(wiki_dir: &Path, slug: &str, cfg: &CuratorCfg) -> Option<(Ctx, ShownPage, Vec<wiki::Page>)> {
    let all = wiki::list_pages(wiki_dir).ok()?;
    let page = curator::read_page(wiki_dir, slug)?;
    let ctx = Ctx {
        now: local_iso(&now()),
        notes: vec![],
        index: all.iter().map(|p| json!({ "slug": p.slug, "title": p.title, "type": p.kind, "summary": p.summary })).collect(),
        pages: vec![page.clone()],
        known: all.iter().map(|p| p.slug.clone()).collect(),
        skipped: vec![],
        page_cap: cfg.page_cap_chars,
    };
    Some((ctx, page, all))
}

/// Records a valid lint plan as held cleanups for the person (and the page as reviewed). Returns how
/// many changes were proposed.
pub fn propose(wiki_dir: &Path, plan: &Value, ctx: &Ctx, page: &ShownPage, batch: &str) -> wiki::Result<usize> {
    let now = now();
    let mut held = vec![];
    for op in plan["pages"].as_array().into_iter().flatten() {
        let slug = op["slug"].as_str().unwrap_or("");
        let cur = if slug == page.slug { Some(page.text.as_str()) } else { None };
        if op["action"].as_str() == Some("create") && ctx.known.contains(slug) {
            continue; // validated against this, but a page may have appeared since
        }
        let Some(w) = page_write(op, cur, batch, &now) else { continue };
        let reason = op["reason"].as_str().map(one_line).filter(|r| !r.is_empty()).unwrap_or_else(|| "a cleanup".into());
        held.push(HeldOp { write: w, reasons: vec![format!("a cleanup proposes it: {reason}")], op: op.clone(), notes: vec![], kind: "lint".into() });
    }
    // Recording, applying (approvals automatic) and the review state change together, under the write lock.
    wiki::with_lock(wiki_dir, || {
        let mut st = load(wiki_dir);
        st.pages.insert(page.slug.clone(), page.hash.clone());
        if held.is_empty() {
            save(wiki_dir, &st);
            return Ok(0);
        }
        crate::held::record(wiki_dir, batch, &local_iso(&now), &held)?;
        if crate::settings::approvals(wiki_dir) == crate::settings::Approvals::Auto {
            // What the cleanup wrote counts as reviewed, so it is not due again until something else changes it.
            for slug in crate::held::apply_auto_locked(wiki_dir, batch)? {
                if let Some(text) = read_if_exists(&wiki_dir.join("pages").join(format!("{slug}.md")))? {
                    st.pages.insert(slug, hash_text(&text, 16));
                }
            }
            save(wiki_dir, &st);
            return Ok(held.len());
        }
        save(wiki_dir, &st);
        let title = one_line(&page.title);
        wiki::append_to_log(wiki_dir, &now, &format!("- {} · curator · Cleanup proposed for your OK: {title} [[{}]]", local_hm(&now), page.slug), true)?;
        Ok(held.len())
    })
}

/// The cleanup schedule as /status and the window show it: the setting in words, when the next pass
/// starts (or that one is due, to run when the curator is idle), and when the last one finished.
pub fn schedule_status(wiki_dir: &Path, off_here: bool) -> Value {
    let (schedule, problem) = crate::settings::cleanup_schedule_and_problem(wiki_dir);
    let st = load(wiki_dir);
    let cron = schedule.cron().filter(|_| !off_here);
    let due = cron.is_some_and(|c| pass_due(&st, c));
    let next = cron.filter(|_| !due).and_then(|c| c.next(&chrono::Local::now())).map(|d| local_iso(&d));
    json!({
        "schedule": schedule.as_str(),
        "description": schedule.describe(),
        "due": due,
        "next": next,
        "lastPass": st.last_pass,
        "problem": problem,
        "offOnThisComputer": off_here,
    })
}

/// After a failed review: try again in an hour, not on every idle moment.
pub fn back_off(wiki_dir: &Path) {
    let mut st = load(wiki_dir);
    st.pause_until = now_ms() + 3_600_000;
    save(wiki_dir, &st);
}

/// Checks a lint plan: the curator's validation, plus "no notes, no log".
pub fn problems(plan: &Value, ctx: &Ctx, first_pass: bool) -> Vec<String> {
    let mut p = validate_plan(plan, ctx);
    if plan["log"].as_array().is_some_and(|l| !l.is_empty()) {
        p.push("log must be empty: the lint writes no log entries".into());
    }
    if first_pass {
        p.extend(size_problems(plan, ctx));
    }
    p
}

pub fn schema() -> Value {
    plan_schema()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_findings() {
        let page = |slug: &str, body: &str| wiki::page_from_text(slug, &format!("---\ntitle: {slug}\n---\n\n{body}"), 0);
        let all = vec![page("a", "# A\n\nSee [[b]] and [[gone]] and [[gone|x]]."), page("b", "# B\n\nnothing")];
        let shown = |slug: &str| curator::ShownPage {
            slug: slug.into(),
            hash: "h".into(),
            title: slug.into(),
            kind: "topic".into(),
            summary: String::new(),
            tags: vec![],
            aliases: vec![],
            body: all.iter().find(|p| p.slug == slug).unwrap().body.clone(),
            text: String::new(),
        };
        let a = findings(&shown("a"), &all, 10);
        assert!(a[0].contains("[[gone]]") && !a[0].contains("[[b]]"), "{a:?}");
        assert!(a.iter().any(|f| f.contains("orphan")), "nothing links to a");
        assert!(a.iter().any(|f| f.contains("over the cap of 10")));
        let b = findings(&shown("b"), &all, 1000);
        assert!(b.is_empty(), "b is linked from a: {b:?}");
    }
}
