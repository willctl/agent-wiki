//! Where the installer puts things: Agent Wiki's own standard folders (aw_core::paths) and the AI
//! apps' configuration files it edits. Computed from an environment map, so a sandboxed run (tests)
//! uses a fake home without touching the real one.

use aw_core::paths::{AppPaths, Env};
use std::path::{Path, PathBuf};

pub const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

pub struct Places {
    pub home: PathBuf,
    pub app: AppPaths,
    pub config: PathBuf,
    pub state: PathBuf,
    pub runtime: PathBuf,
    pub agent: PathBuf,
    pub tray_dir: PathBuf,
    pub tray_exe: PathBuf,
    pub tray_ini: PathBuf,
    pub icons: PathBuf,
    pub webview: PathBuf,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub task_xml: PathBuf,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub service_ini: PathBuf,
    pub market: PathBuf,
    pub rendered: PathBuf,
    pub paste: PathBuf,
    pub curator_codex_home: PathBuf,
    pub default_wiki: PathBuf,
    pub personal_market: PathBuf,
    pub codex_config: PathBuf,
    pub codex_agents: PathBuf,
    pub claude_md: PathBuf,
    pub claude_settings: PathBuf,
    /// What earlier versions installed and this one replaces (removed when found).
    pub legacy_tray_exe: PathBuf,
    pub legacy_tray_ini: PathBuf,
    pub legacy_ui_profile: PathBuf,
    env: Env,
}

impl Places {
    pub fn new(env: &Env) -> Places {
        // The installer always installs to the standard folders, whatever AGENT_WIKI_* overrides the
        // calling shell has (those are for the programs and the tests).
        let env: Env = env.iter().filter(|(k, _)| !k.starts_with("AGENT_WIKI_")).map(|(k, v)| (k.clone(), v.clone())).collect();
        let home = aw_core::paths::home_dir(&env);
        let app = AppPaths::for_platform(&env, std::env::consts::OS, &home);
        let data = app.data_dir.clone();
        let runtime = app.runtime_dir();
        let tray_dir = data.join("tray");
        let service_dir = data.join("service");
        let market = data.join("marketplace");
        let codex_home = env.get("CODEX_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
        let claude_dir = home.join(".claude");
        Places {
            config: app.config_file(),
            state: app.install_state(),
            agent: runtime.join(format!("agent-wiki{EXE}")),
            runtime,
            tray_exe: tray_dir.join(format!("agent-wiki-tray{EXE}")),
            tray_ini: tray_dir.join("agent-wiki-tray.ini"),
            icons: tray_dir.join("icons"),
            webview: tray_dir.join("webview"),
            task_xml: tray_dir.join("AgentWikiTray.task.xml"),
            legacy_tray_exe: tray_dir.join("AgentWikiTray.exe"),
            legacy_tray_ini: tray_dir.join("AgentWikiTray.ini"),
            tray_dir,
            service_ini: service_dir.join("AgentWikiService.ini"),
            rendered: market.join("plugins").join("agent-wiki"),
            market,
            paste: data.join("paste-into-app-settings.md"),
            curator_codex_home: app.curator_codex_home(),
            default_wiki: home.join("AgentWiki"),
            personal_market: home.join(".agents").join("plugins").join("marketplace.json"),
            codex_config: codex_home.join("config.toml"),
            codex_agents: codex_home.join("AGENTS.md"),
            claude_md: claude_dir.join("CLAUDE.md"),
            claude_settings: claude_dir.join("settings.json"),
            legacy_ui_profile: app.cache_dir.join("ui-profile"),
            home,
            app,
            env,
        }
    }

    pub fn var(&self, k: &str) -> Option<&str> {
        self.env.get(k).map(String::as_str).filter(|v| !v.is_empty())
    }

    /// claude_desktop_config.json files Claude desktop reads. Windows: the MSIX LocalCache copy (what
    /// Store installs read), else %APPDATA%\Claude. Only folders that exist (the app is installed).
    pub fn claude_desktop_configs(&self) -> Vec<PathBuf> {
        let mut found = vec![];
        if cfg!(windows) {
            let local = self.var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| self.home.join("AppData").join("Local"));
            if let Ok(rd) = std::fs::read_dir(local.join("Packages")) {
                let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.to_lowercase().starts_with("claude_")).collect();
                names.sort();
                for n in names {
                    let dir = local.join("Packages").join(n).join("LocalCache").join("Roaming").join("Claude");
                    if dir.is_dir() {
                        found.push(dir.join("claude_desktop_config.json"));
                    }
                }
            }
            let appdata = self.var("APPDATA").map(PathBuf::from).unwrap_or_else(|| self.home.join("AppData").join("Roaming")).join("Claude");
            if found.is_empty() && appdata.is_dir() {
                found.push(appdata.join("claude_desktop_config.json"));
            }
        } else {
            let dir = if cfg!(target_os = "macos") { self.home.join("Library").join("Application Support").join("Claude") } else { self.home.join(".config").join("Claude") };
            if dir.is_dir() {
                found.push(dir.join("claude_desktop_config.json"));
            }
        }
        found
    }
}

/// Forward slashes, as the JSON and Markdown the installer writes have always shown paths.
pub fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
