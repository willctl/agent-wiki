//! Small OS facts the programs need.

use std::path::Path;

/// Which file is at `path` right now: device and inode on Unix, volume and file index on Windows. A
/// file replaced by an install (renamed aside, a new one copied in) has a new identity even when its
/// size and times are the same (Windows keeps the source's time on copy, and NTFS tunneling restores
/// the old creation time).
pub fn file_identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path).ok()?;
        Some((m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        // std opens with read, write and delete sharing: this never gets in an installer's way.
        let f = std::fs::File::open(path).ok()?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid open handle and a properly sized out struct.
        if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
            return None;
        }
        Some((info.dwVolumeSerialNumber as u64, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_file_has_a_new_identity() {
        let dir = std::env::temp_dir().join(format!("aw-sys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x");
        std::fs::write(&f, "same").unwrap();
        let a = file_identity(&f).unwrap();
        assert_eq!(file_identity(&f), Some(a));
        std::fs::rename(&f, dir.join("x.old")).unwrap();
        std::fs::write(&f, "same").unwrap();
        assert_ne!(file_identity(&f), Some(a));
        let _ = std::fs::remove_dir_all(dir);
    }
}
