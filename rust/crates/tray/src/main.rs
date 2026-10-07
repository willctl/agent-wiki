//! agent-wiki-tray: the per-user tray (menu bar on macOS) app for Agent Wiki, on all three OSes.
//!
//! The service runs where it cannot show UI, so this app runs in the user's session, started at
//! sign-in (see autostart.rs). It:
//!  - polls the service's read-only GET /status on 127.0.0.1 and shows the state as its icon: healthy,
//!    degraded (backlog, failed notes, errors) or down;
//!  - hosts the curator (`agent-wiki curator --parent-stdin`) as the user, where configured (Windows;
//!    on macOS and Linux a launchd or systemd user unit runs it);
//!  - opens the Agent Wiki window on a left click: the web app the service serves at /ui/, in an
//!    embedded webview (WebView2, WKWebView, WebKitGTK);
//!  - offers a menu on a right click: open the window, status, open wiki/index/logs, recent activity,
//!    pause/resume the curator, curator sign-in and failed notes, start at sign-in, copy the MCP URL,
//!    restart the service, quit;
//!  - watches its own start-at-sign-in entries and logs (logs/tray.log) when one disappears.
//!
//!   agent-wiki-tray [--config <ini>] [--from <who>]   run (single instance; a second start opens the window)
//!   agent-wiki-tray --selftest                         fetch /status once, print the state and the menu as text
//!   agent-wiki-tray --do <action>                      run one menu action without the UI ("ok: ..." / "error: ...")
//!   agent-wiki-tray --quit                             ask the running instance to quit, and wait
//!   agent-wiki-tray --window                           only the window, until it is closed

#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod actions;
mod autostart;
mod config;
mod curator_host;
#[cfg(windows)]
mod keyhandoff;
mod log;
mod menu;
mod platform;
mod status;
#[cfg(windows)]
mod winutil;

use config::Config;
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let ini = opt("--config").map(std::path::PathBuf::from).unwrap_or_else(config::default_ini);
    let cfg = match Config::load(&ini) {
        Ok(c) => c,
        Err(e) => {
            if has("--quit") {
                std::process::exit(0); // nothing to stop without settings
            }
            eprintln!("Agent Wiki tray: cannot read {}: {e}", ini.display());
            std::process::exit(2);
        }
    };
    if has("--quit") {
        std::process::exit(platform::signal_quit(&cfg));
    }
    if has("--selftest") {
        std::process::exit(selftest(&cfg));
    }
    if let Some(action) = opt("--do") {
        let r = actions::run(&cfg, &action, &actions::Ctx { curator: None, open_ui: None });
        let mut out = std::io::stdout();
        let code = match r {
            Ok(m) => {
                let _ = writeln!(out, "ok: {}", m.unwrap_or_else(|| action.clone()));
                0
            }
            Err(e) => {
                let _ = writeln!(out, "error: {e}");
                1
            }
        };
        let _ = out.flush();
        std::process::exit(code);
    }
    if has("--window") {
        std::process::exit(platform::run_window(cfg));
    }
    let from = opt("--from").unwrap_or_else(|| "manual".into());
    std::process::exit(platform::run(cfg, &from));
}

fn selftest(cfg: &Config) -> i32 {
    let s = status::Status::fetch(cfg);
    let a = autostart::check(cfg).ok();
    let mut o = std::io::stdout();
    let mut lines = vec![
        format!("state={}", s.state()),
        format!("error={}", s.error.as_deref().unwrap_or("")),
        format!("tooltip={}", s.tooltip()),
        format!("icons={}", platform::icons_loaded(cfg)),
        format!("autostart={}", a.as_ref().map(|a| a.to_string()).unwrap_or_else(|| "unknown".into())),
        format!("ui={}", cfg.ui_url()),
        format!("window={}", platform::window_line(cfg)),
        format!("curator={}", if cfg.host_curator { cfg.agent.display().to_string() } else { "not hosted".into() }),
    ];
    lines.extend(menu::describe(cfg, &s, a.as_ref()).into_iter().map(|l| format!("item={l}")));
    for l in lines {
        let _ = writeln!(o, "{l}");
    }
    let _ = o.flush();
    0
}
