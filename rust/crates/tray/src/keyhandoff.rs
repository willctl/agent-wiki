//! The tray's side of the embeddings key handoff (Windows only: the macOS and Linux services run as the
//! user and read the key themselves; cli/src/keypipe.rs has the service's side).
//! When /status says hybrid search is on but the service has no key, the tray, running as the user,
//! reads the key from the user's credential store and writes it to the service's pipe. Before writing,
//! it checks that the other end is the service: the pid /status reports, and a pipe owned by
//! NT SERVICE\AgentWiki (or, when the user may query it, a process running the installed
//! agent-wiki.exe). Otherwise the key is not sent.

use crate::config::Config;
use crate::status::Status;

/// What happened, for the tray's log; None when there was nothing to do.
pub fn offer(cfg: &Config, status: &Status) -> Option<String> {
    use std::io::{Read, Write};
    use std::os::windows::io::AsRawHandle;
    use std::sync::Mutex;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;

    if status.get(&["embeddings", "keyAvailable"]).and_then(serde_json::Value::as_bool) != Some(false) {
        return None;
    }
    static LAST: Mutex<i64> = Mutex::new(0);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if aw_core::text::now_ms() - *last < 60_000 {
            return None;
        }
        *last = aw_core::text::now_ms();
    }
    let config = aw_core::wiki::read_config(&aw_core::paths::process_env()).ok()?;
    let settings = aw_core::embed::Settings::from_config(&config)?;
    let Some(key) = aw_core::embed::api_key(&settings) else {
        return Some(format!("hybrid search is on, but no key is stored as \"{}\" for this account", settings.credential));
    };
    let mut pipe = match std::fs::OpenOptions::new().read(true).write(true).open(aw_core::embed::key_pipe_name(cfg.port)) {
        Ok(p) => p,
        Err(e) => return Some(format!("the service's key pipe is not open ({e})")),
    };
    let mut pid = 0u32;
    // SAFETY: the handle is the open pipe; pid is written by the call.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle() as _, &mut pid) } == 0 || i64::from(pid) != status.int(&["pid"]) {
        return Some(format!("not sending the key: the pipe's server (pid {pid}) is not the service /status names"));
    }
    // The pipe must belong to the service's own account (another account's pipe of the same name is
    // owned by that account). The process image path is a second way to tell, when it can be read.
    let owner = pipe_owner(pipe.as_raw_handle() as _);
    let service = account_sid(r"NT SERVICE\AgentWiki");
    let image = image_of(pid);
    let expected = cfg.agent.to_string_lossy().replace('/', "\\");
    let owned = owner.is_some() && owner == service;
    if !owned && !image.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(&expected)) {
        return Some(format!(
            "not sending the key: the pipe belongs to {} (the service is {}), and its server is {}",
            owner.unwrap_or_else(|| "unknown".into()),
            service.unwrap_or_else(|| "unknown".into()),
            image.unwrap_or_else(|| "unknown".into())
        ));
    }
    if pipe.write_all(format!("{key}\n").as_bytes()).is_err() {
        return Some("could not write to the service's key pipe".into());
    }
    let mut reply = String::new();
    let _ = pipe.read_to_string(&mut reply);
    Some(format!("handed the embeddings key to the service (pid {pid}): {}", reply.trim()))
}

/// A kernel object's owner, as a string SID.
fn pipe_owner(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
    let mut owner: PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the handle is open with READ_CONTROL; the descriptor is freed below and owner points into it.
    unsafe {
        if GetSecurityInfo(handle, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), &mut sd) != 0 {
            return None;
        }
        let s = sid_string(owner);
        LocalFree(sd as _);
        s
    }
}

/// An account's SID by name ("NT SERVICE\AgentWiki"), as a string.
fn account_sid(name: &str) -> Option<String> {
    use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let mut sid = [0u8; 128];
    let mut sid_len = sid.len() as u32;
    let mut domain = [0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut kind: SID_NAME_USE = 0;
    // SAFETY: buffers and their lengths are passed in; the SID is read from our own buffer.
    unsafe {
        if LookupAccountNameW(std::ptr::null(), wide.as_ptr(), sid.as_mut_ptr() as _, &mut sid_len, domain.as_mut_ptr(), &mut domain_len, &mut kind) == 0 {
            return None;
        }
        sid_string(sid.as_mut_ptr() as _)
    }
}

/// # Safety
/// `sid` must point to a valid SID.
unsafe fn sid_string(sid: windows_sys::Win32::Security::PSID) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let mut s: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: the caller passes a valid SID; the string is freed with LocalFree.
    unsafe {
        if ConvertSidToStringSidW(sid, &mut s) == 0 || s.is_null() {
            return None;
        }
        let len = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
        LocalFree(s as _);
        Some(out)
    }
}

/// A process's image path, when this account may query it.
fn image_of(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    // SAFETY: a limited query handle, closed below; the buffer length is passed in and updated.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}
