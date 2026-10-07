//! The tray menu as data: (submenu, text, action). The platform code turns it into a native menu, and
//! `--selftest` prints it as "Parent > Child" lines. Same items and wording on every OS.

use crate::autostart::State as Autostart;
use crate::config::Config;
use crate::status::{Status, uptime};

pub struct Item {
    /// "" = top level.
    pub sub: &'static str,
    /// "-" = a separator.
    pub text: String,
    /// "" = an information line (disabled).
    pub action: String,
}

fn item(sub: &'static str, text: impl Into<String>, action: impl Into<String>) -> Item {
    Item { sub, text: text.into(), action: action.into() }
}

/// One line, at most `n` characters, `&` doubled (menus treat a single one as an accelerator mark).
pub fn clip(s: &str, n: usize) -> String {
    let s = s.replace(['\r', '\n'], " ").replace('&', "&&");
    if s.chars().count() > n { format!("{}...", s.chars().take(n - 3).collect::<String>()) } else { s }
}

pub fn items(cfg: &Config, s: &Status, a: Option<&Autostart>) -> Vec<Item> {
    let mut m = vec![item("", "Open Agent Wiki", "open-ui"), item("", "-", "")];
    if !s.reachable() {
        m.push(item("", "Agent Wiki: service not responding", ""));
        m.push(item("", "Apps fall back to their own server until it is back", ""));
    } else {
        let ok = if s.state() == "healthy" { "OK" } else { "needs attention" };
        m.push(item("", format!("Agent Wiki {} · {ok}", s.str(&["version"]).unwrap_or_default()), ""));
        let dead = s.int(&["queue", "dead"]);
        m.push(item("", format!("Up {} · {} note(s) queued{}", uptime(s.int(&["uptimeSec"])), s.int(&["queue", "pending"]), if dead > 0 { format!(" · {dead} failed") } else { String::new() }), ""));
        if let Some(lw) = s.str(&["lastWrite", "at"]) {
            m.push(item("", format!("Last write {} · {}", clip(&lw.replace('T', " "), 16), clip(&s.str(&["lastWrite", "text"]).unwrap_or_default(), 60)), ""));
        }
        for r in s.reasons() {
            m.push(item("", format!("! {}", clip(&r, 80)), ""));
        }
    }
    if let Some(a) = a {
        if !a.starts() {
            m.push(item("", format!("! Will not start at sign-in: {} > Repair", crate::autostart::MENU), ""));
        } else if !a.both() {
            m.push(item("", format!("! {}: {}", crate::autostart::MENU, a.partial()), ""));
        }
    }
    m.push(item("", "-", ""));
    m.push(item("", "Open wiki folder", "open-wiki"));
    m.push(item("", "Open index.md", "open-index"));
    m.push(item("", "Open logs", "open-logs"));
    let recent = s.recent();
    if recent.is_empty() {
        m.push(item("Recent activity", "(nothing yet)", ""));
    }
    for h in recent {
        let date = h["date"].as_str().unwrap_or("").to_string();
        let text = format!("{} {} · {}", date.get(5..).unwrap_or(""), h["time"].as_str().unwrap_or(""), clip(h["text"].as_str().unwrap_or(""), 70));
        m.push(item("Recent activity", text, format!("open-log:{date}")));
    }
    m.push(item("", "-", ""));
    let paused = s.bool(&["curator", "paused"]) || cfg.paused_flag().exists();
    m.push(item("", if paused { "Resume curator" } else { "Pause curator" }, if paused { "resume" } else { "pause" }));
    let running = s.bool(&["curator", "running"]);
    let line = if !s.reachable() {
        "state unknown".to_string()
    } else if !running {
        (if cfg.host_curator { "not running" } else { "not hosted by this tray" }).to_string()
    } else {
        let model = s.str(&["curator", "model"]).map(|m| format!(" ({m}, {})", s.str(&["curator", "reasoningEffort"]).unwrap_or_default())).unwrap_or_default();
        format!("{}{model}", s.str(&["curator", "state"]).unwrap_or_default())
    };
    m.push(item("Curator", format!("Curator: {line}"), ""));
    if let Some(e) = s.str(&["curator", "lastError"]) {
        m.push(item("Curator", format!("Last error: {}", clip(&e, 80)), ""));
    }
    if let Some(model) = s.str(&["models", "curator", "model"]) {
        let ask = s.str(&["models", "ask", "model"]).unwrap_or_default();
        let effort = |r: &str| s.str(&["models", r, "reasoningEffort"]).unwrap_or_default();
        m.push(item("Curator", format!("Models: curator {model} ({}), Ask {ask} ({})", effort("curator"), effort("ask")), ""));
        m.push(item("Curator", "Change models in the window (Status > Models)...", "open-ui"));
    }
    m.push(item("Curator", "Sign in to ChatGPT for the curator...", "sign-in"));
    let dead = s.int(&["queue", "dead"]);
    m.push(item("Curator", format!("Retry failed notes ({dead})"), if dead > 0 { "retry-dead" } else { "" }));
    m.push(item("Curator", format!("File failed notes as sent, without the model ({dead})"), if dead > 0 { "file-raw" } else { "" }));
    m.push(item("Curator", "Restart the curator", if cfg.host_curator { "restart-curator" } else { "" }));
    m.push(item("Curator", "Open the curator log", "open-curator-log"));
    for (text, action) in crate::autostart::menu_lines(a) {
        m.push(item(crate::autostart::MENU, text, action));
    }
    m.push(item("", format!("Copy MCP URL ({})", cfg.mcp_url()), "copy-url"));
    m.push(item("", "Restart service", "restart-service"));
    m.push(item("", "-", ""));
    m.push(item("", "Quit", "quit"));
    m
}

/// The menu as lines of text ("Parent > Child"), for --selftest.
pub fn describe(cfg: &Config, s: &Status, a: Option<&Autostart>) -> Vec<String> {
    items(cfg, s, a).into_iter().map(|i| if i.sub.is_empty() { i.text } else { format!("{} > {}", i.sub, i.text) }).collect()
}
