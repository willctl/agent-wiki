//! Page and note frontmatter: the small YAML subset Agent Wiki writes and reads (src/wiki.mjs).

use crate::text::to_lf;
use regex::Regex;
use std::sync::LazyLock;

/// A frontmatter value: a string or a list of strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Val {
    Str(String),
    List(Vec<String>),
}

impl Val {
    pub fn as_str(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            Val::List(l) => l.join(","),
        }
    }
}

impl From<&str> for Val {
    fn from(s: &str) -> Self {
        Val::Str(s.to_string())
    }
}
impl From<String> for Val {
    fn from(s: String) -> Self {
        Val::Str(s)
    }
}
impl From<Vec<String>> for Val {
    fn from(l: Vec<String>) -> Self {
        Val::List(l)
    }
}

/// Frontmatter in file order (keys are unique; a repeated key replaces the earlier value in place).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta(pub Vec<(String, Val)>);

impl Meta {
    pub fn get(&self, k: &str) -> Option<&Val> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v)
    }
    /// The value as a string ("" when missing; lists joined with commas, as JS String(array) does).
    pub fn str(&self, k: &str) -> String {
        self.get(k).map(|v| v.as_str()).unwrap_or_default()
    }
    pub fn has(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
    pub fn set(&mut self, k: &str, v: impl Into<Val>) {
        let v = v.into();
        match self.0.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1 = v,
            None => self.0.push((k.to_string(), v)),
        }
    }
    pub fn remove(&mut self, k: &str) {
        self.0.retain(|(key, _)| key != k);
    }
}

static PLAIN_SAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9 _./()&+,-]*$").unwrap());
static YAML_RESERVED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:true|false|yes|no|on|off|null|y|n)$").unwrap());
static NUMERIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[-+]?(?:[0-9][0-9_]*)?(?:\.[0-9]+)?(?:e[-+]?[0-9]+)?$").unwrap());
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?(?:Z|[+-][0-9]{2}:[0-9]{2})$").unwrap());
static KV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_-]+):(?:\s+(.*))?$").unwrap());
static ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*-\s+(.*)$").unwrap());
static CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:---|\.\.\.)\s*$").unwrap());
static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+#.*$").unwrap());

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

/// A YAML value that is either a safe plain scalar or a JSON string/array (also valid YAML).
fn yaml_value(v: &Val) -> String {
    match v {
        Val::List(l) => serde_json::to_string(l).unwrap_or_else(|_| "[]".into()),
        Val::Str(s) => {
            if TIMESTAMP.is_match(s) {
                return s.clone();
            }
            if !s.is_empty() && PLAIN_SAFE.is_match(s) && !YAML_RESERVED.is_match(s) && !NUMERIC.is_match(s) && !s.ends_with(char::is_whitespace) {
                return s.clone();
            }
            json_str(s)
        }
    }
}

fn strip_quotes(s: &str, q: char) -> String {
    let s = s.strip_prefix(q).unwrap_or(s);
    s.strip_suffix(q).unwrap_or(s).to_string()
}

fn parse_value(raw: &str) -> Val {
    let s = raw.trim();
    if s.is_empty() {
        return Val::Str(String::new());
    }
    if s.starts_with('"') {
        return match serde_json::from_str::<String>(s) {
            Ok(v) => Val::Str(v),
            Err(_) => Val::Str(strip_quotes(s, '"')),
        };
    }
    if s.starts_with('\'') {
        return Val::Str(strip_quotes(s, '\'').replace("''", "'"));
    }
    if s.starts_with('[') {
        if let Ok(serde_json::Value::Array(a)) = serde_json::from_str::<serde_json::Value>(s) {
            return Val::List(a.iter().map(js_string).collect());
        }
        // A YAML flow sequence such as [a, 'b c'] written by hand.
        let inner = s.strip_prefix('[').unwrap_or(s);
        let inner = inner.strip_suffix(']').unwrap_or(inner);
        return Val::List(
            inner
                .split(',')
                .map(|x| {
                    let t = x.trim();
                    let t = t.strip_prefix(['"', '\'']).unwrap_or(t);
                    t.strip_suffix(['"', '\'']).unwrap_or(t).to_string()
                })
                .filter(|x| !x.is_empty())
                .collect(),
        );
    }
    Val::Str(COMMENT.replace(s, "").into_owned())
}

/// String(x) for a JSON value, as JavaScript renders it.
pub fn js_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Array(a) => a.iter().map(js_string).collect::<Vec<_>>().join(","),
        serde_json::Value::Object(_) => "[object Object]".into(),
        other => other.to_string(),
    }
}

/// Splits a page or note into its frontmatter and body.
pub fn parse(text: &str) -> (Meta, String) {
    let t = to_lf(text);
    let t = t.strip_prefix('\u{feff}').unwrap_or(&t).to_string();
    let lines: Vec<&str> = t.split('\n').collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return (Meta::default(), t);
    }
    let Some(close) = lines.iter().skip(1).position(|l| CLOSE.is_match(l)).map(|i| i + 1) else {
        return (Meta::default(), t);
    };
    let mut meta = Meta::default();
    let mut last: Option<String> = None;
    for line in &lines[1..close] {
        if let Some(c) = KV.captures(line) {
            let key = c[1].to_string();
            meta.set(&key, parse_value(c.get(2).map_or("", |m| m.as_str())));
            last = Some(key);
            continue;
        }
        if let (Some(c), Some(key)) = (ITEM.captures(line), &last) {
            let item = match parse_value(&c[1]) {
                Val::Str(s) => s,
                Val::List(l) => l.join(","),
            };
            match meta.get(key) {
                Some(Val::List(l)) => {
                    let mut l = l.clone();
                    l.push(item);
                    meta.set(key, Val::List(l));
                }
                _ => meta.set(key, Val::List(vec![item])),
            }
        }
    }
    let body = lines[close + 1..].join("\n");
    let body = body.trim_start_matches('\n').to_string();
    (meta, body)
}

const ORDER: [&str; 8] = ["title", "type", "summary", "tags", "aliases", "created", "updated", "updated_by"];

/// Writes frontmatter (known keys first, in a fixed order) and the body.
pub fn serialize(meta: &Meta, body: &str) -> String {
    let mut keys: Vec<&str> = ORDER.iter().copied().filter(|k| meta.has(k)).collect();
    keys.extend(meta.0.iter().map(|(k, _)| k.as_str()).filter(|k| !ORDER.contains(k)));
    let fm: Vec<String> = keys.iter().map(|k| format!("{k}: {}", yaml_value(meta.get(k).unwrap()))).collect();
    format!("---\n{}\n---\n\n{}\n", fm.join("\n"), to_lf(body).trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut m = Meta::default();
        m.set("title", "Agent Wiki");
        m.set("type", "project");
        m.set("summary", "Shared AI memory: Windows service");
        m.set("tags", vec!["a".to_string(), "b c".to_string()]);
        m.set("created", "2026-10-01T14:46:56-05:00");
        m.set("note", "yes");
        let s = serialize(&m, "# Agent Wiki\n\nBody");
        assert!(s.starts_with(
            "---\ntitle: Agent Wiki\ntype: project\nsummary: \"Shared AI memory: Windows service\"\ntags: [\"a\",\"b c\"]\ncreated: 2026-10-01T14:46:56-05:00\nnote: \"yes\"\n---\n\n# Agent Wiki"
        ));
        let (back, body) = parse(&s);
        assert_eq!(back, m);
        assert_eq!(body, "# Agent Wiki\n\nBody\n");
        let (h, _) = parse("---\ntags:\n  - x\n  - 'y z'\nflow: [a, 'b c']\n---\nbody");
        assert_eq!(h.get("tags"), Some(&Val::List(vec!["x".into(), "y z".into()])));
        assert_eq!(h.get("flow"), Some(&Val::List(vec!["a".into(), "b c".into()])));
    }
}
