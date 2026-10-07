//! Running other programs, finding them, and talking to the installed server (health, MCP).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Ran {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    pub fn last_line(&self) -> String {
        format!("{}{}", self.stdout, self.stderr).trim().lines().last().unwrap_or("").to_string()
    }
}

/// Runs a program (no shell) and waits up to `timeout`, without a console window.
pub fn run_in(cmd: &Path, args: &[&str], dir: Option<&Path>, env: &[(&str, &str)], timeout: Duration) -> Ran {
    let mut c = Command::new(cmd);
    c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(d) = dir {
        c.current_dir(d);
    }
    for (k, v) in env {
        c.env(k, v);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return Ran { ok: false, code: None, stdout: String::new(), stderr: e.to_string() },
    };
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let to = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let te = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    let (stdout, stderr) = (to.join().unwrap_or_default(), te.join().unwrap_or_default());
    Ran { ok: status.is_some_and(|s| s.success()), code: status.and_then(|s| s.code()), stdout, stderr }
}

pub fn run(cmd: &Path, args: &[&str]) -> Ran {
    run_in(cmd, args, None, &[], Duration::from_secs(120))
}

/// Whether the Claude desktop app is running. It keeps its claude_desktop_config.json in memory and
/// writes it back when it saves a setting or quits, so an edit made while it runs is lost (seen on
/// 2026-10-06: the install's agent-wiki entry was replaced by the old one six minutes later). Only the
/// desktop app counts: the Claude Code CLI is also named claude.
pub fn claude_desktop_running() -> bool {
    #[cfg(windows)]
    {
        let ps = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        let script = r"@(Get-Process claude -ErrorAction SilentlyContinue | Where-Object { $_.Path -match '\\WindowsApps\\Claude_|\\AnthropicClaude\\' }).Count";
        let r = run(&ps, &["-NoProfile", "-NonInteractive", "-Command", script]);
        r.ok && r.stdout.trim().parse::<u32>().is_ok_and(|n| n > 0)
    }
    #[cfg(target_os = "macos")]
    {
        run(Path::new("/usr/bin/pgrep"), &["-x", "Claude"]).ok
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// A program on PATH (Windows also tries .exe and .cmd), or None.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let p = dir.join(format!("{name}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// GET http://127.0.0.1:<port>/health: the JSON body, or None.
pub fn health(port: u16) -> Option<Value> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    write!(s, "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok()?;
    serde_json::from_str(raw.split_once("\r\n\r\n")?.1).ok()
}

pub fn wait_for_health(port: u16, ok: impl Fn(&Value) -> bool, timeout: Duration) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        last = health(port);
        if last.as_ref().is_some_and(&ok) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    last
}

/// What the self-test checks over one transport: the server's name and version, its tools, and that
/// its instructions name the wiki and wiki_start answers.
pub struct McpCheck {
    pub server: String,
    pub tools: Vec<String>,
    pub instructions: String,
    pub start_ok: bool,
}

fn initialize() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "agent-wiki-installer", "version": aw_core::VERSION } } })
}

fn summarize(init: &Value, tools: &Value, start: &Value) -> McpCheck {
    let mut names: Vec<String> = tools["result"]["tools"].as_array().map(|a| a.iter().filter_map(|t| t["name"].as_str().map(String::from)).collect()).unwrap_or_default();
    names.sort();
    McpCheck {
        server: format!("{} {}", init["result"]["serverInfo"]["name"].as_str().unwrap_or("?"), init["result"]["serverInfo"]["version"].as_str().unwrap_or("?")),
        tools: names,
        instructions: init["result"]["instructions"].as_str().unwrap_or("").to_string(),
        start_ok: start["result"].is_object() && start["result"]["isError"] != true,
    }
}

const START: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wiki_start","arguments":{"app":"installer","topic":"install"}}}"#;

/// The installed server over stdio.
pub fn mcp_stdio(agent: &Path, env_clear_agent_wiki: bool) -> Result<McpCheck, String> {
    let mut c = Command::new(agent);
    c.arg("serve").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if env_clear_agent_wiki {
        for (k, _) in std::env::vars() {
            if k.starts_with("AGENT_WIKI_") {
                c.env_remove(k);
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let mut child = c.spawn().map_err(|e| format!("cannot start {}: {e}", agent.display()))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut ask = |msg: &str| -> Result<Value, String> {
        writeln!(stdin, "{msg}").map_err(|e| e.to_string())?;
        let line = lines.next().ok_or("the server closed stdout")?.map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| e.to_string())
    };
    let init = ask(&initialize().to_string())?;
    // A notification gets no answer: it goes out in front of the next request.
    let tools = ask(concat!(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "\n", r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#))?;
    let start = ask(START)?;
    drop(stdin);
    let _ = child.wait();
    Ok(summarize(&init, &tools, &start))
}

fn post(port: u16, body: &str, sid: Option<&str>) -> Result<(Option<String>, Value), String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(4)).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let session = sid.map(|v| format!("Mcp-Session-Id: {v}\r\nMcp-Protocol-Version: 2025-06-18\r\n")).unwrap_or_default();
    write!(
        s,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|e| e.to_string())?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let (head, body) = raw.split_once("\r\n\r\n").ok_or("malformed response")?;
    let sid = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id")).map(|(_, v)| v.trim().to_string()));
    Ok((sid, if body.trim().is_empty() { Value::Null } else { serde_json::from_str(body).map_err(|e| e.to_string())? }))
}

/// The service over Streamable HTTP.
pub fn mcp_http(port: u16) -> Result<McpCheck, String> {
    let (sid, init) = post(port, &initialize().to_string(), None)?;
    let sid = sid.ok_or("no session id")?;
    post(port, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, Some(&sid))?;
    let (_, tools) = post(port, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, Some(&sid))?;
    let (_, start) = post(port, START, Some(&sid))?;
    Ok(summarize(&init, &tools, &start))
}
