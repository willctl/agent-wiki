//! The SessionStart hook (src/session-start.mjs): prints the wiki's lead-in, page list and recent
//! activity as {"hookSpecificOutput": ...}. A broken or unreachable wiki never blocks a session: the
//! wiki is read on a worker thread, and after 5 s a fallback is printed and the process exits
//! (exiting ends the thread even if it is stuck on a hung path). stdin is drained, never waited on.
//! `agent-wiki hook stop` is the opt-in Stop hook (run_stop).

use aw_core::paths::process_env;
use aw_core::wiki;
use serde_json::json;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

const RULES: &str = "Agent Wiki is the user's shared long-term memory across Claude, ChatGPT and other AI apps, reached through the agent-wiki tools. Call wiki_start once before your first substantive reply and follow the protocol it returns; search the wiki before asking for context the user may have given before, and tell it what happened (decisions, outcomes, preferences, facts, follow-ups) with wiki_log; plain notes are fine, a curator files them into pages (never secrets).";
const FALLBACK: &str = "Agent Wiki (the user's shared memory across AI apps) is installed but could not be read just now; call wiki_start if the tools are available.";

fn collect() -> Option<String> {
    let (dir, _) = wiki::resolve_wiki_dir(&process_env()).ok()?;
    if !std::fs::metadata(&dir).ok()?.is_dir() {
        return None;
    }
    let pages = wiki::list_pages(&dir).ok()?;
    let headlines = wiki::recent_headlines(&dir, 3, 15);
    let slugs: Vec<&str> = pages.iter().map(|p| p.slug.as_str()).collect();
    let shown = &slugs[..slugs.len().min(80)];
    let more = if slugs.len() > shown.len() { format!(", ...and {} more", slugs.len() - shown.len()) } else { String::new() };
    let mut lines = vec![
        format!("{RULES} Wiki folder: {}.", wiki::disp(&dir)),
        String::new(),
        format!("Pages ({}): {}{more}", slugs.len(), if shown.is_empty() { "(none yet)".to_string() } else { shown.join(", ") }),
        String::new(),
        if headlines.is_empty() { "Recent activity (last 3 days): none.".to_string() } else { "Recent activity (last 3 days, newest first):".to_string() },
    ];
    lines.extend(headlines.iter().map(|h| format!("- {} {} · {}", h.date, h.time, h.text)));
    Some(lines.join("\n"))
}

const NUDGE: &str = "Before you stop: if this session produced anything durable (a decision and why, an outcome and where it lives, a preference the user stated, a fact about a project, person or system, a follow-up), tell the shared wiki with wiki_log now, with its source. Otherwise just stop. (Agent Wiki asks this once per session.)";

/// The Stop hook (M7, opt-in with config.json "hooks": {"nudge": true}): once per session, when the
/// transcript is long and has no wiki_log call, asks the model to log what is worth keeping before it
/// stops. Claude Code and Codex both take {"decision": "block", "reason": ...} as "continue with this".
/// Never blocks twice: not when a Stop hook already continued the turn (stop_hook_active), and a
/// marker per session remembers that it asked (or that the session logged).
pub fn run_stop() -> ! {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Some hosts leave stdin open: stop at the first complete JSON object.
        let (mut input, mut buf, mut stdin) = (Vec::new(), [0u8; 8192], std::io::stdin());
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            input.extend_from_slice(&buf[..n]);
            if serde_json::from_slice::<serde_json::Value>(&input).is_ok() {
                break;
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&input).into_owned());
    });
    let input = rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default();
    if let Some(reason) = std::panic::catch_unwind(|| nudge_reason(&input)).ok().flatten() {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{}", json!({ "decision": "block", "reason": reason }));
        let _ = stdout.flush();
    }
    std::process::exit(0)
}

fn nudge_reason(input: &str) -> Option<String> {
    let env = process_env();
    let config = wiki::read_config(&env).ok()?;
    if config["hooks"]["nudge"].as_bool() != Some(true) {
        return None;
    }
    let ev: serde_json::Value = serde_json::from_str(input).ok()?;
    if ev["stop_hook_active"].as_bool() == Some(true) {
        return None;
    }
    let session: String = ev["session_id"].as_str()?.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(100).collect();
    if session.is_empty() {
        return None;
    }
    let marks = aw_core::paths::AppPaths::current().state_dir.join("nudged");
    let mark = marks.join(&session);
    if mark.exists() {
        return None;
    }
    let transcript = std::fs::read_to_string(ev["transcript_path"].as_str()?).ok()?;
    let min_lines = config["hooks"]["nudgeMinLines"].as_u64().unwrap_or(30) as usize;
    static LOGGED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r#""(?:name|tool)"\s*:\s*"[^"]*wiki_log""#).unwrap());
    let _ = std::fs::create_dir_all(&marks);
    if LOGGED.is_match(&transcript) {
        let _ = std::fs::write(&mark, "logged\n"); // nothing to ask, now or later
        return None;
    }
    if transcript.lines().count() < min_lines {
        return None;
    }
    std::fs::write(&mark, "asked\n").ok()?; // ask only if we can remember having asked
    Some(NUDGE.to_string())
}

pub fn run() -> ! {
    std::thread::spawn(|| {
        let mut sink = [0u8; 4096];
        let mut stdin = std::io::stdin();
        while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
    });
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(collect).ok().flatten();
        let _ = tx.send(r);
    });
    let context = rx.recv_timeout(Duration::from_secs(5)).ok().flatten().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| FALLBACK.to_string());
    let out = json!({ "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": context } });
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{out}");
    let _ = stdout.flush();
    std::process::exit(0)
}
