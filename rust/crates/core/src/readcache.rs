//! File contents for reads that only display or search (pages, log days, inbox notes), cached by what
//! the directory listing says: size and modification time, which Windows reports without opening the
//! file (opening is the slow part there, with the virus scanner looking at each one). An entry is
//! re-read when either changes, at least every 30 seconds, and as soon as this process writes the file.
//! Writes and conflict checks always read the file itself.

use std::collections::HashMap;
use std::fs::DirEntry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

const MAX_BYTES: usize = 64 << 20;
const RECHECK: Duration = Duration::from_secs(30);

struct Entry {
    len: u64,
    mtime: i128,
    at: Instant,
    text: Arc<str>,
}

static CACHE: LazyLock<Mutex<(HashMap<PathBuf, Entry>, usize)>> = LazyLock::new(|| Mutex::new((HashMap::new(), 0)));

/// Size and modification time (ns) from a directory entry.
pub fn stamp(e: &DirEntry) -> Option<(u64, i128)> {
    let m = e.metadata().ok()?;
    let t = m.modified().ok()?.duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as i128).unwrap_or(0);
    Some((m.len(), t))
}

/// The file's text (lossy UTF-8), from the cache when the listing shows it unchanged.
pub fn read(path: &Path, len: u64, mtime: i128) -> Option<Arc<str>> {
    {
        let c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = c.0.get(path)
            && e.len == len
            && e.mtime == mtime
            && e.at.elapsed() < RECHECK
        {
            return Some(e.text.clone());
        }
    }
    let bytes = std::fs::read(path).ok()?;
    let text: Arc<str> = Arc::from(String::from_utf8_lossy(&bytes).as_ref());
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if c.1 + text.len() > MAX_BYTES {
        c.0.clear();
        c.1 = 0;
    }
    let size = text.len();
    if let Some(old) = c.0.insert(path.to_path_buf(), Entry { len, mtime, at: Instant::now(), text: text.clone() }) {
        c.1 = c.1.saturating_sub(old.text.len());
    }
    c.1 += size;
    Some(text)
}

/// Reads a directory entry's file through the cache.
pub fn read_entry(e: &DirEntry) -> Option<Arc<str>> {
    let (len, mtime) = stamp(e)?;
    read(&e.path(), len, mtime)
}

/// This process wrote `path`: drop what the cache holds for it.
pub fn forget(path: &Path) {
    let canonical = std::fs::canonicalize(path).ok();
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    for key in std::iter::once(path).chain(canonical.as_deref()) {
        if let Some(old) = c.0.remove(key) {
            c.1 = c.1.saturating_sub(old.text.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_file_is_read_again() {
        let dir = std::env::temp_dir().join(format!("aw-rc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.md");
        std::fs::write(&f, "one").unwrap();
        let entry = || std::fs::read_dir(&dir).unwrap().flatten().find(|e| e.file_name() == "a.md").unwrap();
        assert_eq!(&*read_entry(&entry()).unwrap(), "one");
        std::fs::write(&f, "three").unwrap();
        assert_eq!(&*read_entry(&entry()).unwrap(), "three", "the size changed");
        forget(&f);
        std::fs::write(&f, "THREE").unwrap();
        assert_eq!(&*read_entry(&entry()).unwrap(), "THREE", "forgotten after a write");
        let _ = std::fs::remove_dir_all(dir);
    }
}
