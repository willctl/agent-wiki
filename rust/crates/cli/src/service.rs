//! `agent-wiki service`: the Windows service that serves Streamable HTTP on 127.0.0.1 for every app
//! (replaces the C# wrapper and its Node child). It runs the server in this process, logs to
//! logs\service.log, and when an install replaces this program it stops with an error code, so the
//! Service Control Manager's recovery action starts the new build (no admin rights needed to upgrade).
//!
//!   agent-wiki service --config <AgentWikiService.ini>             started by the SCM
//!   agent-wiki service --config <AgentWikiService.ini> --console   the same in the foreground (stops when stdin closes)
//!
//! The ini (written by the installer): wikiDir, port, logDir, and configDir/dataDir/stateDir/cacheDir
//! (this account's profile is not the user's, so each folder is named).

use crate::http::{self, Ended};
use aw_core::waker::Waker;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg_attr(not(windows), allow(dead_code))]
pub const SERVICE_NAME: &str = "AgentWiki";

fn read_ini(path: &Path) -> Result<HashMap<String, String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut d = HashMap::new();
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
    for k in ["wikidir", "port", "logdir"] {
        if !d.contains_key(k) {
            return Err(format!("{}: missing key '{k}'", path.display()));
        }
    }
    Ok(d)
}

struct Settings {
    port: u16,
    log_dir: PathBuf,
    /// The installing user's SID: who may hand the service the embeddings key (keypipe.rs).
    #[cfg_attr(not(windows), allow(dead_code))]
    owner: Option<String>,
}

/// Applies the ini as environment for this process. Called before any thread starts.
fn apply(ini: &HashMap<String, String>) -> Result<Settings, String> {
    let set = |k: &str, v: &str| {
        // SAFETY: called at startup, before any other thread exists.
        unsafe { std::env::set_var(k, v) };
    };
    set("AGENT_WIKI_DIR", &ini["wikidir"]);
    set("AGENT_WIKI_LOG_DIR", &ini["logdir"]);
    for (key, var) in
        [("configdir", "AGENT_WIKI_CONFIG_DIR"), ("datadir", "AGENT_WIKI_DATA_DIR"), ("statedir", "AGENT_WIKI_STATE_DIR"), ("cachedir", "AGENT_WIKI_CACHE_DIR"), ("home", "AGENT_WIKI_HOME")]
    {
        if let Some(v) = ini.get(key) {
            set(var, v);
        }
    }
    if std::env::var("AGENT_WIKI_PROCESS").map(|s| s.is_empty()).unwrap_or(true) {
        set("AGENT_WIKI_PROCESS", "service");
    }
    let port = ini["port"].parse::<u16>().map_err(|_| format!("port: not a port number: {}", ini["port"]))?;
    Ok(Settings { port, log_dir: PathBuf::from(&ini["logdir"]), owner: ini.get("owner").cloned() })
}

/// logs\service.log, rotated at 5 MB: this process's stderr from now on.
fn open_log(dir: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(dir).ok()?;
    let file = dir.join("service.log");
    if std::fs::metadata(&file).is_ok_and(|m| m.len() > 5 * 1024 * 1024) {
        let old = dir.join("service.log.1");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&file, &old);
    }
    std::fs::OpenOptions::new().create(true).append(true).open(file).ok()
}

/// A [service] line in the log (stderr, which is logs\service.log under the SCM).
fn note(msg: &str) {
    eprintln!("{} [service] {msg}", aw_core::text::local_iso_ms(&aw_core::text::now()));
}

/// Serves until stopped; the exit code tells the SCM whether to restart (an upgrade).
fn host(settings: &Settings, stop: Arc<Waker>) -> i32 {
    note(&format!("started pid {} (v{}): {}", std::process::id(), aw_core::VERSION, std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default()));
    #[cfg(windows)]
    if let Some(owner) = settings.owner.clone() {
        crate::keypipe::serve(settings.port, owner);
    }
    let code = match http::serve(settings.port, stop, true) {
        Ok(Ended::Stopped) => 0,
        Ok(Ended::Upgraded) => {
            note("a new build was installed; stopping so the service restarts on it");
            http::UPGRADED_EXIT
        }
        Err(e) => {
            note(&format!("cannot serve: {e}"));
            1
        }
    };
    note("stopped");
    code
}

pub fn run(args: &[String]) -> ! {
    let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let ini_path = opt("--config").map(PathBuf::from).unwrap_or_else(|| std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("AgentWikiService.ini"))).unwrap_or_default());
    let settings = match read_ini(&ini_path).and_then(|ini| apply(&ini)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("agent-wiki service: {e}");
            std::process::exit(2)
        }
    };
    if args.iter().any(|a| a == "--console") {
        let stop = Waker::new();
        {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut sink = [0u8; 4096];
                let mut stdin = std::io::stdin();
                while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
                stop.stop();
            });
        }
        std::process::exit(host(&settings, stop));
    }
    redirect_stderr(open_log(&settings.log_dir).as_ref());
    scm::run(settings)
}

#[cfg(windows)]
fn redirect_stderr(file: Option<&std::fs::File>) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    let Some(f) = file.and_then(|f| f.try_clone().ok()) else { return };
    let h = f.as_raw_handle();
    std::mem::forget(f); // stays open for the life of the process
    // SAFETY: a valid handle we own, kept open.
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, h as _);
        SetStdHandle(STD_OUTPUT_HANDLE, h as _);
    }
}

#[cfg(not(windows))]
fn redirect_stderr(_: Option<&std::fs::File>) {}

/// The Service Control Manager protocol.
#[cfg(windows)]
mod scm {
    use super::{Settings, host, note};
    use aw_core::waker::Waker;
    use std::sync::{Arc, Mutex, OnceLock};
    use windows_sys::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR};
    use windows_sys::Win32::System::Services::*;

    struct Shared {
        settings: Settings,
        stop: Arc<Waker>,
        handle: Mutex<SERVICE_STATUS_HANDLE>,
    }
    // SAFETY: the status handle is an opaque token the SCM accepts from any thread.
    unsafe impl Send for Shared {}
    unsafe impl Sync for Shared {}

    static SHARED: OnceLock<Shared> = OnceLock::new();

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn report(state: SERVICE_STATUS_CURRENT_STATE, exit: i32, wait_ms: u32) {
        let Some(sh) = SHARED.get() else { return };
        let handle = *sh.handle.lock().unwrap();
        if handle.is_null() {
            return;
        }
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
            dwWin32ExitCode: if exit == 0 { NO_ERROR } else { ERROR_SERVICE_SPECIFIC_ERROR },
            dwServiceSpecificExitCode: exit as u32,
            dwCheckPoint: 0,
            dwWaitHint: wait_ms,
        };
        // SAFETY: a valid status handle and a fully initialized struct.
        unsafe { SetServiceStatus(handle, &status) };
    }

    unsafe extern "system" fn control(code: u32, _kind: u32, _data: *mut core::ffi::c_void, _ctx: *mut core::ffi::c_void) -> u32 {
        match code {
            SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
                report(SERVICE_STOP_PENDING, 0, 10_000);
                if let Some(sh) = SHARED.get() {
                    note(if code == SERVICE_CONTROL_STOP { "stop requested" } else { "system shutting down" });
                    sh.stop.stop();
                }
                NO_ERROR
            }
            SERVICE_CONTROL_INTERROGATE => NO_ERROR,
            _ => ERROR_CALL_NOT_IMPLEMENTED,
        }
    }

    unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
        let Some(sh) = SHARED.get() else { return };
        let name = wide(super::SERVICE_NAME);
        // SAFETY: a NUL-terminated name and a handler with the right signature.
        let handle = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), std::ptr::null_mut()) };
        if handle.is_null() {
            note("cannot register with the Service Control Manager");
            return;
        }
        *sh.handle.lock().unwrap() = handle;
        report(SERVICE_START_PENDING, 0, 5_000);
        let stop = sh.stop.clone();
        let worker = std::thread::spawn(move || host(&SHARED.get().unwrap().settings, stop));
        report(SERVICE_RUNNING, 0, 0);
        let code = worker.join().unwrap_or(1);
        report(SERVICE_STOPPED, code, 0);
    }

    pub fn run(settings: Settings) -> ! {
        let _ = SHARED.set(Shared { settings, stop: Waker::new(), handle: Mutex::new(std::ptr::null_mut()) });
        let mut name = wide(super::SERVICE_NAME);
        let table = [SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) }, SERVICE_TABLE_ENTRYW { lpServiceName: std::ptr::null_mut(), lpServiceProc: None }];
        // SAFETY: a NULL-terminated table whose strings outlive the call (it returns when the service stops).
        let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
        if ok == 0 {
            eprintln!("agent-wiki service: not started by the Service Control Manager (use --console to run it here)");
            std::process::exit(2);
        }
        std::process::exit(0)
    }
}

#[cfg(not(windows))]
mod scm {
    use super::Settings;
    pub fn run(_: Settings) -> ! {
        eprintln!("agent-wiki service: Windows only; elsewhere run `agent-wiki serve --http --exit-on-upgrade` from launchd or systemd");
        std::process::exit(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_needs_the_basics() {
        let dir = std::env::temp_dir().join(format!("aw-svc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.ini");
        std::fs::write(&f, "\u{feff}# comment\nwikiDir=C:/w\nPORT = 47821\nlogDir=C:/l\nconfigDir=C:/c\n").unwrap();
        let d = read_ini(&f).unwrap();
        assert_eq!(d["port"], "47821");
        assert_eq!(d["configdir"], "C:/c");
        std::fs::write(&f, "wikiDir=x\n").unwrap();
        assert!(read_ini(&f).unwrap_err().contains("missing key 'port'"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
