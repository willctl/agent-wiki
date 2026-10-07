//! Start at sign-in. The tray reports and logs a missing entry but never re-adds one by itself (that
//! is what malware does, and security tools treat it so); Repair and the installer do.
//!
//! - Windows: two independent per-user entries, neither needing admin: a Task Scheduler logon task
//!   (registered from the installer's task XML) and the HKCU Run value. Either one is enough; the
//!   later start exits (single instance). Two, because on 2026-10-02 the Run value alone vanished
//!   overnight on a managed PC.
//! - macOS: a LaunchAgent in ~/Library/LaunchAgents with RunAtLoad.
//! - Linux: an XDG autostart entry.

use crate::config::Config;
use std::path::{Path, PathBuf};

#[cfg(windows)]
pub const MENU: &str = "Start at sign-in";
#[cfg(target_os = "macos")]
pub const MENU: &str = "Start at login";
#[cfg(all(unix, not(target_os = "macos")))]
pub const MENU: &str = "Start at sign-in";

/// Each entry: "on", "missing", "disabled" (turned off by you) or "starts <other program>".
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub entries: Vec<(&'static str, String)>,
}

impl State {
    pub fn starts(&self) -> bool {
        self.entries.iter().any(|(_, v)| v == "on")
    }
    pub fn both(&self) -> bool {
        self.entries.iter().all(|(_, v)| v == "on")
    }
    /// What is off when only some entries are on (Windows has two).
    pub fn partial(&self) -> String {
        let get = |k: &str| self.entries.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap_or_default();
        if get("task") == "on" { format!("Run key entry {}", get("run")) } else { format!("logon task {}", get("task")) }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.entries.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(" "))
    }
}

pub fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

/// The program a command line starts: its first token, quoted or not.
pub fn command_exe(command: &str) -> String {
    let c = command.trim();
    if let Some(rest) = c.strip_prefix('"') {
        return rest.split('"').next().unwrap_or("").to_string();
    }
    c.split(' ').next().unwrap_or("").to_string()
}

fn same_file(a: &Path, b: &Path) -> bool {
    if cfg!(windows) { a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase() } else { a == b || std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok() }
}

/// "on" when a command line starts this program, else "starts <program>".
fn points(command: &str) -> String {
    let exe = command_exe(command);
    #[cfg(windows)]
    let exe = crate::winutil::expand_env(&exe);
    let full = std::path::absolute(&exe).unwrap_or_else(|_| PathBuf::from(&exe));
    if same_file(&full, &exe_path()) { "on".into() } else { format!("starts {}", full.display()) }
}

pub fn menu_lines(a: Option<&State>) -> Vec<(String, String)> {
    let get = |k: &str| a.map(|s| s.entries.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone()).unwrap_or_default()).unwrap_or_else(|| "checking...".into());
    #[cfg(windows)]
    return vec![
        (format!("Logon task (Task Scheduler): {}", get("task")), String::new()),
        (format!("Run key entry (HKCU): {}", get("run")), String::new()),
        ("Repair: register both again".into(), "repair-autostart".into()),
    ];
    #[cfg(target_os = "macos")]
    return vec![(format!("Login item (LaunchAgent): {}", get("launchagent")), String::new()), ("Repair: register it again".into(), "repair-autostart".into())];
    #[cfg(all(unix, not(target_os = "macos")))]
    return vec![(format!("Autostart entry: {}", get("autostart")), String::new()), ("Repair: register it again".into(), "repair-autostart".into())];
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
const STARTUP_APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

#[cfg(windows)]
fn schtasks(args: &[&str]) -> (i32, String) {
    use std::os::windows::process::CommandExt;
    let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    match std::process::Command::new(Path::new(&sys).join("System32").join("schtasks.exe")).args(args).creation_flags(0x0800_0000).stdin(std::process::Stdio::null()).output() {
        Ok(o) => (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
        Err(e) => (-1, e.to_string()),
    }
}

#[cfg(windows)]
fn check_run(cfg: &Config) -> String {
    let Some(v) = crate::winutil::hkcu_get_sz(&cfg.run_key, &cfg.run_value) else { return "missing".into() };
    // Task Manager > Startup apps keeps its on/off switch apart from the Run key: an odd first byte is off.
    if cfg.run_key.eq_ignore_ascii_case(crate::config::DEFAULT_RUN_KEY) && crate::winutil::hkcu_get_binary(STARTUP_APPROVED, &cfg.run_value).is_some_and(|b| b.first().is_some_and(|x| x & 1 == 1)) {
        return "disabled".into();
    }
    points(&v)
}

#[cfg(windows)]
fn xml_decode(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

#[cfg(windows)]
fn check_task(cfg: &Config) -> String {
    let (code, out) = schtasks(&["/query", "/tn", &cfg.task, "/xml", "ONE"]);
    if code != 0 {
        return "missing".into();
    }
    if let Some(s) = out.find("<Settings>").and_then(|a| out[a..].find("</Settings>").map(|b| &out[a..a + b]))
        && s.split("<Enabled>").nth(1).is_some_and(|e| e.trim_start().starts_with("false"))
    {
        return "disabled".into();
    }
    let Some(cmd) = out.split("<Command>").nth(1).and_then(|r| r.split("</Command>").next()) else { return "starts nothing".into() };
    points(&format!("\"{}\"", xml_decode(cmd).trim_matches('"')))
}

#[cfg(windows)]
pub fn check(cfg: &Config) -> Result<State, String> {
    Ok(State { entries: vec![("task", check_task(cfg)), ("run", check_run(cfg))] })
}

/// Registers both entries again for this program. Leaves a Run entry you turned off in Startup apps alone.
#[cfg(windows)]
pub fn repair(cfg: &Config) -> Result<String, String> {
    if !cfg.task_xml.exists() {
        return Err(format!("No task definition at {}; run the installer once.", cfg.task_xml.display()));
    }
    let xml = cfg.task_xml.to_string_lossy().into_owned();
    let (code, out) = schtasks(&["/create", "/tn", &cfg.task, "/xml", &xml, "/f"]);
    if code != 0 {
        return Err(format!("Task Scheduler refused the logon task: {}", crate::menu::clip(&out, 200)));
    }
    crate::winutil::hkcu_set_sz(&cfg.run_key, &cfg.run_value, &format!("\"{}\" --from run", exe_path().display()))?;
    let s = check(cfg)?;
    crate::log::tray(cfg, &format!("start at sign-in repaired: {s}"));
    if s.entries[1].1 == "disabled" {
        return Ok("Logon task registered. The Run key entry is turned off in Task Manager > Startup apps; turn it on there if you want both.".into());
    }
    Ok(format!("Agent Wiki starts at sign-in again ({s})."))
}

// ---------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
fn plist_path(cfg: &Config) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library").join("LaunchAgents").join(format!("{}.plist", label(cfg)))
}

#[cfg(target_os = "macos")]
fn label(cfg: &Config) -> String {
    if cfg.task == "AgentWikiTray" { "com.agentwiki.tray".into() } else { cfg.task.clone() }
}

#[cfg(target_os = "macos")]
pub fn check(cfg: &Config) -> Result<State, String> {
    let v = match std::fs::read_to_string(plist_path(cfg)) {
        Err(_) => "missing".to_string(),
        Ok(t) => {
            if t.split("<key>Disabled</key>").nth(1).is_some_and(|r| r.trim_start().starts_with("<true/>")) {
                "disabled".into()
            } else {
                let args = t.split("<key>ProgramArguments</key>").nth(1).unwrap_or("");
                let exe = args.split("<string>").nth(1).and_then(|r| r.split("</string>").next()).unwrap_or("");
                points(&format!("\"{exe}\""))
            }
        }
    };
    Ok(State { entries: vec![("launchagent", v)] })
}

#[cfg(target_os = "macos")]
pub fn repair(cfg: &Config) -> Result<String, String> {
    let file = plist_path(cfg);
    std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
    let exe = exe_path().display().to_string().replace('&', "&amp;").replace('<', "&lt;");
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{}</string>\n  <key>ProgramArguments</key>\n  <array><string>{exe}</string><string>--from</string><string>login</string></array>\n  <key>RunAtLoad</key><true/>\n  <key>ProcessType</key><string>Interactive</string>\n</dict>\n</plist>\n",
        label(cfg)
    );
    std::fs::write(&file, plist).map_err(|e| e.to_string())?;
    let s = check(cfg)?;
    crate::log::tray(cfg, &format!("start at login repaired: {s}"));
    Ok(format!("Agent Wiki starts at login again ({s})."))
}

// ---------------------------------------------------------------- Linux and other Unix

#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(".config"));
    base.join("autostart").join("agent-wiki-tray.desktop")
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn check(_cfg: &Config) -> Result<State, String> {
    let v = match std::fs::read_to_string(desktop_path()) {
        Err(_) => "missing".to_string(),
        Ok(t) => {
            let val = |k: &str| t.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::trim).unwrap_or("").to_string();
            if val("Hidden").eq_ignore_ascii_case("true") || val("X-GNOME-Autostart-enabled").eq_ignore_ascii_case("false") { "disabled".into() } else { points(&val("Exec")) }
        }
    };
    Ok(State { entries: vec![("autostart", v)] })
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn repair(cfg: &Config) -> Result<String, String> {
    let file = desktop_path();
    std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
    let exe = exe_path().display().to_string();
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=Agent Wiki\nComment=The shared AI memory: status, curator and window\nExec=\"{exe}\" --from autostart\nIcon=agent-wiki\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    std::fs::write(&file, text).map_err(|e| e.to_string())?;
    let s = check(cfg)?;
    crate::log::tray(cfg, &format!("start at sign-in repaired: {s}"));
    Ok(format!("Agent Wiki starts at sign-in again ({s})."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines() {
        assert_eq!(command_exe("\"C:\\A B\\x.exe\" --from run"), "C:\\A B\\x.exe");
        assert_eq!(command_exe("C:\\x.exe --y"), "C:\\x.exe");
        let s = State { entries: vec![("task", "on".into()), ("run", "missing".into())] };
        assert!(s.starts() && !s.both());
        assert_eq!(s.to_string(), "task:on run:missing");
        assert_eq!(s.partial(), "Run key entry missing");
    }
}
