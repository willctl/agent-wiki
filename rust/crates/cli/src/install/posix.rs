//! macOS and Linux: the server and the curator as per-user services (launchd LaunchAgents, systemd
//! user units). Both run as the user, so the curator needs no tray to host it; both restart on
//! failure, and the server exits when an install replaces it (--exit-on-upgrade) to be restarted on
//! the new build.

use super::InstallError;
use super::places::Places;
use super::sys::run;
use std::path::{Path, PathBuf};

pub const SERVER_LABEL: &str = "com.agentwiki.server";
pub const CURATOR_LABEL: &str = "com.agentwiki.curator";
pub const SERVER_UNIT: &str = "agent-wiki.service";
pub const CURATOR_UNIT: &str = "agent-wiki-curator.service";

/// A path as one shell token: single quotes when needed.
pub fn shell_token(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "_./+:@%,=-".contains(c)) { s.into_owned() } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// PATH for the services: the usual places for node (an npm-installed codex is a node script) and codex.
fn service_path(p: &Places) -> String {
    let mut dirs: Vec<String> = vec![];
    if let Some(codex) = super::sys::which("codex").and_then(|c| c.parent().map(Path::to_path_buf)) {
        dirs.push(codex.to_string_lossy().into_owned());
    }
    if let Some(node) = super::sys::which("node").and_then(|c| c.parent().map(Path::to_path_buf)) {
        dirs.push(node.to_string_lossy().into_owned());
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(String::from));
    dirs.push(p.home.join(".local").join("bin").to_string_lossy().into_owned());
    dirs.dedup();
    dirs.join(":")
}

fn launch_agents(p: &Places) -> PathBuf {
    p.home.join("Library").join("LaunchAgents")
}

fn plist(label: &str, args: &[String], log: &Path, path: &str) -> String {
    let a: String = args.iter().map(|x| format!("<string>{}</string>", xml(x))).collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{label}</string>\n  <key>ProgramArguments</key>\n  <array>{a}</array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>5</integer>\n  <key>ProcessType</key><string>Background</string>\n  <key>EnvironmentVariables</key>\n  <dict><key>PATH</key><string>{}</string></dict>\n  <key>StandardErrorPath</key><string>{}</string>\n  <key>StandardOutPath</key><string>/dev/null</string>\n</dict>\n</plist>\n",
        xml(path),
        xml(&log.to_string_lossy())
    )
}

fn unit(description: &str, args: &[String], path: &str, restart_sec: u32) -> String {
    let exec: Vec<String> = args.iter().map(|a| if a.contains([' ', '"', '\\']) { format!("\"{}\"", a.replace('\\', "\\\\").replace('"', "\\\"")) } else { a.clone() }).collect();
    format!(
        "# Written by the agent-wiki installer; removed by agent-wiki uninstall.\n[Unit]\nDescription={description}\nAfter=default.target\n\n[Service]\nExecStart={}\nEnvironment=PATH={path}\nRestart=always\nRestartSec={restart_sec}\nNoNewPrivileges=yes\n\n[Install]\nWantedBy=default.target\n",
        exec.join(" ")
    )
}

fn uid() -> u32 {
    // SAFETY: getuid has no failure mode.
    unsafe { libc::getuid() }
}

/// Writes and (re)starts the server and curator services. Returns what it did, for the log.
pub fn install_services(p: &Places, port: u16, curator: bool) -> Result<Vec<String>, InstallError> {
    let agent = p.agent.to_string_lossy().into_owned();
    let server_args = vec![agent.clone(), "serve".into(), "--http".into(), "--port".into(), port.to_string(), "--exit-on-upgrade".into()];
    let curator_args = vec![agent, "curator".into()];
    let path = service_path(p);
    let mut done = vec![];
    let err = |e: std::io::Error| InstallError(e.to_string());
    std::fs::create_dir_all(&p.app.log_dir).map_err(err)?;
    if cfg!(target_os = "macos") {
        let dir = launch_agents(p);
        std::fs::create_dir_all(&dir).map_err(err)?;
        let mut jobs = vec![(SERVER_LABEL, server_args, p.app.log_dir.join("service.log"))];
        if curator {
            jobs.push((CURATOR_LABEL, curator_args, p.app.log_dir.join("curator.log")));
        }
        for (label, args, log) in jobs {
            let file = dir.join(format!("{label}.plist"));
            std::fs::write(&file, plist(label, &args, &log, &path)).map_err(err)?;
            let domain = format!("gui/{}", uid());
            let _ = run(Path::new("launchctl"), &["bootout", &domain, &file.to_string_lossy()]);
            let r = run(Path::new("launchctl"), &["bootstrap", &domain, &file.to_string_lossy()]);
            done.push(format!("{label}: {}", if r.ok { "loaded".to_string() } else { format!("launchctl bootstrap: {}", r.last_line()) }));
        }
    } else {
        let dir = p.app.config_dir.parent().map(|d| d.join("systemd").join("user")).unwrap_or_else(|| p.home.join(".config").join("systemd").join("user"));
        std::fs::create_dir_all(&dir).map_err(err)?;
        std::fs::write(dir.join(SERVER_UNIT), unit("Agent Wiki MCP server (127.0.0.1)", &server_args, &path, 2)).map_err(err)?;
        let mut units = vec![SERVER_UNIT];
        if curator {
            std::fs::write(dir.join(CURATOR_UNIT), unit("Agent Wiki curator", &curator_args, &path, 5)).map_err(err)?;
            units.push(CURATOR_UNIT);
        }
        let sc = Path::new("systemctl");
        let _ = run(sc, &["--user", "daemon-reload"]);
        for u in units {
            let r = run(sc, &["--user", "enable", u]);
            let s = run(sc, &["--user", "restart", u]);
            done.push(format!("{u}: {}", if r.ok && s.ok { "enabled and started".to_string() } else { format!("systemctl: {}", if s.ok { r.last_line() } else { s.last_line() }) }));
        }
    }
    Ok(done)
}

pub fn uninstall_services(p: &Places) -> Vec<String> {
    let mut done = vec![];
    if cfg!(target_os = "macos") {
        for label in [SERVER_LABEL, CURATOR_LABEL] {
            let file = launch_agents(p).join(format!("{label}.plist"));
            if file.exists() {
                let _ = run(Path::new("launchctl"), &["bootout", &format!("gui/{}", uid()), &file.to_string_lossy()]);
                let _ = std::fs::remove_file(&file);
                done.push(format!("removed {label}"));
            }
        }
    } else {
        let dir = p.app.config_dir.parent().map(|d| d.join("systemd").join("user")).unwrap_or_else(|| p.home.join(".config").join("systemd").join("user"));
        for u in [SERVER_UNIT, CURATOR_UNIT] {
            if dir.join(u).exists() {
                let _ = run(Path::new("systemctl"), &["--user", "disable", "--now", u]);
                let _ = std::fs::remove_file(dir.join(u));
                done.push(format!("removed {u}"));
            }
        }
        let _ = run(Path::new("systemctl"), &["--user", "daemon-reload"]);
    }
    done
}

/// Whether a graphical session is there to show a tray icon.
pub fn has_display() -> bool {
    cfg!(target_os = "macos") || std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}
