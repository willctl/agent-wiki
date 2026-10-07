//! The service's side of the embeddings key handoff (Windows). The service runs as
//! NT SERVICE\AgentWiki and cannot read the user's credentials, so hybrid search would stay BM25 for
//! every app that reaches it over HTTP. The tray, running as the user, reads the key and writes it to
//! this pipe; the service keeps it in memory only (aw_core::embed::set_key), never on disk.
//!
//! The pipe accepts only local clients running as the installing user (the ini's `owner` SID) or
//! SYSTEM. The tray checks the other end too: the pid /status reports, and a pipe owned by the
//! service's account, so a pipe someone else created first never receives the key.

#[cfg(windows)]
pub fn serve(port: u16, owner_sid: String) {
    std::thread::spawn(move || {
        if let Err(e) = serve_loop(port, &owner_sid) {
            eprintln!("{} [service] key pipe stopped: {e}", aw_core::text::local_iso_ms(&aw_core::text::now()));
        }
    });
}

#[cfg(windows)]
fn serve_loop(port: u16, owner_sid: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT};

    if !owner_sid.starts_with("S-1-") || !owner_sid[4..].chars().all(|c| c.is_ascii_digit() || c == '-') {
        return Err(format!("owner is not a SID: {owner_sid}"));
    }
    // Owner rights and SYSTEM in full; the installing user may read and write; nobody else.
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;GA;;;OW)(A;;GRGW;;;{owner_sid})").encode_utf16().chain([0]).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a valid SDDL string; the descriptor is freed with LocalFree when the loop ends.
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) } == 0 {
        return Err(format!("security descriptor: error {}", unsafe { GetLastError() }));
    }
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
    let wide: Vec<u16> = aw_core::embed::key_pipe_name(port).encode_utf16().chain([0]).collect();
    let result = loop {
        // SAFETY: plain Win32 calls on a handle this loop owns and closes.
        let pipe = unsafe { CreateNamedPipeW(wide.as_ptr(), PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, 1, 1024, 1024, 0, &sa) };
        if pipe == INVALID_HANDLE_VALUE {
            break Err(format!("cannot create the pipe: error {}", unsafe { GetLastError() }));
        }
        unsafe {
            let connected = ConnectNamedPipe(pipe, std::ptr::null_mut()) != 0 || GetLastError() == 535; // ERROR_PIPE_CONNECTED
            if connected {
                let mut buf = [0u8; 1024];
                let mut n = 0u32;
                let reply: &[u8] = if ReadFile(pipe, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) != 0 {
                    let key = String::from_utf8_lossy(&buf[..n as usize]).trim().to_string();
                    if (8..=512).contains(&key.len()) && key.chars().all(|c| c.is_ascii_graphic()) {
                        aw_core::embed::set_key(Some(key));
                        b"ok\n"
                    } else {
                        b"refused\n"
                    }
                } else {
                    b"refused\n"
                };
                let mut written = 0u32;
                WriteFile(pipe, reply.as_ptr(), reply.len() as u32, &mut written, std::ptr::null_mut());
                FlushFileBuffers(pipe);
                DisconnectNamedPipe(pipe);
            }
            CloseHandle(pipe);
        }
    };
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW above.
    unsafe { LocalFree(sd as _) };
    result
}
