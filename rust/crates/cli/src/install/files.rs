//! Safe edits of other apps' configuration: every existing file is backed up once as
//! <file>.bak-agent-wiki, merged (never replaced), keeps its line endings, and is written atomically.
//! Also the managed instruction block, the Codex approval setting, Claude Code's allow rules, and the
//! plugin rendering.

use super::places::fwd;
use super::{InstallError, info};
use regex::Regex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub type Result<T> = std::result::Result<T, InstallError>;

pub fn read_text(file: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(file) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(InstallError(format!("cannot read {}: {e}", fwd(file)))),
    }
}

fn atomic_write(file: &Path, content: &str) -> Result<()> {
    let dir = file.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| InstallError(format!("cannot create {}: {e}", fwd(dir))))?;
    let tmp = PathBuf::from(format!("{}.agent-wiki-{}.tmp", file.display(), std::process::id()));
    std::fs::write(&tmp, content).map_err(|e| InstallError(format!("cannot write {}: {e}", fwd(&tmp))))?;
    let mut i = 0u64;
    loop {
        match std::fs::rename(&tmp, file) {
            Ok(()) => return Ok(()),
            Err(e) if i < 20 && (e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5) | Some(32))) => {
                std::thread::sleep(std::time::Duration::from_millis(50 + i * 50));
                i += 1;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(InstallError(format!("cannot write {}: {e}", fwd(file))));
            }
        }
    }
}

/// Copies an existing file to <file>.bak-agent-wiki once. The first backup is never overwritten.
pub fn backup_once(file: &Path) -> Result<Option<PathBuf>> {
    if !file.exists() {
        return Ok(None);
    }
    let bak = PathBuf::from(format!("{}.bak-agent-wiki", file.display()));
    if !bak.exists() {
        std::fs::copy(file, &bak).map_err(|e| InstallError(format!("cannot back up {}: {e}", fwd(file))))?;
        info(&format!("backed up {} -> {}", fwd(file), bak.file_name().unwrap_or_default().to_string_lossy()));
    }
    Ok(Some(bak))
}

/// Read-modify-write of a config file. `transform(text)` gets LF text (None: no file) and returns the
/// new LF text, or None for no change. Existing files are backed up once and keep their line endings.
pub fn edit_file(file: &Path, transform: impl FnOnce(Option<&str>) -> Result<Option<String>>) -> Result<bool> {
    let cur = read_text(file)?;
    let lf = cur.as_ref().map(|t| t.replace("\r\n", "\n"));
    let Some(next) = transform(lf.as_deref())? else { return Ok(false) };
    let crlf = cur.as_ref().is_some_and(|t| t.contains("\r\n"));
    let next = next.replace("\r\n", "\n").replace('\r', "\n");
    let out = if crlf { next.replace('\n', "\r\n") } else { next };
    if cur.as_deref() == Some(out.as_str()) {
        return Ok(false);
    }
    if cur.is_some() {
        backup_once(file)?;
    }
    atomic_write(file, &out)?;
    Ok(true)
}

pub fn write_lf(file: &Path, text: &str) -> Result<()> {
    atomic_write(file, &text.replace("\r\n", "\n"))
}

pub fn read_json(file: &Path) -> Result<Option<Value>> {
    let Some(t) = read_text(file)? else { return Ok(None) };
    serde_json::from_str(t.trim_start_matches('\u{feff}')).map(Some).map_err(|e| InstallError(format!("{} is not valid JSON ({e}); fix it or restore its backup, then re-run.", fwd(file))))
}

/// JSON.stringify(v, null, 2) + "\n".
pub fn to_json(v: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(v).unwrap_or_default())
}

// ---------------------------------------------------------------- managed Markdown block

pub const BLOCK_START: &str = "<!-- AGENT-WIKI:START (managed by the agent-wiki installer; replaced on re-install) -->";
pub const BLOCK_END: &str = "<!-- AGENT-WIKI:END -->";
static BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n*<!-- AGENT-WIKI:START[^\n]*-->(?s:.*?)<!-- AGENT-WIKI:END -->\n*").unwrap());

fn split_around_block(text: &str) -> Option<(String, String)> {
    let m = BLOCK_RE.find(text)?;
    Some((text[..m.start()].trim_end().to_string(), text[m.end()..].trim().to_string()))
}

/// Replaces the managed block in place, or appends it. Everything else is kept.
pub fn upsert_block(text: Option<&str>, inner: &str) -> String {
    let block = format!("{BLOCK_START}\n{}\n{BLOCK_END}", inner.trim());
    let cur = text.unwrap_or("");
    let (before, after) = split_around_block(cur).unwrap_or_else(|| (cur.trim_end().to_string(), String::new()));
    let parts: Vec<&str> = [before.as_str(), block.as_str(), after.as_str()].into_iter().filter(|s| !s.trim().is_empty()).collect();
    format!("{}\n", parts.join("\n\n"))
}

/// Removes the managed block: None when there is none, "" when nothing else remains.
pub fn remove_block(text: Option<&str>) -> Option<String> {
    let (before, after) = split_around_block(text?)?;
    let rest: Vec<&str> = [before.as_str(), after.as_str()].into_iter().filter(|s| !s.trim().is_empty()).collect();
    Some(if rest.is_empty() { String::new() } else { format!("{}\n", rest.join("\n\n")) })
}

// ---------------------------------------------------------------- Codex config.toml

pub const SERVER: &str = "agent-wiki";
pub const TOML_COMMENT: &str = "# agent-wiki: run the wiki tools without confirmation (managed by the agent-wiki installer)";

fn approval_header(plugin_id: &str) -> String {
    format!("[plugins.\"{plugin_id}\".mcp_servers.{SERVER}]")
}

fn header_re(plugin_id: &str) -> Regex {
    Regex::new(&format!(r#"^\[\s*plugins\s*\.\s*"{}"\s*\.\s*mcp_servers\s*\.\s*"?{}"?\s*\]\s*(#.*)?$"#, regex::escape(plugin_id), regex::escape(SERVER))).unwrap()
}

fn parse_toml(text: &str) -> Result<toml::Table> {
    text.parse::<toml::Table>().map_err(|e| InstallError(format!("Refusing to write config.toml: the result would not be valid TOML ({}).", e.to_string().lines().next().unwrap_or(""))))
}

/// plugins.<id>.mcp_servers.agent-wiki.default_tools_approval_mode, if set.
pub fn toml_approval(text: &str, plugin_id: &str) -> Option<String> {
    let t = text.parse::<toml::Table>().ok()?;
    t.get("plugins")?.get(plugin_id)?.get("mcp_servers")?.get(SERVER)?.get("default_tools_approval_mode")?.as_str().map(String::from)
}

/// Ensures default_tools_approval_mode = "approve" in the plugin's server table: the new text, or None.
pub fn toml_set_approval(text: Option<&str>, plugin_id: &str) -> Result<Option<String>> {
    let cur = text.unwrap_or("");
    parse_toml(cur)?;
    if toml_approval(cur, plugin_id).as_deref() == Some("approve") {
        return Ok(None);
    }
    let mut lines: Vec<String> = cur.split('\n').map(String::from).collect();
    let re = header_re(plugin_id);
    let out = match lines.iter().position(|l| re.is_match(l.trim())) {
        Some(h) => {
            let mut end = h + 1;
            while end < lines.len() && !lines[end].trim_start().starts_with('[') {
                end += 1;
            }
            static KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*default_tools_approval_mode\s*=").unwrap());
            match (h + 1..end).find(|i| KEY.is_match(&lines[*i])) {
                Some(k) => lines[k] = "default_tools_approval_mode = \"approve\"".into(),
                None => lines.insert(h + 1, "default_tools_approval_mode = \"approve\"".into()),
            }
            lines.join("\n")
        }
        None => format!("{}\n\n{TOML_COMMENT}\n{}\ndefault_tools_approval_mode = \"approve\"\n", cur.trim_end(), approval_header(plugin_id)),
    };
    parse_toml(&out)?;
    if toml_approval(&out, plugin_id).as_deref() != Some("approve") {
        return Err(InstallError("config.toml merge did not produce the expected approval setting.".into()));
    }
    Ok(Some(out))
}

/// Removes the approval table (and our comment) for the plugin: the new text, or None.
pub fn toml_remove_approval(text: Option<&str>, plugin_id: &str) -> Result<Option<String>> {
    let Some(text) = text else { return Ok(None) };
    let mut lines: Vec<&str> = text.split('\n').collect();
    let re = header_re(plugin_id);
    let Some(h) = lines.iter().position(|l| re.is_match(l.trim())) else { return Ok(None) };
    let start = if h > 0 && lines[h - 1].trim() == TOML_COMMENT { h - 1 } else { h };
    let mut end = h + 1;
    while end < lines.len() && !lines[end].trim_start().starts_with('[') {
        end += 1;
    }
    // keep trailing comments and blank lines that belong to the next table
    while end - 1 > h && (lines[end - 1].trim().is_empty() || lines[end - 1].trim_start().starts_with('#')) {
        end -= 1;
    }
    lines.drain(start..end);
    static GAPS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());
    let out = format!("{}\n", GAPS.replace_all(&lines.join("\n"), "\n\n").trim_end());
    parse_toml(&out)?;
    Ok(Some(out))
}

// ---------------------------------------------------------------- Claude Code settings.json

/// Rules that let Claude Code call the wiki tools without asking: the plugin's server and a plain
/// `agent-wiki` server (the Claude desktop entry, which desktop Code sessions also attach).
pub const CLAUDE_ALLOW: [&str; 2] = ["mcp__plugin_agent-wiki_agent-wiki", "mcp__agent-wiki"];

pub fn claude_allow_set(text: Option<&str>) -> Result<Option<String>> {
    let mut s: Value = match text {
        Some(t) => serde_json::from_str(t).map_err(|e| InstallError(format!("Claude Code settings.json is not valid JSON ({e})")))?,
        None => json!({}),
    };
    if !s.is_object() {
        s = json!({});
    }
    let perms = s.as_object_mut().unwrap().entry("permissions").or_insert_with(|| json!({}));
    if !perms.is_object() {
        *perms = json!({});
    }
    let p = perms.as_object_mut().unwrap();
    let had_list = p.get("allow").is_some_and(Value::is_array);
    let mut allow: Vec<Value> = p.get("allow").and_then(Value::as_array).cloned().unwrap_or_default();
    let missing: Vec<&str> = CLAUDE_ALLOW.iter().copied().filter(|r| !allow.iter().any(|a| a == r)).collect();
    if missing.is_empty() && had_list {
        return Ok(None);
    }
    allow.extend(missing.into_iter().map(|r| json!(r)));
    p.insert("allow".into(), Value::Array(allow));
    Ok(Some(to_json(&s)))
}

pub fn claude_allow_remove(text: Option<&str>) -> Result<Option<String>> {
    let Some(t) = text else { return Ok(None) };
    let mut s: Value = serde_json::from_str(t).map_err(|e| InstallError(format!("Claude Code settings.json is not valid JSON ({e})")))?;
    let Some(perms) = s.get_mut("permissions").and_then(Value::as_object_mut) else { return Ok(None) };
    let Some(allow) = perms.get("allow").and_then(Value::as_array).cloned() else { return Ok(None) };
    if !allow.iter().any(|a| CLAUDE_ALLOW.iter().any(|r| a == r)) {
        return Ok(None);
    }
    let kept: Vec<Value> = allow.into_iter().filter(|a| !CLAUDE_ALLOW.iter().any(|r| a == r)).collect();
    if kept.is_empty() {
        perms.remove("allow");
    } else {
        perms.insert("allow".into(), Value::Array(kept));
    }
    if perms.is_empty() {
        s.as_object_mut().unwrap().remove("permissions");
    }
    Ok(Some(to_json(&s)))
}

// ---------------------------------------------------------------- the plugin

/// plugin/ in the repository, embedded: (path, text).
pub const PLUGIN_FILES: [(&str, &str); 5] = [
    (".claude-plugin/plugin.json", include_str!("../../../../../plugin/.claude-plugin/plugin.json")),
    (".codex-plugin/plugin.json", include_str!("../../../../../plugin/.codex-plugin/plugin.json")),
    (".mcp.json", include_str!("../../../../../plugin/.mcp.json")),
    ("hooks/hooks.json", include_str!("../../../../../plugin/hooks/hooks.json")),
    ("skills/agent-wiki/SKILL.md", include_str!("../../../../../plugin/skills/agent-wiki/SKILL.md")),
];

pub const POINTER: &str = include_str!("../../../../../protocol/POINTER.md");

fn deep_replace(v: &mut Value, map: &[(&str, String)]) {
    match v {
        Value::String(s) => {
            for (k, r) in map {
                *s = s.replace(k, r);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| deep_replace(x, map)),
        Value::Object(o) => o.values_mut().for_each(|x| deep_replace(x, map)),
        _ => {}
    }
}

/// Renders the plugin into `dest` with the placeholders filled in. Writes a staging folder, checks it
/// is exactly the source tree with nothing left unrendered, then swaps it in (so copying into an
/// existing folder can never nest .claude-plugin/.claude-plugin). `mcp_server` replaces the stdio
/// template in .mcp.json.
pub fn render_plugin(dest: &Path, map: &[(&str, String)], mcp_server: &Value) -> Result<Vec<String>> {
    let staging = PathBuf::from(format!("{}.staging-{}", dest.display(), std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let err = |e: std::io::Error| InstallError(format!("cannot render the plugin into {}: {e}", fwd(&staging)));
    for (rel, text) in PLUGIN_FILES {
        let text = text.replace("\r\n", "\n");
        let out = if rel == ".mcp.json" {
            to_json(&json!({ "mcpServers": { SERVER: mcp_server } }))
        } else if rel.ends_with(".json") {
            let mut v: Value = serde_json::from_str(&text).map_err(|e| InstallError(format!("plugin template {rel}: {e}")))?;
            deep_replace(&mut v, map);
            to_json(&v)
        } else {
            map.iter().fold(text, |s, (k, r)| s.replace(k, r))
        };
        static LEFT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__[A-Z_]+__").unwrap());
        if let Some(m) = LEFT.find(&out) {
            return Err(InstallError(format!("Unrendered placeholder {} in {rel}", m.as_str())));
        }
        let file = rel.split('/').fold(staging.clone(), |p, s| p.join(s));
        std::fs::create_dir_all(file.parent().unwrap()).map_err(err)?;
        std::fs::write(&file, &out).map_err(err)?;
    }
    let got = walk(&staging);
    let want: Vec<String> = {
        let mut w: Vec<String> = PLUGIN_FILES.iter().map(|(r, _)| r.to_string()).collect();
        w.sort();
        w
    };
    if got != want {
        return Err(InstallError(format!("Rendered plugin tree differs from the source: {}", got.join(", "))));
    }
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest.parent().unwrap_or(Path::new("."))).map_err(err)?;
    std::fs::rename(&staging, dest).map_err(err)?;
    Ok(want)
}

/// Files under `dir`, relative, with forward slashes, sorted.
pub fn walk(dir: &Path) -> Vec<String> {
    fn go(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                go(base, &p, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push(fwd(rel));
            }
        }
    }
    let mut out = vec![];
    go(dir, dir, &mut out);
    out.sort();
    out
}

/// Hash of a folder (relative paths and contents), to compare a rendered plugin with an app's cached copy.
pub fn tree_hash(dir: &Path) -> Option<String> {
    if !dir.exists() {
        return None;
    }
    let mut bytes = vec![];
    for rel in walk(dir) {
        bytes.extend_from_slice(rel.as_bytes());
        bytes.push(0);
        bytes.extend(std::fs::read(rel.split('/').fold(dir.to_path_buf(), |p, s| p.join(s))).unwrap_or_default());
        bytes.push(0);
    }
    Some(aw_core::text::sha256_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_block_append_replace_remove() {
        let user = "# My rules\n\nBe terse.\n";
        let once = upsert_block(Some(user), "v1 text");
        assert_eq!(once, format!("# My rules\n\nBe terse.\n\n{BLOCK_START}\nv1 text\n{BLOCK_END}\n"));
        let with_tail = format!("{once}\n## Later section\n");
        let twice = upsert_block(Some(&with_tail), "v2 text");
        assert_eq!(twice, format!("# My rules\n\nBe terse.\n\n{BLOCK_START}\nv2 text\n{BLOCK_END}\n\n## Later section\n"));
        assert_eq!(upsert_block(Some(&twice), "v2 text"), twice, "idempotent");
        assert_eq!(remove_block(Some(&twice)).unwrap(), "# My rules\n\nBe terse.\n\n## Later section\n");
        assert_eq!(remove_block(Some(&upsert_block(None, "only"))).unwrap(), "");
        assert_eq!(remove_block(Some(user)), None);
    }

    #[test]
    fn toml_approval_round_trips() {
        let id = "agent-wiki@personal";
        let base = "model = \"x\"\n\n[other]\na = 1\n";
        let set = toml_set_approval(Some(base), id).unwrap().unwrap();
        assert_eq!(toml_approval(&set, id).as_deref(), Some("approve"));
        assert!(set.contains(TOML_COMMENT));
        assert_eq!(toml_set_approval(Some(&set), id).unwrap(), None, "idempotent");
        let back = toml_remove_approval(Some(&set), id).unwrap().unwrap();
        assert_eq!(back, base);
        // A table codex wrote itself: the key is added inside it.
        let codex = format!("[plugins.\"{id}\".mcp_servers.agent-wiki]\nenabled = true\n");
        let merged = toml_set_approval(Some(&codex), id).unwrap().unwrap();
        assert_eq!(merged, format!("[plugins.\"{id}\".mcp_servers.agent-wiki]\ndefault_tools_approval_mode = \"approve\"\nenabled = true\n"));
        assert!(toml_set_approval(Some("not = = toml"), id).is_err());
    }

    #[test]
    fn claude_allow_rules() {
        let mine = "{\n  \"permissions\": {\n    \"allow\": [\n      \"Bash(ls)\"\n    ]\n  }\n}\n";
        let set = claude_allow_set(Some(mine)).unwrap().unwrap();
        let v: Value = serde_json::from_str(&set).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash(ls)", "mcp__plugin_agent-wiki_agent-wiki", "mcp__agent-wiki"]));
        assert_eq!(claude_allow_set(Some(&set)).unwrap(), None);
        assert_eq!(claude_allow_remove(Some(&set)).unwrap().unwrap(), mine);
        let only = claude_allow_set(None).unwrap().unwrap();
        assert_eq!(claude_allow_remove(Some(&only)).unwrap().unwrap(), "{}\n");
    }

    #[test]
    fn plugin_renders_twice_with_no_placeholders() {
        let dir = std::env::temp_dir().join(format!("aw-plugin-{}", std::process::id()));
        let dest = dir.join("plugins").join("agent-wiki");
        let map = vec![
            ("__VERSION__", "9.9.9".to_string()),
            ("__HOOK_COMMAND__", "agent-wiki hook".to_string()),
            ("__POINTER__", "pointer".to_string()),
            ("__PROTOCOL__", "protocol".to_string()),
            ("__NODE__", "unused".to_string()),
            ("__RUNTIME__", "unused".to_string()),
        ];
        let server = json!({ "command": "C:/x/agent-wiki.exe", "args": ["serve"] });
        let a = render_plugin(&dest, &map, &server).unwrap();
        let h1 = tree_hash(&dest);
        render_plugin(&dest, &map, &server).unwrap();
        assert_eq!(tree_hash(&dest), h1, "same tree twice");
        assert_eq!(walk(&dest), a);
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(dest.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["agent-wiki"]["args"], json!(["serve"]));
        assert!(std::fs::read_to_string(dest.join("hooks").join("hooks.json")).unwrap().contains("agent-wiki hook"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
