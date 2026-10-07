//! The request log: one JSON line per HTTP request, tool call and curator run, appended to
//! <log dir>/requests-YYYY-MM-DD.jsonl (src/reqlog.mjs). Each line is one append on a file opened for
//! appending (FILE_APPEND_DATA on Windows, O_APPEND elsewhere), so lines from several processes
//! never interleave. Every string is redacted by the secret guard first; lines stay under 4 KB.

use crate::secrets::redact_secrets;
use crate::text::{local_date, local_iso_ms, now, one_line, random_hex};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_LINE: usize = 4000;

/// Redacts, collapses to one line and truncates a value for the log.
pub fn clip(value: &Value, max: usize) -> Option<String> {
    let s = match value {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).collect::<Vec<_>>().join(", "),
        Value::Object(_) => value.to_string(),
        other => other.to_string(),
    };
    Some(clip_str(&s, max))
}

pub fn clip_str(s: &str, max: usize) -> String {
    let s = one_line(&redact_secrets(s));
    if s.chars().count() > max {
        let mut t: String = s.chars().take(max.saturating_sub(3)).collect();
        t.push_str("...");
        t
    } else {
        s
    }
}

/// Short, redacted summary of tool arguments. Bodies keep their length so a reader can tell what was sent.
pub fn summarize_args(args: &Value) -> Option<Value> {
    let obj = args.as_object()?;
    let mut out = Map::new();
    for (k, v) in obj {
        if v.is_null() || v.as_str() == Some("") {
            continue;
        }
        if k == "body" || k == "content" {
            out.insert(k.clone(), clip(v, 120).map(Value::String).unwrap_or(Value::Null));
            let len = match v {
                Value::String(s) => s.encode_utf16().count(),
                other => other.to_string().encode_utf16().count(),
            };
            out.insert(format!("{k}_chars"), Value::from(len));
        } else {
            out.insert(k.clone(), clip(v, if k == "app" { 40 } else { 160 }).map(Value::String).unwrap_or(Value::Null));
        }
    }
    Some(Value::Object(out))
}

pub fn new_request_id() -> String {
    random_hex(12)
}

struct State {
    part: u32,
    part_date: String,
    checked_at: i64,
    swept_day: String,
    failures: u32,
    /// The file being appended to, kept open (reopening it per line is slow on Windows).
    open: Option<(PathBuf, fs::File)>,
}

pub struct RequestLog {
    dir: Option<PathBuf>,
    proc_name: String,
    retention_days: i64,
    max_bytes: u64,
    state: Mutex<State>,
}

impl RequestLog {
    pub fn new(dir: &Path, proc_name: &str, retention_days: i64) -> Self {
        RequestLog {
            dir: Some(dir.to_path_buf()),
            proc_name: proc_name.to_string(),
            retention_days,
            max_bytes: 50 * 1024 * 1024,
            state: Mutex::new(State { part: 1, part_date: String::new(), checked_at: 0, swept_day: String::new(), failures: 0, open: None }),
        }
    }

    /// A logger that writes nothing.
    pub fn null() -> Self {
        RequestLog {
            dir: None,
            proc_name: String::new(),
            retention_days: 30,
            max_bytes: 0,
            state: Mutex::new(State { part: 1, part_date: String::new(), checked_at: 0, swept_day: String::new(), failures: 0, open: None }),
        }
    }

    fn file_for(dir: &Path, date: &str, n: u32) -> PathBuf {
        dir.join(if n > 1 { format!("requests-{date}.{n}.jsonl") } else { format!("requests-{date}.jsonl") })
    }

    fn current_file(&self, dir: &Path, st: &mut State) -> PathBuf {
        let date = local_date(&now());
        if date != st.part_date {
            st.part_date = date.clone();
            st.part = 1;
            st.checked_at = 0;
            if let Ok(rd) = fs::read_dir(dir) {
                for e in rd.flatten() {
                    if let Some((d, n)) = parse_name(&e.file_name().to_string_lossy())
                        && d == date
                        && n > st.part
                    {
                        st.part = n;
                    }
                }
            }
        }
        let now_ms = crate::text::now_ms();
        if now_ms - st.checked_at > 10_000 {
            st.checked_at = now_ms;
            match fs::metadata(Self::file_for(dir, &date, st.part)) {
                Ok(m) if m.len() >= self.max_bytes => st.part += 1,
                Err(_) => st.open = None, // deleted meanwhile: start it again
                _ => {}
            }
        }
        if st.swept_day != date {
            st.swept_day = date.clone();
            sweep(dir, self.retention_days);
        }
        Self::file_for(dir, &date, st.part)
    }

    /// Writes one entry (fields in order). Never fails the caller.
    pub fn write(&self, entry: Vec<(&str, Value)>) {
        let Some(dir) = &self.dir else { return };
        let mut st = self.state.lock().unwrap();
        if st.failures > 20 {
            return;
        }
        let mut rec = Map::new();
        rec.insert("t".into(), Value::String(local_iso_ms(&now())));
        rec.insert("proc".into(), Value::String(self.proc_name.clone()));
        rec.insert("pid".into(), Value::from(std::process::id()));
        for (k, v) in entry {
            if !v.is_null() {
                rec.insert(k.to_string(), v);
            } else {
                rec.remove(k);
            }
        }
        let mut line = Value::Object(rec.clone()).to_string();
        if line.len() > MAX_LINE {
            for k in ["args", "error", "detail"] {
                if let Some(v) = rec.get(k).cloned() {
                    rec.insert(k.into(), clip(&v, 300).map(Value::String).unwrap_or(Value::Null));
                }
            }
            line = Value::Object(rec.clone()).to_string();
            if line.len() > MAX_LINE {
                let mut small = Map::new();
                for k in ["t", "proc", "pid", "kind", "rid"] {
                    if let Some(v) = rec.get(k) {
                        small.insert(k.into(), v.clone());
                    }
                }
                small.insert("truncated".into(), Value::Bool(true));
                line = Value::Object(small).to_string();
            }
        }
        let file = self.current_file(dir, &mut st);
        if st.open.as_ref().is_none_or(|(p, _)| *p != file) {
            st.open = None;
            if fs::create_dir_all(dir).is_ok()
                && let Ok(f) = fs::OpenOptions::new().create(true).append(true).open(&file)
            {
                st.open = Some((file.clone(), f));
            }
        }
        // One write per line, in append mode: lines from several processes never interleave.
        let ok = st.open.as_mut().is_some_and(|(_, f)| f.write_all(format!("{line}\n").as_bytes()).is_ok());
        if ok {
            st.failures = 0;
        } else {
            st.open = None;
            st.failures += 1;
        }
    }
}

fn parse_name(n: &str) -> Option<(String, u32)> {
    let rest = n.strip_prefix("requests-")?.strip_suffix(".jsonl")?;
    let (date, part) = match rest.split_once('.') {
        Some((d, p)) => (d, p.parse().ok()?),
        None => (rest, 1),
    };
    let ok = date.len() == 10 && date.chars().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() });
    ok.then(|| (date.to_string(), part))
}

/// Deletes request logs older than `retention_days`.
pub fn sweep(dir: &Path, retention_days: i64) -> usize {
    let cutoff = local_date(&crate::text::from_ms(crate::text::now_ms() - retention_days * 86_400_000));
    let mut removed = 0;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Some((d, _)) = parse_name(&e.file_name().to_string_lossy())
                && d < cutoff
                && fs::remove_file(e.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}
