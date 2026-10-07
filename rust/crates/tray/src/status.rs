//! The service's read-only GET /status on 127.0.0.1: the only way the tray talks to the service.

use crate::config::Config;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub data: Option<Value>,
    pub error: Option<String>,
}

/// GET http://127.0.0.1:<port><path>: (status code, body). Plain HTTP/1.1, 4 s timeouts, no proxy.
pub fn http_get(port: u16, path: &str) -> Result<(u16, String), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let t = Duration::from_secs(4);
    let mut s = TcpStream::connect_timeout(&addr, t).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(t));
    let _ = s.set_write_timeout(Some(t));
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed response")?;
    let code = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("malformed status line")?;
    Ok((code, body.to_string()))
}

impl Status {
    pub fn fetch(cfg: &Config) -> Status {
        match http_get(cfg.port, cfg.status_url_path()) {
            Ok((_, body)) => match serde_json::from_str::<Value>(&body) {
                Ok(v @ Value::Object(_)) => Status { data: Some(v), error: None },
                _ => Status { data: None, error: Some("not JSON".into()) },
            },
            Err(e) => Status { data: None, error: Some(e) },
        }
    }

    pub fn reachable(&self) -> bool {
        self.data.is_some()
    }

    pub fn state(&self) -> &'static str {
        if !self.reachable() {
            "down"
        } else if self.str(&["health"]).as_deref() == Some("ok") {
            "healthy"
        } else {
            "degraded"
        }
    }

    pub fn get(&self, keys: &[&str]) -> Option<&Value> {
        let mut cur = self.data.as_ref()?;
        for k in keys {
            cur = cur.get(*k)?;
        }
        (!cur.is_null()).then_some(cur)
    }

    /// When /status lists changes applied automatically after `seen` (the time of the newest one
    /// announced; None: never announced any): the notification's title and text, and the newest time.
    pub fn auto_applied_since(&self, seen: Option<i64>) -> Option<(String, String, i64)> {
        let times: Vec<i64> = self.get(&["autoApplied", "times"])?.as_array()?.iter().filter_map(Value::as_i64).collect();
        let newest = *times.iter().max()?;
        let new = times.iter().filter(|t| seen.is_none_or(|s| **t > s)).count();
        if new == 0 {
            return None;
        }
        let title = self.str(&["autoApplied", "last", "title"]).unwrap_or_default();
        let lint = self.str(&["autoApplied", "last", "kind"]).as_deref() == Some("lint");
        let what = if new == 1 { if lint { "Agent Wiki applied a cleanup".to_string() } else { "Agent Wiki applied a change".to_string() } } else { format!("Agent Wiki applied {new} changes") };
        let text = if new == 1 { format!("{title}. Open Agent Wiki > Inbox to see it or undo it.") } else { format!("The last one: {title}. Open Agent Wiki > Inbox to see them or undo them.") };
        Some((what, text, newest))
    }

    pub fn str(&self, keys: &[&str]) -> Option<String> {
        self.get(keys).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }

    pub fn int(&self, keys: &[&str]) -> i64 {
        self.get(keys).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(0)
    }

    pub fn bool(&self, keys: &[&str]) -> bool {
        self.get(keys).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn reasons(&self) -> Vec<String> {
        self.get(&["reasons"]).and_then(Value::as_array).map(|a| a.iter().map(|r| r.as_str().map(String::from).unwrap_or_else(|| r.to_string())).collect()).unwrap_or_default()
    }

    pub fn recent(&self) -> Vec<Value> {
        self.get(&["recent"]).and_then(Value::as_array).map(|a| a.iter().filter(|x| x.is_object()).cloned().collect()).unwrap_or_default()
    }

    pub fn tooltip(&self) -> String {
        let t = if !self.reachable() {
            "Agent Wiki: service down".to_string()
        } else {
            let dead = self.int(&["queue", "dead"]);
            let mut t =
                format!("Agent Wiki {}: {} · {} queued", self.str(&["version"]).unwrap_or_default(), if self.state() == "healthy" { "OK" } else { "needs attention" }, self.int(&["queue", "pending"]));
            if dead > 0 {
                t.push_str(&format!(" · {dead} failed"));
            }
            t
        };
        if t.chars().count() > 63 { format!("{}...", t.chars().take(60).collect::<String>()) } else { t }
    }
}

/// What the tray has announced, kept in a file so a restart (or the upgrade that applies everything
/// left waiting before the tray's first poll) neither repeats a notification nor skips one.
pub struct Announced {
    file: PathBuf,
    pub seen: Option<i64>,
}

impl Announced {
    pub fn load(file: &Path) -> Announced {
        let seen = std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["autoSeen"].as_i64());
        Announced { file: file.to_path_buf(), seen }
    }

    /// The notification to show for this poll, if any; remembers it as shown.
    pub fn take(&mut self, s: &Status) -> Option<(String, String)> {
        let (title, text, newest) = s.auto_applied_since(self.seen)?;
        self.seen = Some(newest);
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.file, format!("{}\n", serde_json::json!({ "autoSeen": newest })));
        Some((title, text))
    }
}

pub fn uptime(sec: i64) -> String {
    if sec < 90 {
        format!("{sec} s")
    } else if sec < 5400 {
        format!("{} min", sec / 60)
    } else if sec < 172_800 {
        format!("{} h {} min", sec / 3600, sec % 3600 / 60)
    } else {
        format!("{} days", sec / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notifies_each_change_applied_automatically_once_across_restarts() {
        let s = |times: &[i64], kind: &str| Status {
            data: Some(json!({ "health": "ok", "autoApplied": { "count": times.len(), "times": times, "last": { "title": "Harbor", "kind": kind } } })),
            error: None,
        };
        let file = std::env::temp_dir().join(format!("aw-tray-announced-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let mut a = Announced::load(&file);
        assert_eq!(a.take(&s(&[], "held")), None);
        let (title, _) = a.take(&s(&[300, 200, 100], "held")).unwrap();
        assert_eq!(title, "Agent Wiki applied 3 changes", "applied before the tray's first poll: still announced");
        assert_eq!(a.take(&s(&[300, 200, 100], "held")), None, "once");
        let mut after_restart = Announced::load(&file);
        assert_eq!(after_restart.take(&s(&[300, 200, 100], "held")), None, "not again after a restart");
        let (title, text) = after_restart.take(&s(&[400, 300, 200, 100], "lint")).unwrap();
        assert_eq!(title, "Agent Wiki applied a cleanup");
        assert!(text.starts_with("Harbor. Open Agent Wiki > Inbox"));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn state_tooltip_uptime() {
        let s = Status { data: Some(json!({ "health": "ok", "version": "1.3.0", "queue": { "pending": 2, "dead": 0 } })), error: None };
        assert_eq!(s.state(), "healthy");
        assert_eq!(s.tooltip(), "Agent Wiki 1.3.0: OK · 2 queued");
        let d = Status { data: Some(json!({ "health": "degraded", "version": "1.3.0", "queue": { "pending": 0, "dead": 3 } })), error: None };
        assert_eq!(d.tooltip(), "Agent Wiki 1.3.0: needs attention · 0 queued · 3 failed");
        assert_eq!(Status::default().state(), "down");
        assert_eq!(uptime(45), "45 s");
        assert_eq!(uptime(3600), "60 min");
        assert_eq!(uptime(7300), "2 h 1 min");
        assert_eq!(uptime(200_000), "2 days");
    }
}
