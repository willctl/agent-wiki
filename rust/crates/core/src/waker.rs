//! Interruptible naps for the curator's and Ask's loops: a nap ends early when something it waits
//! for changes on disk (a cheap poll of a directory signature, instead of fs.watch) or when the
//! process is stopping.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct Waker {
    generation: Mutex<u64>,
    cv: Condvar,
    stop: AtomicBool,
}

impl Waker {
    pub fn new() -> Arc<Self> {
        Arc::new(Waker { generation: Mutex::new(0), cv: Condvar::new(), stop: AtomicBool::new(false) })
    }

    pub fn wake(&self) {
        *self.generation.lock().unwrap() += 1;
        self.cv.notify_all();
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Sleeps up to `ms` (at least 10), returning early on wake() or stop().
    pub fn nap(&self, ms: i64) {
        let deadline = Instant::now() + Duration::from_millis(ms.max(10) as u64);
        let mut g = self.generation.lock().unwrap();
        let start = *g;
        while !self.stopped() && *g == start {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            g = self.cv.wait_timeout(g, left).unwrap().0;
        }
    }

    /// Waits up to `ms` for stop(): true once stopping.
    pub fn wait_stop(&self, ms: i64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms.max(0) as u64);
        let mut g = self.generation.lock().unwrap();
        while !self.stopped() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            g = self.cv.wait_timeout(g, left).unwrap().0;
        }
        self.stopped()
    }

    /// Wakes whenever `signature` changes (checked every 250 ms) until stopped.
    pub fn watch(self: &Arc<Self>, signature: impl Fn() -> String + Send + 'static) -> std::thread::JoinHandle<()> {
        let me = self.clone();
        std::thread::spawn(move || {
            let mut last = signature();
            while !me.wait_stop(250) {
                let now = signature();
                if now != last {
                    last = now;
                    me.wake();
                }
            }
        })
    }
}

/// A directory's entries (names, sizes and modification times), minus the names `skip` matches.
pub fn dir_signature(dir: &Path, skip: &[&str]) -> String {
    let Ok(rd) = std::fs::read_dir(dir) else { return String::new() };
    let mut items: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if skip.iter().any(|s| *s == name) || name.ends_with(".tmp") {
                return None;
            }
            let m = e.metadata().ok();
            let mtime = m.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
            Some(format!("{name}:{}:{mtime}", m.map(|m| m.len()).unwrap_or(0)))
        })
        .collect();
    items.sort();
    items.join("|")
}

/// Whether each path exists, as a signature.
pub fn exists_signature(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| if p.exists() { '1' } else { '0' }).collect()
}
