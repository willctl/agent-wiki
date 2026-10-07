//! The isolated `codex exec` both model users share: the curator (an edit plan) and Ask (an answer
//! found with the wiki's read-only tools), as src/codex.mjs. It runs as the user, in the curator's own
//! CODEX_HOME, with the user's config, rules, plugins, hooks, apps, memories, shell and web search off,
//! and returns the final message parsed as JSON (--output-schema). It never reads Codex's credential
//! files.

use crate::reqlog::clip_str;
use crate::text::{now_ms, random_hex};
use regex::Regex;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

/// What a model run needs: which model, how hard it thinks, which codex, where its home is.
#[derive(Clone, Debug)]
pub struct ModelCfg {
    pub model: String,
    pub reasoning_effort: String,
    pub codex_path: String,
    pub codex_home: PathBuf,
    pub timeout_seconds: f64,
}

/// kind: signed_out | rate_limited | config | timeout | model_error | bad_output | aborted | interrupted
#[derive(Clone, Debug)]
pub struct ModelError {
    pub kind: String,
    pub message: String,
    pub ms: i64,
    pub retry_at: Option<i64>,
}

impl ModelError {
    pub fn new(kind: &str, message: impl Into<String>, ms: i64) -> Self {
        ModelError { kind: kind.into(), message: message.into(), ms, retry_at: None }
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

const FEATURES_OFF: [&str; 16] = [
    "plugins",
    "apps",
    "hooks",
    "memories",
    "multi_agent",
    "goals",
    "shell_tool",
    "unified_exec",
    "view_image",
    "image_generation",
    "skill_search",
    "tool_suggest",
    "browser_use",
    "computer_use",
    "in_app_browser",
    "sleep_tool",
];

/// The isolated invocation. `extra`: more arguments (Ask adds its MCP server and its reasoning effort).
pub fn codex_args(cfg: &ModelCfg, work_dir: &Path, schema_file: &Path, out_file: &Path, extra: &[String]) -> Vec<String> {
    let mut a: Vec<String> =
        ["exec", "--ephemeral", "--skip-git-repo-check", "--ignore-user-config", "--ignore-rules", "--strict-config", "--sandbox", "read-only", "-C"].iter().map(|s| s.to_string()).collect();
    a.push(work_dir.to_string_lossy().into_owned());
    a.push("-m".into());
    a.push(cfg.model.clone());
    let mut c = |kv: String| {
        a.push("-c".into());
        a.push(kv);
    };
    c(format!("model_reasoning_effort=\"{}\"", cfg.reasoning_effort));
    for kv in [
        "model_reasoning_summary=\"none\"",
        "model_verbosity=\"low\"",
        "forced_login_method=\"chatgpt\"",
        "web_search=\"disabled\"",
        "project_doc_max_bytes=0",
        "agents.enabled=false",
        "skills.include_instructions=false",
        "skills.bundled.enabled=false",
        "include_permissions_instructions=false",
        "include_collaboration_mode_instructions=false",
        "include_environment_context=false",
        "include_apps_instructions=false",
        "history.persistence=\"none\"",
    ] {
        c(kv.to_string());
    }
    a.extend(extra.iter().cloned());
    for f in FEATURES_OFF {
        a.push("--disable".into());
        a.push(f.into());
    }
    a.extend(["--output-schema".to_string(), schema_file.to_string_lossy().into_owned(), "-o".into(), out_file.to_string_lossy().into_owned(), "--json".into(), "-".into()]);
    a
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// `node`, for a codex that is a JavaScript entry point (npm's `codex` on Linux and macOS, or a test
/// double): AGENT_WIKI_NODE, a node next to the codex link (npm puts both in one bin folder), PATH,
/// then the usual install places.
fn find_node(codex_link: &Path) -> PathBuf {
    if let Some(n) = std::env::var_os("AGENT_WIKI_NODE").filter(|n| !n.is_empty()) {
        return PathBuf::from(n);
    }
    let mut candidates: Vec<PathBuf> = vec![];
    if let Some(dir) = codex_link.parent() {
        candidates.push(dir.join(exe("node")));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(exe("node"))));
    }
    if cfg!(windows) {
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(pf).join("nodejs").join("node.exe"));
        }
    } else {
        candidates.extend(["/usr/local/bin/node", "/opt/homebrew/bin/node", "/usr/bin/node"].map(PathBuf::from));
    }
    candidates.into_iter().find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from(exe("node")))
}

/// How to start codex: (program, leading args). A JavaScript entry point runs with node.
pub fn codex_command(codex_path: &str) -> (PathBuf, Vec<String>) {
    let link = PathBuf::from(codex_path);
    let target = std::fs::canonicalize(&link).unwrap_or_else(|_| link.clone());
    let t = target.to_string_lossy();
    let shown = crate::text::display_path(t.strip_prefix(r"\\?\").unwrap_or(&t));
    static JS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.[cm]?js$").unwrap());
    if JS.is_match(&shown) {
        return (find_node(&link), vec![shown]);
    }
    if !cfg!(windows) {
        let mut head = [0u8; 128];
        if let Ok(n) = std::fs::File::open(&target).and_then(|mut f| f.read(&mut head)) {
            let line = String::from_utf8_lossy(&head[..n]).split('\n').next().unwrap_or("").to_string();
            static NODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#!.*\bnode\b").unwrap());
            if NODE.is_match(&line) {
                return (find_node(&link), vec![shown]);
            }
        }
    }
    (link, vec![])
}

fn command(cfg: &ModelCfg, args: &[String]) -> Command {
    let (program, lead) = codex_command(&cfg.codex_path);
    let mut cmd = Command::new(program);
    cmd.args(lead).args(args);
    cmd.env("CODEX_HOME", &cfg.codex_home).env_remove("CODEX_API_KEY").env_remove("OPENAI_API_KEY");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // its own group, so kill_tree stops codex and what it started
    }
    cmd
}

/// Stops a codex child and everything it started: taskkill /T on Windows, the process group elsewhere.
pub fn kill_tree(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let ok = Command::new("taskkill.exe")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            let _ = child.kill();
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: plain syscall on our own child's process group.
        let r = unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
        if r != 0 {
            let _ = child.kill();
        }
    }
}

pub fn classify(text: &str) -> &'static str {
    static SIGNED_OUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)not logged in|log ?in again|sign in again|could not be refreshed|unauthori[sz]ed|\b401\b|login required|no auth").unwrap());
    static RATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)usage limit|rate limit|\b429\b|quota exceeded|too many requests").unwrap());
    static MODEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)model[^\n]{0,80}(is not supported|not supported when|does not exist|not found|not available|is unavailable|unsupported)|(unknown|invalid|unsupported) model|unsupported value[^\n]{0,80}(reasoning|effort)|is not supported with the [^\n]{0,60}model").unwrap()
    });
    static CONFIG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)unknown configuration field|unknown feature|unexpected argument|error parsing -c|invalid value for").unwrap());
    if SIGNED_OUT.is_match(text) {
        "signed_out"
    } else if RATE.is_match(text) {
        "rate_limited"
    } else if MODEL.is_match(text) {
        "model_unavailable"
    } else if CONFIG.is_match(text) {
        "config"
    } else {
        "model_error"
    }
}

/// `codex login status` for the curator's CODEX_HOME: (signed in, detail).
pub fn login_status(cfg: &ModelCfg) -> (bool, String) {
    let mut cmd = command(cfg, &["login".into(), "status".into()]);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, e.to_string()),
    };
    let out = Arc::new(Mutex::new(String::new()));
    let readers: Vec<_> = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
        .into_iter()
        .flatten()
        .map(|mut r| {
            let out = out.clone();
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = r.read_to_string(&mut s);
                out.lock().unwrap().push_str(&s);
            })
        })
        .collect();
    let t0 = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) if t0.elapsed() > Duration::from_secs(30) => {
                kill_tree(&mut child);
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => break None,
        }
    };
    for r in readers {
        let _ = r.join();
    }
    let text = out.lock().unwrap().clone();
    static IN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)logged in").unwrap());
    static OUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)not logged in").unwrap());
    (code == Some(0) && IN.is_match(&text) && !OUT.is_match(&text), clip_str(&text, 200))
}

pub struct RunOpts<'a> {
    pub run_dir: &'a Path,
    pub abort: &'a AtomicBool,
    pub schema: &'a Value,
    pub schema_name: &'a str,
    pub extra: Vec<String>,
    pub on_event: Option<&'a mut dyn FnMut(&Value)>,
}

pub struct ModelOut {
    pub output: Value,
    pub usage: Value,
    pub ms: i64,
}

/// Runs one isolated `codex exec`: the final message as JSON, the token usage, how long it took.
pub fn run_model(cfg: &ModelCfg, prompt: &str, mut o: RunOpts<'_>) -> Result<ModelOut, ModelError> {
    let io = |e: std::io::Error| ModelError::new("model_error", e.to_string(), 0);
    let work_dir = o.run_dir.join("work");
    std::fs::create_dir_all(&work_dir).map_err(io)?;
    // Concurrent Ask requests share run_dir. Never truncate a schema another child is reading.
    let run_id = format!("{}-{}", std::process::id(), random_hex(16));
    let schema_file = o.run_dir.join(format!("{}-{run_id}.schema.json", o.schema_name));
    crate::wiki::write_synced(&schema_file, &o.schema.to_string(), true).map_err(io)?;
    let out_file = o.run_dir.join(format!("out-{run_id}.json"));
    let t0 = now_ms();
    let r = run(cfg, prompt, &mut o, &work_dir, &schema_file, &out_file, t0);
    let _ = std::fs::remove_file(&out_file);
    let _ = std::fs::remove_file(&schema_file);
    r
}

fn run(cfg: &ModelCfg, prompt: &str, o: &mut RunOpts<'_>, work_dir: &Path, schema_file: &Path, out_file: &Path, t0: i64) -> Result<ModelOut, ModelError> {
    let mut cmd = command(cfg, &codex_args(cfg, work_dir, schema_file, out_file, &o.extra));
    cmd.current_dir(work_dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut events: Vec<Value> = vec![];
    let stderr = Arc::new(Mutex::new(String::new()));
    let (code, timed_out) = match cmd.spawn() {
        Err(e) => {
            stderr.lock().unwrap().push_str(&format!("\n{e}"));
            (if e.kind() == std::io::ErrorKind::NotFound { -2 } else { -1 }, false)
        }
        Ok(mut child) => {
            let (tx, rx) = mpsc::channel::<Value>();
            let out = child.stdout.take().unwrap();
            let reader = std::thread::spawn(move || {
                for line in BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(e) = serde_json::from_str::<Value>(line.trim()) {
                        let _ = tx.send(e);
                    }
                }
            });
            let mut err = child.stderr.take().unwrap();
            let err_buf = stderr.clone();
            let err_reader = std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                while let Ok(n) = err.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut b = err_buf.lock().unwrap();
                    if b.len() < 64_000 {
                        b.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
            });
            let mut stdin = child.stdin.take().unwrap();
            let prompt = prompt.to_string();
            let writer = std::thread::spawn(move || {
                let _ = stdin.write_all(prompt.as_bytes());
            });
            let deadline = Instant::now() + Duration::from_secs_f64(cfg.timeout_seconds.max(1.0));
            let mut timed_out = false;
            let mut killed = false;
            let mut stdout_done = false;
            let status = loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(e) => {
                        if let Some(f) = o.on_event.as_mut() {
                            // a progress listener must not break the run
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&e)));
                        }
                        events.push(e);
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => stdout_done = true,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if !killed && (o.abort.load(Ordering::SeqCst) || Instant::now() > deadline) {
                    timed_out = !o.abort.load(Ordering::SeqCst);
                    killed = true;
                    kill_tree(&mut child);
                }
                if stdout_done && let Ok(Some(s)) = child.try_wait() {
                    break s;
                }
                if stdout_done {
                    std::thread::sleep(Duration::from_millis(20));
                }
            };
            let _ = reader.join();
            let _ = err_reader.join();
            let _ = writer.join();
            (status.code().unwrap_or(-1), timed_out)
        }
    };
    let ms = now_ms() - t0;
    let usage = events.iter().find(|e| e["type"] == "turn.completed").map(|e| e["usage"].clone()).unwrap_or(Value::Null);
    if o.abort.load(Ordering::SeqCst) {
        return Err(ModelError::new("aborted", "stopped while the model was running", ms));
    }
    if timed_out {
        return Err(ModelError::new("timeout", format!("the model did not answer within {}s", crate::wiki::js_number(cfg.timeout_seconds)), ms));
    }
    let failures: Vec<String> =
        events.iter().filter(|e| e["type"] == "turn.failed" || e["type"] == "error").map(|e| e["error"]["message"].as_str().or_else(|| e["message"].as_str()).unwrap_or("").to_string()).collect();
    if code != 0 {
        let stderr = stderr.lock().unwrap().clone();
        let text = failures.iter().cloned().chain([stderr.clone()]).collect::<Vec<_>>().join("\n");
        let kind = if code == -2 { "config" } else { classify(&text) };
        let last_line = stderr.trim().split('\n').next_back().unwrap_or("").to_string();
        let msg = failures.last().filter(|s| !s.is_empty()).cloned().or_else(|| (!last_line.is_empty()).then_some(last_line)).unwrap_or_else(|| format!("codex exited with code {code}"));
        let mut e = ModelError::new(kind, clip_str(&msg, 300), ms);
        e.retry_at = retry_at(&text);
        return Err(e);
    }
    let out = std::fs::read_to_string(out_file).unwrap_or_default();
    let last = if out.trim().is_empty() {
        events.iter().rev().find(|e| e["item"]["type"] == "agent_message").and_then(|e| e["item"]["text"].as_str()).unwrap_or("").to_string()
    } else {
        out.trim().to_string()
    };
    match serde_json::from_str::<Value>(&last) {
        Ok(output) => Ok(ModelOut { output, usage, ms }),
        Err(_) => Err(ModelError::new("bad_output", format!("the model did not return valid JSON ({} chars)", last.encode_utf16().count()), ms)),
    }
}

/// "try again at 3:45 PM" -> epoch ms, if the message says when.
pub fn retry_at(text: &str) -> Option<i64> {
    use chrono::{Local, TimeZone};
    static AT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)try again (?:at|after) ([^.\n]+)").unwrap());
    static HM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)([0-9]{1,2}):([0-9]{2})\s*(am|pm)?").unwrap());
    let when = AT.captures(text)?.get(1)?.as_str().trim().to_string();
    if let Some(t) = crate::text::parse_ms(&when).or_else(|| chrono::DateTime::parse_from_rfc2822(&when).ok().map(|d| d.timestamp_millis())) {
        return Some(t);
    }
    let c = HM.captures(&when)?;
    let mut h: u32 = c[1].parse().ok()?;
    let m: u32 = c[2].parse().ok()?;
    match c.get(3).map(|a| a.as_str().to_lowercase()).as_deref() {
        Some("pm") if h < 12 => h += 12,
        Some("am") if h == 12 => h = 0,
        _ => {}
    }
    let today = Local::now().date_naive();
    let mut t = Local.from_local_datetime(&today.and_hms_opt(h, m, 0)?).earliest()?;
    if t.timestamp_millis() < now_ms() {
        t += chrono::Duration::days(1);
    }
    Some(t.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_failures() {
        assert_eq!(classify("{\"detail\":\"The 'gpt-x' model is not supported when using Codex with a ChatGPT account.\"}"), "model_unavailable");
        assert_eq!(classify("Unsupported value: 'ultra' is not supported with the 'gpt-5.5' model. Supported values are: 'low', 'medium'."), "model_unavailable");
        assert_eq!(classify("The model `nope` does not exist or you do not have access to it."), "model_unavailable");
        assert_eq!(classify("Error: 401 Unauthorized"), "signed_out");
        assert_eq!(classify("You've hit your usage limit."), "rate_limited");
        assert_eq!(classify("error: unexpected argument '--foo'"), "config");
        assert_eq!(classify("something else"), "model_error");
    }

    #[test]
    fn retry_time() {
        let t = retry_at("Usage limit reached. Try again at 3:45 PM.").unwrap();
        assert!(t > now_ms() && t - now_ms() <= 86_400_000);
        assert!(retry_at("no time here").is_none());
    }

    #[test]
    fn isolated_args() {
        let cfg = ModelCfg { model: "m".into(), reasoning_effort: "low".into(), codex_path: "codex".into(), codex_home: "h".into(), timeout_seconds: 1.0 };
        let a = codex_args(&cfg, Path::new("w"), Path::new("s"), Path::new("o"), &[]).join(" ");
        for f in ["--ephemeral", "--ignore-user-config", "--strict-config", "--sandbox read-only", "-m m", "--disable plugins", "model_reasoning_effort=\"low\""] {
            assert!(a.contains(f), "{f}");
        }
        assert!(a.ends_with("--json -"));
    }
}
