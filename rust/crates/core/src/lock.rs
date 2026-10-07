//! Cross-process locks for the wiki folder (src/lock.mjs), compatible with the Node programs:
//! `mkdir .locks/<name>.lock` is the mutex, `<name>.lock/owner` holds the owner as JSON, and on
//! Windows every lock holder keeps `.locks/alive/<pid>-<start>.alive` open with share mode 0 for its
//! whole life, so anyone can tell instantly whether an owner is alive. Elsewhere an owner is checked
//! by pid and by the start time of the process holding that pid now.

use crate::text::{now_ms, random_hex};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

const LEGACY_STALE_MS: i64 = 30_000;
const NO_OWNER_GRACE_MS: i64 = 5_000;
const START_TOLERANCE_MS: i64 = 2_000;

#[derive(Debug)]
pub struct LockBusy(pub String);

/// When this process started (ms since the epoch), as the OS records it.
pub fn process_start() -> i64 {
    static START: OnceLock<i64> = OnceLock::new();
    *START.get_or_init(|| process_start_ms(std::process::id() as i64).unwrap_or_else(now_ms))
}

pub fn host() -> String {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(hostname).clone()
}

fn hostname() -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{ComputerNamePhysicalDnsHostname, GetComputerNameExW};
        let mut size: u32 = 0;
        unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, std::ptr::null_mut(), &mut size) };
        let mut buf = vec![0u16; size as usize + 1];
        if unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &mut size) } != 0 {
            return String::from_utf16_lossy(&buf[..size as usize]);
        }
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
        String::new()
    }
}

/// A stable id for this machine: /etc/machine-id (Linux), the platform UUID (macOS), else the hostname.
pub fn machine_id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        #[cfg(target_os = "linux")]
        for f in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(s) = fs::read_to_string(f)
                && !s.trim().is_empty()
            {
                return s.trim().to_string();
            }
        }
        #[cfg(target_os = "macos")]
        if let Ok(out) = std::process::Command::new("/usr/sbin/ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(m) = regex::Regex::new(r#""IOPlatformUUID"\s*=\s*"([^"]+)""#).unwrap().captures(&text) {
                return m[1].to_string();
            }
        }
        host()
    })
    .clone()
}

/// When the process with this pid started (ms since the epoch), where that can be read.
pub fn process_start_ms(pid: i64) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 2..];
        let ticks: i64 = rest.split(' ').nth(19)?.parse().ok()?;
        let proc_stat = fs::read_to_string("/proc/stat").ok()?;
        let btime: i64 = proc_stat.lines().find_map(|l| l.strip_prefix("btime "))?.trim().parse().ok()?;
        Some(btime * 1000 + (ticks * 1000) / 100)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h.is_null() {
                return None;
            }
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
            let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
            CloseHandle(h);
            if ok == 0 {
                return None;
            }
            let t = ((c.dwHighDateTime as i64) << 32) | c.dwLowDateTime as i64; // 100 ns since 1601
            Some(t / 10_000 - 11_644_473_600_000)
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let out = std::process::Command::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()]).env("LC_ALL", "C").output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let t = chrono::NaiveDateTime::parse_from_str(&s.split_whitespace().collect::<Vec<_>>().join(" "), "%a %b %d %H:%M:%S %Y").ok()?;
        t.and_local_timezone(chrono::Local).single().map(|d| d.timestamp_millis())
    }
}

/// Whether a process with this pid exists now (Node: process.kill(pid, 0); EPERM counts as alive).
fn pid_alive(pid: i64) -> bool {
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if h.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok == 0 || code == STILL_ACTIVE as u32
        }
    }
}

// ---------------------------------------------------------------- alive files (Windows)

static ALIVE: LazyLock<Mutex<HashMap<PathBuf, Option<fs::File>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg_attr(not(windows), allow(dead_code))]
fn alive_name(pid: i64, start: i64) -> String {
    format!("{pid}-{start}.alive")
}

#[cfg(windows)]
fn open_exclusive(file: &Path, create: bool) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new().read(true).write(true).create(create).share_mode(0).open(file)
}

/// Opens (once per .locks folder) and keeps our alive file. False where that is impossible.
fn ensure_alive(locks: &Path) -> bool {
    #[cfg(not(windows))]
    {
        let _ = locks;
        false
    }
    #[cfg(windows)]
    {
        let mut map = ALIVE.lock().unwrap();
        if let Some(v) = map.get(locks) {
            return v.is_some();
        }
        let dir = locks.join("alive");
        let f = fs::create_dir_all(&dir).ok().and_then(|_| open_exclusive(&dir.join(alive_name(std::process::id() as i64, process_start())), true).ok());
        let ok = f.is_some();
        map.insert(locks.to_path_buf(), f);
        ok
    }
}

/// Closes our alive files (tests, before deleting a wiki folder). Only call it holding no locks.
pub fn release_alive() {
    ALIVE.lock().unwrap().clear();
}

// ---------------------------------------------------------------- owners

#[derive(Clone, Debug, Default)]
pub struct Owner {
    pub pid: i64,
    pub start: Option<i64>,
    pub host: Option<String>,
    pub machine: Option<String>,
    pub alive: bool,
    pub label: Option<String>,
    pub token: Option<String>,
    pub since: Option<i64>,
    pub legacy: bool,
}

/// Parses `owner`: v2 JSON, or the v1.1 text "pid N at <time>".
pub fn parse_owner(text: &str) -> Option<Owner> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with('{') {
        let v: Value = serde_json::from_str(t).ok()?;
        let pid = v.get("pid")?.as_i64()?;
        if v.get("pid")?.as_f64()? != pid as f64 {
            return None;
        }
        return Some(Owner {
            pid,
            start: v.get("start").and_then(Value::as_i64),
            host: v.get("host").and_then(Value::as_str).map(String::from),
            machine: v.get("machine").and_then(Value::as_str).map(String::from),
            alive: v.get("alive").and_then(Value::as_bool).unwrap_or(false),
            label: v.get("label").and_then(Value::as_str).map(String::from),
            token: v.get("token").and_then(Value::as_str).map(String::from),
            since: v.get("since").and_then(Value::as_i64),
            legacy: false,
        });
    }
    let rest = t.strip_prefix("pid ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some(Owner { pid: digits.parse().ok()?, legacy: true, ..Default::default() })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    Unknown,
}

impl Liveness {
    pub fn as_str(self) -> &'static str {
        match self {
            Liveness::Alive => "alive",
            Liveness::Dead => "dead",
            Liveness::Unknown => "unknown",
        }
    }
}

pub fn owner_liveness(locks: &Path, owner: &Owner) -> Liveness {
    let me = std::process::id() as i64;
    if owner.pid == me && owner.start.is_none_or(|s| s == process_start()) {
        return Liveness::Alive;
    }
    let other_machine = match &owner.machine {
        Some(m) => *m != machine_id(),
        None => owner.host.as_ref().is_some_and(|h| *h != host()),
    };
    if other_machine {
        return Liveness::Unknown;
    }
    #[cfg(windows)]
    if owner.alive
        && let Some(start) = owner.start
    {
        let file = locks.join("alive").join(alive_name(owner.pid, start));
        return match open_exclusive(&file, false) {
            Ok(f) => {
                drop(f);
                let _ = fs::remove_file(&file);
                Liveness::Dead
            }
            Err(e) if e.raw_os_error() == Some(32) || e.raw_os_error() == Some(33) => Liveness::Alive, // sharing / lock violation
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Dead,
            Err(_) => Liveness::Unknown,
        };
    }
    #[cfg(not(windows))]
    let _ = locks;
    if !pid_alive(owner.pid) {
        return Liveness::Dead;
    }
    // The pid is in use: by the owner, or by a process that got its pid after it died?
    if !cfg!(windows)
        && let (Some(start), Some(now)) = (owner.start, process_start_ms(owner.pid))
        && (now - start).abs() > START_TOLERANCE_MS
    {
        return Liveness::Dead;
    }
    Liveness::Alive
}

pub struct Seen {
    pub state: Option<Liveness>, // None = gone
    pub owner: Option<Owner>,
    pub age_ms: i64,
}

fn inspect(locks: &Path, lock_dir: &Path) -> Seen {
    let Ok(st) = fs::metadata(lock_dir) else {
        return Seen { state: None, owner: None, age_ms: 0 };
    };
    let owner = fs::read_to_string(lock_dir.join("owner")).ok().and_then(|t| parse_owner(&t));
    let mtime = st.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis() as i64).unwrap_or(0);
    let age_ms = now_ms() - owner.as_ref().and_then(|o| o.since).unwrap_or(mtime);
    let state = Some(owner.as_ref().map_or(Liveness::Unknown, |o| owner_liveness(locks, o)));
    Seen { state, owner, age_ms }
}

/// Moves a lock judged stale out of the way, then deletes it. Puts it back if it turned out to be someone else's.
fn break_lock(lock_dir: &Path, expected_token: Option<&str>) -> bool {
    let stale = PathBuf::from(format!("{}.stale-{}-{}", lock_dir.display(), std::process::id(), random_hex(6)));
    if fs::rename(lock_dir, &stale).is_err() {
        return false;
    }
    let owner = fs::read_to_string(stale.join("owner")).ok().and_then(|t| parse_owner(&t));
    if let (Some(exp), Some(tok)) = (expected_token, owner.as_ref().and_then(|o| o.token.as_deref()))
        && exp != tok
    {
        let _ = fs::rename(&stale, lock_dir);
        return false;
    }
    let _ = fs::remove_dir_all(&stale);
    true
}

pub struct LockOpts {
    pub timeout_ms: i64,
    pub max_hold_ms: i64,
    pub label: String,
}

impl Default for LockOpts {
    fn default() -> Self {
        LockOpts { timeout_ms: 10_000, max_hold_ms: 5 * 60_000, label: label() }
    }
}

/// The label lock owners record: AGENT_WIKI_PROCESS, or "agent-wiki".
pub fn label() -> String {
    std::env::var("AGENT_WIKI_PROCESS").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "agent-wiki".into())
}

/// A held lock: released when dropped.
pub struct Guard {
    lock_dir: PathBuf,
    token: String,
    released: bool,
}

impl Guard {
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        // Rename first so the lock disappears in one step even if we die halfway through deleting it.
        let cur = fs::read_to_string(self.lock_dir.join("owner")).ok().and_then(|t| parse_owner(&t));
        if cur.and_then(|o| o.token).as_deref() != Some(self.token.as_str()) {
            return; // broken by someone else (we held it far too long)
        }
        let gone = PathBuf::from(format!("{}.released-{}-{}", self.lock_dir.display(), std::process::id(), random_hex(6)));
        if fs::rename(&self.lock_dir, &gone).is_ok() {
            let _ = fs::remove_dir_all(&gone);
        } else {
            let _ = fs::remove_dir_all(&self.lock_dir);
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.release();
    }
}

fn jitter_sleep(lo: u64, span: u64) {
    let r = u64::from_str_radix(&random_hex(4), 16).unwrap_or(0) % span.max(1);
    std::thread::sleep(Duration::from_millis(lo + r));
}

pub enum AcquireError {
    Busy(LockBusy),
    Io(std::io::Error),
}

/// Takes the named lock. Busy after `timeout_ms`.
pub fn acquire(wiki_dir: &Path, name: &str, opts: &LockOpts) -> Result<Guard, AcquireError> {
    let locks = wiki_dir.join(".locks");
    fs::create_dir_all(&locks).map_err(AcquireError::Io)?;
    let alive = ensure_alive(&locks);
    let lock_dir = locks.join(format!("{name}.lock"));
    let token = random_hex(16);
    let t0 = Instant::now();
    loop {
        let failed = match fs::create_dir(&lock_dir) {
            Ok(()) => {
                let owner = json!({
                    "v": 2, "pid": std::process::id(), "start": process_start(), "host": host(), "machine": machine_id(),
                    "alive": alive, "label": opts.label, "token": token, "since": now_ms(),
                });
                fs::write(lock_dir.join("owner"), format!("{owner}\n")).map_err(AcquireError::Io)?;
                return Ok(Guard { lock_dir, token, released: false });
            }
            Err(e) => {
                let busyish = e.kind() == std::io::ErrorKind::AlreadyExists || e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(32);
                if !busyish {
                    return Err(AcquireError::Io(e));
                }
                e
            }
        };
        let seen = inspect(&locks, &lock_dir);
        let Some(state) = seen.state else {
            if failed.kind() == std::io::ErrorKind::AlreadyExists {
                continue; // released just now: take it
            }
            // Not there, yet mkdir failed: delete pending (Windows) or no permission on .locks.
            if t0.elapsed().as_millis() as i64 >= opts.timeout_ms {
                return Err(AcquireError::Io(failed));
            }
            jitter_sleep(10, 40);
            continue;
        };
        let legacy = seen.owner.as_ref().is_some_and(|o| o.legacy);
        let stale = match state {
            Liveness::Dead => true,
            Liveness::Unknown => seen.age_ms > if seen.owner.is_some() { LEGACY_STALE_MS } else { NO_OWNER_GRACE_MS },
            Liveness::Alive => seen.age_ms > if legacy { LEGACY_STALE_MS } else { opts.max_hold_ms },
        };
        if stale {
            break_lock(&lock_dir, seen.owner.as_ref().and_then(|o| o.token.as_deref()));
            continue;
        }
        if t0.elapsed().as_millis() as i64 >= opts.timeout_ms {
            let who = seen.owner.as_ref().map_or("another process".to_string(), |o| format!("{} {}", o.label.clone().unwrap_or_else(|| "pid".into()), o.pid));
            return Err(AcquireError::Busy(LockBusy(format!("The wiki {name} lock is held by {who} (for {}s).", (seen.age_ms as f64 / 1000.0).round()))));
        }
        jitter_sleep(5, 35);
    }
}

/// Who holds a lock right now, for /status: None or (state, owner, age_ms).
pub fn lock_info(wiki_dir: &Path, name: &str) -> Option<Seen> {
    let locks = wiki_dir.join(".locks");
    let seen = inspect(&locks, &locks.join(format!("{name}.lock")));
    seen.state.map(|_| seen)
}

/// Startup cleanup: alive files of dead processes, and leftovers of broken or released locks.
pub fn cleanup_locks(wiki_dir: &Path) -> usize {
    let locks = wiki_dir.join(".locks");
    let mut removed = 0;
    if let Ok(rd) = fs::read_dir(&locks) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.contains(".lock.stale-") || n.contains(".lock.released-") {
                let _ = fs::remove_dir_all(e.path());
                removed += 1;
            }
        }
    }
    #[cfg(windows)]
    if let Ok(rd) = fs::read_dir(locks.join("alive")) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let Some(stem) = n.strip_suffix(".alive") else { continue };
            let Some((pid, start)) = stem.split_once('-') else { continue };
            if let (Ok(pid), Ok(start)) = (pid.parse(), start.parse()) {
                let o = Owner { pid, start: Some(start), host: Some(host()), machine: Some(machine_id()), alive: true, ..Default::default() };
                if owner_liveness(&locks, &o) == Liveness::Dead {
                    removed += 1;
                }
            }
        }
    }
    removed
}
