//! What each menu item does, shared by the tray (clicks) and `--do <action>` (scripts and tests).
//! Returns a message for the user (or None), or what went wrong. Slow actions block.

use crate::config::Config;
use crate::curator_host::CuratorHost;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Ctx<'a> {
    pub curator: Option<Arc<CuratorHost>>,
    /// Opens (or brings up) the window, when a running tray handles the action itself.
    pub open_ui: Option<&'a dyn Fn() -> Result<Option<String>, String>>,
}

/// Actions that may take seconds: run off the UI thread.
pub const SLOW: [&str; 4] = ["restart-service", "retry-dead", "file-raw", "repair-autostart"];

pub fn run(cfg: &Config, action: &str, ctx: &Ctx<'_>) -> Result<Option<String>, String> {
    match action {
        "open-wiki" => open_folder(&cfg.wiki_dir).map(|_| None),
        "open-index" => open_file(&cfg.wiki_dir.join("index.md")).map(|_| None),
        "open-logs" => open_folder(&cfg.log_dir).map(|_| None),
        "open-curator-log" => open_file(&cfg.log_dir.join("curator.log")).map(|_| None),
        a if a.starts_with("open-log:") => {
            let date = &a[9..];
            if !aw_core::wiki::DATE_RE.is_match(date) {
                return Err(format!("Not a log day: {date}"));
            }
            open_file(&cfg.wiki_dir.join("log").join(&date[..4]).join(format!("{date}.md"))).map(|_| None)
        }
        "pause" => {
            let flag = cfg.paused_flag();
            std::fs::create_dir_all(flag.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::write(&flag, format!("paused from the tray at {}\n", aw_core::text::local_iso(&aw_core::text::now()))).map_err(|e| e.to_string())?;
            Ok(Some("Curator paused. Notes keep being saved and wait in the inbox.".into()))
        }
        "resume" => {
            let _ = std::fs::remove_file(cfg.paused_flag());
            Ok(Some("Curator resumed.".into()))
        }
        "copy-url" => {
            set_clipboard(&cfg.mcp_url())?;
            Ok(Some(format!("Copied {}", cfg.mcp_url())))
        }
        "sign-in" => sign_in(cfg, ctx.curator.clone()).map(|_| None),
        "retry-dead" => run_curator(cfg, "--retry-dead", "Failed notes queued for another try."),
        "file-raw" => run_curator(cfg, "--file-raw-dead", "Failed notes filed into the log as they were sent."),
        "restart-curator" => {
            ctx.curator.as_ref().ok_or("This tray does not host the curator.")?.restart();
            Ok(Some("Curator restarted.".into()))
        }
        "restart-service" => restart_service(cfg),
        "repair-autostart" => crate::autostart::repair(cfg).map(Some),
        "open-ui" => match ctx.open_ui {
            Some(open) => open(),
            None => crate::platform::request_open(cfg).map(|_| None),
        },
        other => Err(format!("Unknown action: {other}")),
    }
}

/// `agent-wiki curator <arg>`, waiting up to 2 minutes.
fn run_curator(cfg: &Config, arg: &str, done: &str) -> Result<Option<String>, String> {
    let mut cmd = Command::new(&cfg.agent);
    cmd.args(["curator", arg]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("Failed: cannot start {}: {e}", cfg.agent.display()))?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("Failed: {}", crate::menu::clip(&String::from_utf8_lossy(&out.stderr), 200)));
    }
    Ok(Some(done.into()))
}

fn hide(_cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        _cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
fn open_folder(folder: &Path) -> Result<(), String> {
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    Command::new("explorer.exe").arg(folder).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn open_file(file: &Path) -> Result<(), String> {
    if !file.exists() {
        return Err(format!("Not found: {}", file.display()));
    }
    // With no app registered for the extension (.md often has none), the shell would show the
    // "How do you want to open this file?" picker; Notepad is the better default.
    let ext = file.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    if crate::winutil::has_association(&ext) && crate::winutil::shell_open(&file.to_string_lossy()).is_ok() {
        return Ok(());
    }
    Command::new("notepad.exe").arg(file).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn set_clipboard(text: &str) -> Result<(), String> {
    crate::winutil::set_clipboard(std::ptr::null_mut(), text)
}

#[cfg(windows)]
fn sign_in(cfg: &Config, curator: Option<Arc<CuratorHost>>) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let (Some(codex), Some(home)) = (&cfg.codex, &cfg.codex_home) else {
        return Err("The Codex CLI was not found when Agent Wiki was installed.".into());
    };
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let line = format!(
        "/k title Agent Wiki curator: sign in to ChatGPT && echo This signs the Agent Wiki curator in to ChatGPT (its own Codex home: %CODEX_HOME%). && echo. && \"{codex}\" login && echo. && echo Done. You can close this window."
    );
    let mut child = Command::new("cmd.exe")
        .raw_arg(line)
        .env("CODEX_HOME", home)
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .creation_flags(0x0000_0010) // CREATE_NEW_CONSOLE
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(c) = curator {
        // Pick up the new sign-in at once.
        std::thread::spawn(move || {
            let _ = child.wait();
            c.restart();
        });
    }
    Ok(())
}

#[cfg(windows)]
fn restart_service(cfg: &Config) -> Result<Option<String>, String> {
    use crate::winutil::wide;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SERVICE_DOES_NOT_EXIST, GetLastError};
    use windows_sys::Win32::System::Services::*;
    struct Handle(SC_HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: a handle we opened.
            unsafe { CloseServiceHandle(self.0) };
        }
    }
    let denied = || "Your account may not restart the service yet. Run the Agent Wiki installer once in an elevated terminal (it grants only start and stop of this service).".to_string();
    // SAFETY: plain SCM calls with valid strings; every handle is closed by Handle.
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err(format!("Cannot reach the Service Control Manager (error {}).", GetLastError()));
        }
        let scm = Handle(scm);
        let name = wide(&cfg.service);
        let svc = OpenServiceW(scm.0, name.as_ptr(), SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP);
        if svc.is_null() {
            return Err(match GetLastError() {
                ERROR_ACCESS_DENIED => denied(),
                ERROR_SERVICE_DOES_NOT_EXIST => format!("The service {} is not installed.", cfg.service),
                e => format!("Cannot open the service {} (error {e}).", cfg.service),
            });
        }
        let svc = Handle(svc);
        let state = || {
            let mut st: SERVICE_STATUS = std::mem::zeroed();
            if QueryServiceStatus(svc.0, &mut st) == 0 { 0 } else { st.dwCurrentState }
        };
        let wait_for = |want: u32| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while state() != want && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(200));
            }
            state() == want
        };
        if state() != SERVICE_STOPPED {
            let mut st: SERVICE_STATUS = std::mem::zeroed();
            if ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) == 0 && GetLastError() == ERROR_ACCESS_DENIED {
                return Err(denied());
            }
            wait_for(SERVICE_STOPPED);
        }
        if StartServiceW(svc.0, 0, std::ptr::null()) == 0 {
            let e = GetLastError();
            if e == ERROR_ACCESS_DENIED {
                return Err(denied());
            }
            if e != 1056 {
                // 1056: already running (its recovery action restarted it)
                return Err(format!("The service did not start (error {e}); see logs\\service.log."));
            }
        }
        if !wait_for(SERVICE_RUNNING) {
            return Err("The service did not restart within 30 s; see logs\\service.log.".into());
        }
    }
    Ok(Some("The service restarted.".into()))
}

// ---------------------------------------------------------------- macOS and Linux

#[cfg(not(windows))]
fn opener() -> &'static str {
    if cfg!(target_os = "macos") { "open" } else { "xdg-open" }
}

#[cfg(not(windows))]
fn open_folder(folder: &Path) -> Result<(), String> {
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    Command::new(opener()).arg(folder).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn open_file(file: &Path) -> Result<(), String> {
    if !file.exists() {
        return Err(format!("Not found: {}", file.display()));
    }
    // macOS: -t opens Markdown and logs in the default text editor even when no app claims them.
    let mut cmd = Command::new(opener());
    if cfg!(target_os = "macos") {
        cmd.arg("-t");
    }
    cmd.arg(file).spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn set_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    let candidates: &[(&str, &[&str])] =
        if cfg!(target_os = "macos") { &[("pbcopy", &[])] } else { &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])] };
    for (cmd, args) in candidates {
        if let Ok(mut c) = Command::new(cmd).args(*args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(text.as_bytes());
            }
            if c.wait().is_ok_and(|s| s.success()) {
                return Ok(());
            }
        }
    }
    Err("No clipboard tool found (install wl-clipboard or xclip).".into())
}

#[cfg(not(windows))]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(not(windows))]
fn sign_in(cfg: &Config, curator: Option<Arc<CuratorHost>>) -> Result<(), String> {
    let (Some(codex), Some(home)) = (&cfg.codex, &cfg.codex_home) else {
        return Err("The Codex CLI was not found when Agent Wiki was installed.".into());
    };
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let script = format!(
        "echo 'This signs the Agent Wiki curator in to ChatGPT (its own Codex home).'; env -u CODEX_API_KEY -u OPENAI_API_KEY CODEX_HOME={} {} login; echo; echo 'Done. You can close this window.'",
        shell_quote(&home.to_string_lossy()),
        shell_quote(codex)
    );
    let r = if cfg!(target_os = "macos") {
        let apple = format!("tell application \"Terminal\" to do script \"{}\"", script.replace('\\', "\\\\").replace('"', "\\\""));
        Command::new("osascript").args(["-e", &apple, "-e", "tell application \"Terminal\" to activate"]).spawn()
    } else {
        Command::new("x-terminal-emulator").args(["-e", "sh", "-c", &format!("{script}; exec sh")]).spawn()
    };
    r.map_err(|e| format!("Cannot open a terminal: {e}"))?;
    if let Some(c) = curator {
        c.restart();
    }
    Ok(())
}

#[cfg(not(windows))]
fn restart_service(cfg: &Config) -> Result<Option<String>, String> {
    let out = if cfg!(target_os = "macos") {
        // SAFETY: getuid has no failure mode.
        let uid = unsafe { libc::getuid() };
        Command::new("launchctl").args(["kickstart", "-k", &format!("gui/{uid}/{}", cfg.service)]).output()
    } else {
        Command::new("systemctl").args(["--user", "restart", &cfg.service]).output()
    }
    .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("The service did not restart: {}", crate::menu::clip(&String::from_utf8_lossy(&out.stderr), 200)));
    }
    Ok(Some("The service restarted.".into()))
}
