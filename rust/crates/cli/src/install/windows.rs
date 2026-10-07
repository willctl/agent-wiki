//! Windows: the service (sc.exe), its folder grants and the tray's start/stop right, elevation, the
//! logon task, the registry, and app-package detection.

use super::InstallError;
use super::places::fwd;
use super::sys::{Ran, run, run_in};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

pub const SERVICE_NAME: &str = "AgentWiki";
pub const SERVICE_ACCOUNT: &str = "NT SERVICE\\AgentWiki";
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
pub const TRAY_RUN_VALUE: &str = "AgentWikiTray";
pub const TRAY_TASK: &str = "AgentWikiTray";
/// SERVICE_START (RP) + SERVICE_STOP (WP) + SERVICE_QUERY_STATUS (LC): what the tray's "Restart
/// service" needs, and nothing that changes, reconfigures or deletes the service or its permissions.
pub const TRAY_SERVICE_RIGHTS: &str = "RPWPLC";
pub const PIN_TRAY_ICON: &str = "Settings > Personalization > Taskbar > Other system tray icons > Agent Wiki: On";

fn system32(exe: &str) -> PathBuf {
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join("System32").join(exe)
}

/// The app package whose AppData virtualization captures this process's writes, or None. Inside an
/// MSIX app (Claude desktop's Code tab, Store PowerShell, ChatGPT desktop) files created under AppData
/// land in %LOCALAPPDATA%\Packages\<package>\LocalCache instead, invisible to the service, the tray at
/// sign-in and every other app. A probe file shows it directly.
pub fn packaged() -> Option<String> {
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    let name = format!("agent-wiki-probe-{}-{}", std::process::id(), aw_core::text::now_ms());
    let probe = local.join(&name);
    std::fs::write(&probe, "").ok()?;
    let found = std::fs::read_dir(local.join("Packages"))
        .ok()
        .and_then(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).find(|pkg| local.join("Packages").join(pkg).join("LocalCache").join("Local").join(&name).exists()));
    let _ = std::fs::remove_file(&probe);
    found
}

/// The SID of the account running this process (S-1-5-21-... or, for Entra ID accounts, S-1-12-1-...).
pub fn user_sid() -> Option<String> {
    let r = run(&system32("whoami.exe"), &["/user", "/fo", "csv", "/nh"]);
    static SID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""(S-1-[0-9-]+)""#).unwrap());
    SID.captures(&r.stdout).map(|c| c[1].to_string())
}

/// True when this process runs elevated (High or System integrity level).
pub fn is_elevated() -> bool {
    let r = run(&system32("whoami.exe"), &["/groups"]);
    r.stdout.contains("S-1-16-12288") || r.stdout.contains("S-1-16-16384")
}

pub fn sc(args: &[&str]) -> Ran {
    run(&system32("sc.exe"), args)
}

pub fn sc_ok(args: &[&str]) -> Result<String, InstallError> {
    let r = sc(args);
    if !r.ok {
        return Err(InstallError(format!("sc.exe {} failed (exit {:?}):\n{}{}", args.join(" "), r.code, r.stdout, r.stderr)));
    }
    Ok(r.stdout)
}

/// None (not installed) or a state such as RUNNING. Works without elevation.
pub fn service_state() -> Option<String> {
    let r = sc(&["query", SERVICE_NAME]);
    if !r.ok {
        return None;
    }
    static STATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"STATE\s*:\s*[0-9]+\s+(\w+)").unwrap());
    Some(STATE.captures(&r.stdout).map(|c| c[1].to_string()).unwrap_or_else(|| "UNKNOWN".into()))
}

/// The program the service starts (BINARY_PATH_NAME, without quotes and arguments), or None.
pub fn service_bin() -> Option<PathBuf> {
    let r = sc(&["qc", SERVICE_NAME]);
    static BIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)BINARY_PATH_NAME\s*:\s*(.+)$").unwrap());
    let v = BIN.captures(&r.stdout)?[1].trim().to_string();
    Some(PathBuf::from(if let Some(rest) = v.strip_prefix('"') { rest.split('"').next().unwrap_or("").to_string() } else { v.split_whitespace().next().unwrap_or("").to_string() }))
}

pub fn wait_state(want: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if service_state().as_deref() == Some(want) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Grants the service's account `perm` (RX or M) on `target`, inherited below it. Your own folders need no admin.
pub fn grant(target: &Path, perm: &str) -> Result<String, InstallError> {
    let t = target.to_string_lossy();
    let r = run(&system32("icacls.exe"), &[&t, "/grant", &format!("{SERVICE_ACCOUNT}:(OI)(CI){perm}"), "/Q"]);
    if !r.ok {
        return Err(InstallError(format!("icacls {t} failed:\n{}{}", r.stdout, r.stderr)));
    }
    Ok(format!("{SERVICE_ACCOUNT} {} on {}", if perm == "M" { "Modify" } else { "Read" }, fwd(target)))
}

pub fn ungrant(target: &Path) -> Ran {
    run(&system32("icacls.exe"), &[&target.to_string_lossy(), "/remove:g", SERVICE_ACCOUNT, "/Q"])
}

fn sid_ace_re(sid: &str) -> Regex {
    Regex::new(&format!(r"\(A;;([A-Z]*);;;{}\)", regex::escape(sid))).unwrap()
}

/// Windows stores rights in its own order (RPWPLC comes back as LCRPWP): compare as sets of codes.
fn same_rights(a: &str, b: &str) -> bool {
    let set = |s: &str| {
        let mut v: Vec<String> = s.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        v.sort();
        v
    };
    set(a) == set(b)
}

/// Adds (or refreshes) the start/stop ACE for `sid` in a service's SDDL DACL: the new SDDL.
pub fn sddl_with_start_stop(sddl: &str, sid: &str) -> Result<String, InstallError> {
    static SID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^S-1-[0-9-]+$").unwrap());
    if !SID_RE.is_match(sid) {
        return Err(InstallError(format!("not a SID: {sid}")));
    }
    let s = sddl.trim();
    // The SACL ("S:...") follows the DACL ("D:..."); find it outside the parenthesized ACEs.
    let mut depth = 0i32;
    let mut sacl = s.len();
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            'S' if depth == 0 && s[i..].starts_with("S:") => {
                sacl = i;
                break;
            }
            _ => {}
        }
    }
    let d = s.find("D:").filter(|d| *d < sacl).ok_or_else(|| InstallError(format!("unexpected service security descriptor: {sddl}")))?;
    let _ = d;
    let re = sid_ace_re(sid);
    let mine: Vec<_> = re.captures_iter(&s[..sacl]).collect();
    if mine.len() == 1 && same_rights(&mine[0][1], TRAY_SERVICE_RIGHTS) {
        return Ok(s.to_string());
    }
    Ok(format!("{}(A;;{TRAY_SERVICE_RIGHTS};;;{sid}){}", re.replace_all(&s[..sacl], ""), &s[sacl..]))
}

/// Runs this program elevated with `args` (one UAC prompt) and waits: its exit code, or why not.
pub fn run_elevated(args: &[String]) -> Result<u32, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    let exe: Vec<u16> = std::env::current_exe().map_err(|e| e.to_string())?.as_os_str().encode_wide().chain([0]).collect();
    let quote = |a: &String| if a.contains([' ', '"']) || a.is_empty() { format!("\"{}\"", a.replace('"', "\\\"")) } else { a.clone() };
    let params: Vec<u16> = args.iter().map(quote).collect::<Vec<_>>().join(" ").encode_utf16().chain([0]).collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain([0]).collect();
    // SAFETY: a fully initialized struct whose strings outlive the call; the process handle is closed below.
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = 0; // hidden: its output goes to a log file the caller shows
        if ShellExecuteExW(&mut info) == 0 {
            let e = windows_sys::Win32::Foundation::GetLastError();
            return Err(if e == 1223 { "the UAC prompt was declined".into() } else { format!("not elevated (error {e})") });
        }
        if WaitForSingleObject(info.hProcess, INFINITE) != WAIT_OBJECT_0 {
            CloseHandle(info.hProcess);
            return Err("lost track of the elevated process".into());
        }
        let mut code: u32 = 1;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        Ok(code)
    }
}

// ---------------------------------------------------------------- start the tray at sign-in

fn xml_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The Task Scheduler definition that starts the tray when you sign in: runs as you, with your normal
/// rights, on your own logon (no admin), normal priority, no time limit, one instance, no battery
/// conditions.
pub fn task_xml(exe: &Path, sid: &str, enabled: bool) -> Result<String, InstallError> {
    static SID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^S-1-[0-9]+(-[0-9]+)+$").unwrap());
    if !SID_RE.is_match(sid) {
        return Err(InstallError(format!("not a SID: {sid}")));
    }
    let win = exe.to_string_lossy().replace('/', "\\");
    let dir = Path::new(&win).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    Ok([
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>",
        "<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">",
        "  <RegistrationInfo>",
        "    <Author>agent-wiki installer</Author>",
        "    <Description>Starts the Agent Wiki tray (and its curator) when you sign in. Written by agent-wiki install; removed by agent-wiki uninstall.</Description>",
        "  </RegistrationInfo>",
        "  <Triggers>",
        "    <LogonTrigger>",
        "      <Enabled>true</Enabled>",
        &format!("      <UserId>{sid}</UserId>"),
        "      <Delay>PT10S</Delay>",
        "    </LogonTrigger>",
        "  </Triggers>",
        "  <Principals>",
        "    <Principal id=\"Author\">",
        &format!("      <UserId>{sid}</UserId>"),
        "      <LogonType>InteractiveToken</LogonType>",
        "      <RunLevel>LeastPrivilege</RunLevel>",
        "    </Principal>",
        "  </Principals>",
        "  <Settings>",
        "    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
        "    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
        "    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
        "    <AllowHardTerminate>true</AllowHardTerminate>",
        "    <StartWhenAvailable>false</StartWhenAvailable>",
        "    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>",
        "    <IdleSettings>",
        "      <StopOnIdleEnd>false</StopOnIdleEnd>",
        "      <RestartOnIdle>false</RestartOnIdle>",
        "    </IdleSettings>",
        "    <AllowStartOnDemand>true</AllowStartOnDemand>",
        &format!("    <Enabled>{enabled}</Enabled>"),
        "    <Hidden>false</Hidden>",
        "    <RunOnlyIfIdle>false</RunOnlyIfIdle>",
        "    <WakeToRun>false</WakeToRun>",
        "    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
        "    <Priority>4</Priority>",
        "  </Settings>",
        "  <Actions Context=\"Author\">",
        "    <Exec>",
        &format!("      <Command>\"{}\"</Command>", xml_text(&win)),
        "      <Arguments>--from task</Arguments>",
        &format!("      <WorkingDirectory>{}</WorkingDirectory>", xml_text(&dir)),
        "    </Exec>",
        "  </Actions>",
        "</Task>",
        "",
    ]
    .join("\n"))
}

/// schtasks /xml reads UTF-16 only.
pub fn task_xml_bytes(xml: &str) -> Vec<u8> {
    let mut b = vec![0xff, 0xfe];
    for u in xml.encode_utf16() {
        b.extend_from_slice(&u.to_le_bytes());
    }
    b
}

pub fn delete_task(name: &str) -> bool {
    run(&system32("schtasks.exe"), &["/delete", "/tn", name, "/f"]).ok
}

pub fn reg_delete(key: &str, name: &str) -> bool {
    run(&system32("reg.exe"), &["delete", key, "/v", name, "/f"]).ok
}

/// Where Windows 11 shows the tray icon: new icons go to the hidden overflow (^) and only you can pin
/// them (Explorer keeps that choice itself). Report-only: (promoted, text).
pub fn tray_icon_placement(exe: &Path) -> (Option<bool>, String) {
    let r = run(&system32("reg.exe"), &["query", r"HKCU\Control Panel\NotifyIconSettings", "/s"]);
    let want = exe.to_string_lossy().to_lowercase();
    let mut cur_exe: Option<String> = None;
    let mut promoted: Option<bool> = None;
    let mut found: Option<Option<bool>> = None;
    for line in r.stdout.lines().chain(["HKEY_END"]) {
        if line.starts_with("HKEY_") {
            if cur_exe.as_deref().is_some_and(|e| e.to_lowercase() == want) {
                found = Some(promoted);
            }
            cur_exe = None;
            promoted = None;
            continue;
        }
        let t = line.trim();
        if let Some(v) = t.strip_prefix("ExecutablePath").and_then(|r| r.split_once("REG_SZ")).map(|(_, v)| v.trim().to_string()) {
            cur_exe = Some(v);
        }
        if let Some(v) = t.strip_prefix("IsPromoted").and_then(|r| r.split_once("REG_DWORD")).map(|(_, v)| v.trim().to_string()) {
            promoted = Some(v.trim_start_matches("0x").trim_start_matches('0') == "1");
        }
    }
    match found {
        None => (None, format!("Explorer has not registered it yet; once it shows, to pin it: {PIN_TRAY_ICON}")),
        Some(Some(true)) => (Some(true), "pinned to the taskbar".into()),
        Some(Some(false)) => (Some(false), format!("in the ^ overflow (your choice); to pin it: {PIN_TRAY_ICON}")),
        Some(None) => (None, format!("in the ^ overflow (where Windows 11 puts new icons); to pin it: {PIN_TRAY_ICON}")),
    }
}

/// The 8.3 form of a path with spaces, or None.
pub fn short_path(p: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: valid input string and output buffer with its length.
    let n = unsafe { GetShortPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..n]);
    (!s.contains(' ')).then_some(s)
}

/// A path as one token of a shell command line that works alike in cmd, PowerShell and bash: forward
/// slashes, and the 8.3 form instead of quotes when the path has spaces.
pub fn shell_token(p: &Path) -> String {
    if !p.to_string_lossy().contains(' ') {
        return fwd(p);
    }
    short_path(p).map(|s| s.replace('\\', "/")).unwrap_or_else(|| format!("\"{}\"", fwd(p)))
}

pub fn set_clipboard_from_file(file: &Path) -> Result<(), String> {
    let ps = format!("Set-Clipboard -Value (Get-Content -Raw -Encoding UTF8 -LiteralPath '{}')", file.to_string_lossy().replace('\'', "''"));
    let r = run_in(&system32("WindowsPowerShell\\v1.0\\powershell.exe"), &["-NoProfile", "-NonInteractive", "-Command", &ps], None, &[], Duration::from_secs(30));
    if r.ok { Ok(()) } else { Err(r.stderr.trim().to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_ace_added_once_before_the_sacl() {
        let sid = "S-1-5-21-1-2-3-1001";
        let base = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)S:(AU;FA;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;WD)";
        let with = sddl_with_start_stop(base, sid).unwrap();
        assert!(with.contains(&format!("(A;;RPWPLC;;;{sid})S:(AU")), "{with}");
        assert_eq!(sddl_with_start_stop(&with, sid).unwrap(), with, "already granted");
        let reordered = with.replace("RPWPLC", "LCRPWP");
        assert_eq!(sddl_with_start_stop(&reordered, sid).unwrap(), reordered, "same rights in another order");
        assert!(sddl_with_start_stop(base, "nope").is_err());
    }

    #[test]
    fn logon_task_runs_as_you() {
        let xml = task_xml(Path::new(r"C:\Users\x\AppData\Local\AgentWiki\tray\agent-wiki-tray.exe"), "S-1-5-21-1-2-3-1001", true).unwrap();
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>") && xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<Command>\"C:\\Users\\x\\AppData\\Local\\AgentWiki\\tray\\agent-wiki-tray.exe\"</Command>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>") && xml.contains("<Priority>4</Priority>"));
        assert_eq!(&task_xml_bytes("A")[..], &[0xff, 0xfe, b'A', 0]);
    }
}
