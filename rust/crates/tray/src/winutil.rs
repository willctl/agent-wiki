//! Small Win32 helpers: wide strings, the registry, the shell, the clipboard.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, KEY_READ, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetKeyValueW,
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Copies `s` into a fixed UTF-16 buffer (NUL-terminated, truncated to fit).
pub fn fill(buf: &mut [u16], s: &str) {
    let n = buf.len().saturating_sub(1);
    let mut i = 0;
    for c in s.encode_utf16().take(n) {
        buf[i] = c;
        i += 1;
    }
    buf[i] = 0;
}

fn reg_get(root: HKEY, key: &str, value: &str, kind: u32) -> Option<Vec<u8>> {
    let (k, v) = (wide(key), wide(value));
    let mut size: u32 = 0;
    // SAFETY: valid NUL-terminated strings; first call asks for the size.
    if unsafe { RegGetValueW(root, k.as_ptr(), v.as_ptr(), kind, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) } != ERROR_SUCCESS {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf has the size the first call reported.
    if unsafe { RegGetValueW(root, k.as_ptr(), v.as_ptr(), kind, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size) } != ERROR_SUCCESS {
        return None;
    }
    buf.truncate(size as usize);
    Some(buf)
}

/// A REG_SZ under HKCU.
pub fn hkcu_get_sz(key: &str, value: &str) -> Option<String> {
    let b = reg_get(HKEY_CURRENT_USER, key, value, RRF_RT_REG_SZ)?;
    let u: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|c| *c != 0).collect();
    Some(String::from_utf16_lossy(&u))
}

pub fn hkcu_get_dword(key: &str, value: &str) -> Option<u32> {
    let b = reg_get(HKEY_CURRENT_USER, key, value, windows_sys::Win32::System::Registry::RRF_RT_REG_DWORD)?;
    Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

pub fn hkcu_get_binary(key: &str, value: &str) -> Option<Vec<u8>> {
    reg_get(HKEY_CURRENT_USER, key, value, RRF_RT_REG_BINARY)
}

pub fn hkcu_set_sz(key: &str, value: &str, data: &str) -> Result<(), String> {
    let (k, v, d) = (wide(key), wide(value), wide(data));
    // SAFETY: valid strings; the data length includes the terminating NUL, in bytes.
    let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr(), REG_SZ, d.as_ptr().cast(), (d.len() * 2) as u32) };
    if r == ERROR_SUCCESS { Ok(()) } else { Err(format!("cannot write HKCU\\{key}\\{value} (error {r})")) }
}

/// Whether an app is registered for a file extension (".md" often has none).
pub fn has_association(ext: &str) -> bool {
    let k = wide(ext);
    let mut h: HKEY = std::ptr::null_mut();
    // SAFETY: valid string and out pointer.
    if unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, k.as_ptr(), 0, KEY_READ, &mut h) } != ERROR_SUCCESS {
        return false;
    }
    let mut subkeys: u32 = 0;
    // SAFETY: an open key; only the subkey count is asked for.
    unsafe {
        RegQueryInfoKeyW(
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            &mut subkeys,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        RegCloseKey(h);
    }
    subkeys > 0 || reg_get(HKEY_CLASSES_ROOT, ext, "", RRF_RT_REG_SZ).is_some_and(|b| b.len() > 2)
}

/// ShellExecute "open": a URL, a folder, or a file with its registered app.
pub fn shell_open(target: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, t) = (wide("open"), wide(target));
    // SAFETY: valid strings; returns a value > 32 on success.
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), t.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
    if r as isize > 32 { Ok(()) } else { Err(format!("cannot open {target} (error {})", r as isize)) }
}

/// %VARS% expanded.
pub fn expand_env(s: &str) -> String {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    let src = wide(s);
    let mut buf = vec![0u16; 1024];
    // SAFETY: buffer and its length in characters.
    let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n > buf.len() {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..n.saturating_sub(1)])
}

/// Puts text on the clipboard, retrying while another program holds it open.
pub fn set_clipboard(owner: HWND, text: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    let data = wide(text);
    let mut opened = false;
    for _ in 0..40 {
        // SAFETY: plain call; owner may be null.
        if unsafe { OpenClipboard(owner) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if !opened {
        return Err("The clipboard is in use by another program; try again.".into());
    }
    // SAFETY: the clipboard is open; the global block is filled before it is handed over, and freed only if not.
    let ok = unsafe {
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
        if h.is_null() {
            false
        } else {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
                GlobalUnlock(h);
            }
            let set = !SetClipboardData(CF_UNICODETEXT, h as _).is_null();
            if !set {
                GlobalFree(h);
            }
            set
        }
    };
    // SAFETY: we opened it above.
    unsafe { CloseClipboard() };
    if ok { Ok(()) } else { Err("Could not put the URL on the clipboard.".into()) }
}
