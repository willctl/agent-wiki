//! The tray's settings: agent-wiki-tray.ini next to the program (written by the installer), or
//! `--config <file>`. Keys are case-insensitive; `#` and `;` start comments.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Config {
    /// The agent-wiki program (the curator and its commands); default: next to this one.
    pub agent: PathBuf,
    pub wiki_dir: PathBuf,
    pub log_dir: PathBuf,
    pub icon_dir: PathBuf,
    /// The window's own WebView2 / WebKit data folder.
    pub webview_dir: PathBuf,
    /// What the tray announced (status::Announced), next to the ini unless the ini says statefile=.
    pub state_file: PathBuf,
    pub port: u16,
    pub codex: Option<String>,
    pub codex_home: Option<PathBuf>,
    /// Windows service name; on macOS the launchd label; on Linux the systemd user unit.
    pub service: String,
    /// Names the single-instance lock (tests use their own).
    pub instance: String,
    pub host_curator: bool,
    pub show_icon: bool,
    /// Open the window on request (tests turn it off and check the log instead).
    pub window: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub run_key: String,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub run_value: String,
    #[cfg_attr(all(unix, not(target_os = "macos")), allow(dead_code))]
    pub task: String,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub task_xml: PathBuf,
    pub autostart_check_seconds: u64,
    pub file: PathBuf,
}

pub const DEFAULT_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

impl Config {
    pub fn status_url_path(&self) -> &'static str {
        "/status"
    }
    pub fn mcp_url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
    pub fn ui_url(&self) -> String {
        format!("http://127.0.0.1:{}/ui/", self.port)
    }
    pub fn paused_flag(&self) -> PathBuf {
        self.wiki_dir.join(".curator").join("paused")
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut d: HashMap<String, String> = HashMap::new();
        for raw in text.trim_start_matches('\u{feff}').lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=')
                && !k.trim().is_empty()
            {
                d.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        for k in ["wikidir", "logdir", "icons", "port"] {
            if !d.contains_key(k) {
                return Err(format!("missing key '{k}'"));
            }
        }
        let dir = std::fs::canonicalize(path).ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from("."));
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| dir.clone());
        let agent = d.get("agent").map(PathBuf::from).unwrap_or_else(|| exe_dir.join(if cfg!(windows) { "agent-wiki.exe" } else { "agent-wiki" }));
        let on = |k: &str, default: bool| d.get(k).map(|v| v != "0" && !v.eq_ignore_ascii_case("off") && !v.eq_ignore_ascii_case("false")).unwrap_or(default);
        let log_dir = PathBuf::from(&d["logdir"]);
        let paths = aw_core::paths::AppPaths::current();
        Ok(Config {
            agent,
            wiki_dir: PathBuf::from(&d["wikidir"]),
            icon_dir: PathBuf::from(&d["icons"]),
            webview_dir: d.get("webviewdir").map(PathBuf::from).unwrap_or_else(|| paths.data_dir.join("tray").join("webview")),
            state_file: d.get("statefile").map(PathBuf::from).unwrap_or_else(|| dir.join("tray-state.json")),
            port: d["port"].parse().map_err(|_| format!("port: not a port number: {}", d["port"]))?,
            codex: d.get("codex").cloned().filter(|s| !s.is_empty()),
            codex_home: d.get("codexhome").map(PathBuf::from),
            service: d.get("service").cloned().unwrap_or_else(|| default_service().into()),
            instance: d.get("instance").cloned().unwrap_or_else(|| "AgentWikiTray".into()),
            host_curator: on("curator", cfg!(windows)),
            show_icon: on("icon", true),
            window: on("window", true),
            run_key: d.get("runkey").cloned().unwrap_or_else(|| DEFAULT_RUN_KEY.into()),
            run_value: d.get("runvalue").cloned().unwrap_or_else(|| "AgentWikiTray".into()),
            task: d.get("task").cloned().unwrap_or_else(|| "AgentWikiTray".into()),
            task_xml: d.get("taskxml").map(PathBuf::from).unwrap_or_else(|| dir.join("AgentWikiTray.task.xml")),
            autostart_check_seconds: d.get("autostartcheckseconds").and_then(|v| v.parse::<u64>().ok()).unwrap_or(120).max(1),
            log_dir,
            file: path.to_path_buf(),
        })
    }
}

fn default_service() -> &'static str {
    if cfg!(windows) {
        "AgentWiki"
    } else if cfg!(target_os = "macos") {
        "com.agentwiki.server"
    } else {
        "agent-wiki.service"
    }
}

/// agent-wiki-tray.ini next to this program.
pub fn default_ini() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("agent-wiki-tray.ini"))).unwrap_or_else(|| PathBuf::from("agent-wiki-tray.ini"))
}
