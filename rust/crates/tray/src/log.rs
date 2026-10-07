//! logs/tray.log: when the tray started and by what, its start-at-sign-in checks, and what it did.

use crate::config::Config;
use std::io::Write;
use std::sync::Mutex;

static GATE: Mutex<()> = Mutex::new(());

fn now() -> String {
    aw_core::text::local_iso_ms(&aw_core::text::now())
}

/// Appends to a log file, rotating it (to .1) past `max` bytes.
pub fn append(dir: &std::path::Path, name: &str, max: u64, line: &str) {
    let _g = GATE.lock().unwrap_or_else(|e| e.into_inner());
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let file = dir.join(name);
    if std::fs::metadata(&file).is_ok_and(|m| m.len() > max) {
        let old = dir.join(format!("{name}.1"));
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&file, &old);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
        let _ = f.write_all(format!("{} {line}\n", now()).as_bytes());
    }
}

pub fn tray(cfg: &Config, msg: &str) {
    append(&cfg.log_dir, "tray.log", 1024 * 1024, &format!("[tray] {msg}"));
}
