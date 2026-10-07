//! `serve --http`: Streamable HTTP on 127.0.0.1 (stateless, JSON responses), /health, /status, the
//! window (/ui, /api). Only this machine and never a web page: other Hosts and cross-origin requests
//! are refused (DNS rebinding). Same behavior as src/server.mjs + the MCP SDK's transport.

use crate::httpd::{self, Request, Response};
use crate::mcp::{self, Ctx, State, log, rpc_error};
use crate::uiapi;
use aw_core::inbox;
use aw_core::lock::lock_info;
use aw_core::paths::AppPaths;
use aw_core::reqlog::{RequestLog, clip_str, new_request_id};
use aw_core::text::{from_ms, local_iso, now_ms, parse_ms, random_uuid, sha256_hex};
use aw_core::waker::Waker;
use aw_core::{asks, wiki};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

const MAX_BATCH: usize = 100;

pub fn build_hash() -> String {
    static B: OnceLock<String> = OnceLock::new();
    B.get_or_init(|| std::env::current_exe().ok().and_then(|p| std::fs::read(p).ok()).map(|b| sha256_hex(&b)[..12].to_string()).unwrap_or_else(|| "unknown".into())).clone()
}

pub fn started_ms() -> i64 {
    static S: OnceLock<i64> = OnceLock::new();
    *S.get_or_init(now_ms)
}

/// Recent errors (5xx, unexpected exceptions) for /status.
pub struct Counters {
    recent: Mutex<VecDeque<i64>>,
    total: AtomicUsize,
    last: Mutex<Option<Value>>,
}

impl Counters {
    fn new() -> Self {
        Counters { recent: Mutex::new(VecDeque::new()), total: AtomicUsize::new(0), last: Mutex::new(None) }
    }
    pub fn error(&self, msg: &str) {
        self.total.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(json!({ "at": mcp::now_iso(), "message": clip_str(msg, 200) }));
        let mut r = self.recent.lock().unwrap();
        r.push_back(now_ms());
        while r.front().is_some_and(|t| now_ms() - t > 15 * 60_000) {
            r.pop_front();
        }
    }
    pub fn snapshot(&self) -> Value {
        let mut r = self.recent.lock().unwrap();
        while r.front().is_some_and(|t| now_ms() - t > 15 * 60_000) {
            r.pop_front();
        }
        json!({ "total": self.total.load(Ordering::SeqCst), "last15m": r.len(), "last": self.last.lock().unwrap().clone() })
    }
}

/// Mcp-Session-Id -> client name, kept in logs/http-sessions.json so names survive restarts.
struct Sessions {
    file: PathBuf,
    map: Mutex<Vec<(String, String)>>,
}

impl Sessions {
    fn load(file: PathBuf) -> Self {
        let map = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str::<Map<String, Value>>(&t).ok())
            .map(|m| m.into_iter().map(|(k, v)| (k, v.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default();
        Sessions { file, map: Mutex::new(map) }
    }
    fn get(&self, id: &str) -> Option<String> {
        self.map.lock().unwrap().iter().find(|(k, _)| k == id).map(|(_, v)| v.clone())
    }
    fn set(&self, id: &str, client: &str) {
        let mut m = self.map.lock().unwrap();
        m.retain(|(k, _)| k != id);
        m.push((id.to_string(), client.to_string()));
        while m.len() > 500 {
            m.remove(0);
        }
        let obj: Map<String, Value> = m.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        let _ = std::fs::create_dir_all(self.file.parent().unwrap_or(&self.file));
        let tmp = PathBuf::from(format!("{}.{}.tmp", self.file.display(), std::process::id()));
        if std::fs::write(&tmp, Value::Object(obj).to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }
}

pub struct App {
    pub state: RwLock<State>,
    pub reqlog: RequestLog,
    sessions: Sessions,
    pub counters: Counters,
    pub port: u16,
    closing: AtomicBool,
    inflight: AtomicUsize,
}

impl App {
    /// The state, retrying setup if it failed (e.g. folder permissions fixed since startup).
    pub fn state(&self) -> State {
        if self.state.read().unwrap().setup_error.is_some() {
            *self.state.write().unwrap() = mcp::setup();
        }
        self.state.read().unwrap().clone()
    }
}

/// The /status report: health, queue, curator, Ask worker, recent activity, lock.
pub fn status_report(app: &App, st: &State) -> Value {
    let started = started_ms();
    let mut s = Map::new();
    s.insert("ok".into(), json!(st.setup_error.is_none()));
    s.insert("name".into(), json!("agent-wiki"));
    s.insert("version".into(), json!(aw_core::VERSION));
    s.insert("build".into(), json!(build_hash()));
    s.insert("pid".into(), json!(std::process::id()));
    s.insert("startedAt".into(), json!(local_iso(&from_ms(started))));
    s.insert("uptimeSec".into(), json!(((now_ms() - started) as f64 / 1000.0).round() as i64));
    s.insert("wikiDir".into(), st.wiki_dir.as_ref().map(|d| json!(wiki::disp(d))).unwrap_or(Value::Null));
    s.insert("mcpUrl".into(), json!(format!("http://127.0.0.1:{}/mcp", app.port)));
    s.insert("writeMode".into(), json!(if st.curated { "curated" } else { "direct" }));
    if let Some(e) = &st.setup_error {
        s.insert("error".into(), json!(e));
    }
    let Some(dir) = st.wiki_dir.as_ref().filter(|_| st.setup_error.is_none()) else {
        s.insert("health".into(), json!("degraded"));
        s.insert("reasons".into(), json!([st.setup_error.clone().unwrap_or_else(|| "wiki unavailable".into())]));
        return Value::Object(s);
    };
    let queue = inbox::queue_stats(dir);
    let headlines = wiki::recent_headlines(dir, 3, 10);
    let curator = inbox::read_curator_status(dir);
    let paused = inbox::is_paused(dir);
    let lock = lock_info(dir, "write");
    let asker = asks::worker_status(dir);
    let heartbeat_age = curator.as_ref().and_then(|c| c.get("heartbeatAt")).and_then(Value::as_str).and_then(parse_ms).map(|t| (now_ms() - t) as f64 / 1000.0);
    let running = heartbeat_age.is_some_and(|a| a < 180.0);
    let errors = app.counters.snapshot();
    let mut reasons: Vec<String> = vec![];
    let oldest = queue["oldestPendingAt"].as_str().and_then(parse_ms);
    let wait_min = oldest.map(|t| (now_ms() - t) as f64 / 60_000.0).unwrap_or(0.0);
    let pending = queue["pending"].as_i64().unwrap_or(0);
    if st.curated {
        let dead = queue["dead"].as_i64().unwrap_or(0);
        if dead > 0 {
            reasons.push(format!("{dead} note(s) failed curation"));
        }
        if pending > 0 && wait_min > 30.0 {
            reasons.push(format!("backlog: {pending} note(s), oldest waiting {} min", wait_min.round() as i64));
        }
        if pending > 0 && !running {
            reasons.push("curator is not running".into());
        }
        let cstate = curator.as_ref().and_then(|c| c.get("state")).and_then(Value::as_str).unwrap_or("");
        if running && cstate == "signed_out" {
            reasons.push("curator is signed out of ChatGPT".into());
        }
        if running && (cstate == "rate_limited" || cstate == "error") {
            let last = curator.as_ref().and_then(|c| c.get("lastError")).and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(cstate);
            reasons.push(format!("curator: {last}"));
        }
        let held = aw_core::held::pending_count(dir) + aw_core::forget::pending_count(dir);
        s.insert("held".into(), json!(held));
        let (approvals, problem) = aw_core::settings::approvals_and_problem(dir);
        s.insert("approvals".into(), json!(approvals.as_str()));
        s.insert("autoApplied".into(), aw_core::held::auto_summary(dir));
        // The models in use (for the tray's menu), and a choice this ChatGPT account does not offer.
        let models = aw_core::models::overview(dir, &st.config);
        reasons.extend(models["problems"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from));
        s.insert("models".into(), json!({ "curator": { "model": models["curator"]["model"], "reasoningEffort": models["curator"]["reasoningEffort"] }, "ask": { "model": models["ask"]["model"], "reasoningEffort": models["ask"]["reasoningEffort"] } }));
        let cleanup = aw_core::lint::schedule_status(dir, aw_core::curator::cleanups_off(&st.config));
        reasons.extend(cleanup["problem"].as_str().map(String::from));
        s.insert("cleanup".into(), cleanup);
        // Hybrid search: on, and whether this process can reach the API key (else searches are BM25 alone).
        if let Some(e) = aw_core::embed::settings() {
            s.insert("embeddings".into(), json!({ "model": e.model, "keyAvailable": aw_core::embed::api_key(&e).is_some() }));
        }
        reasons.extend(problem);
        if held > 0 {
            reasons.push(format!("{held} change(s) need your OK (window > Inbox)"));
        }
        let skipped: Vec<&str> = curator.as_ref().and_then(|c| c["lastRun"]["skipped"].as_array()).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        if !skipped.is_empty() {
            reasons.push(format!("curator: {} page(s) did not fit in the last batch's context: {}", skipped.len(), skipped.join(", ")));
        }
    }
    let last15 = errors["last15m"].as_i64().unwrap_or(0);
    if last15 > 0 {
        reasons.push(format!("{last15} server error(s) in the last 15 min"));
    }
    let last_note = queue["newestAt"].as_str().and_then(parse_ms).unwrap_or(0);
    let last = headlines.first();
    let last_log = last.and_then(|h| parse_ms(&format!("{}T{}:00", h.date, h.time))).unwrap_or(0);
    let last_write = if last_note > last_log {
        json!({ "at": queue["newestAt"], "text": "note queued" })
    } else if let Some(h) = last {
        json!({ "at": format!("{} {}", h.date, h.time), "text": h.text })
    } else {
        Value::Null
    };
    s.insert("health".into(), json!(if reasons.is_empty() { "ok" } else { "degraded" }));
    s.insert("reasons".into(), json!(reasons));
    s.insert("queue".into(), queue);
    s.insert("lastWrite".into(), last_write);
    s.insert("recent".into(), serde_json::to_value(&headlines).unwrap_or(Value::Null));
    let cur = match curator {
        Some(Value::Object(mut c)) => {
            c.insert("running".into(), json!(running));
            c.insert("heartbeatAgeSec".into(), heartbeat_age.map(|a| json!(a.round() as i64)).unwrap_or(Value::Null));
            c.insert("paused".into(), json!(paused));
            Value::Object(c)
        }
        _ => json!({ "running": false, "paused": paused }),
    };
    s.insert("curator".into(), cur);
    s.insert("requests".into(), errors);
    s.insert("ask".into(), asker);
    s.insert(
        "lock".into(),
        lock.map(|l| {
            json!({
                "state": l.state.map(|x| x.as_str()),
                "label": l.owner.as_ref().and_then(|o| o.label.clone()),
                "pid": l.owner.as_ref().map(|o| o.pid),
                "ageSec": (l.age_ms as f64 / 1000.0).round() as i64,
            })
        })
        .unwrap_or(Value::Null),
    );
    Value::Object(s)
}

// ---------------------------------------------------------------- responses

pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl Reply {
    pub fn json(status: u16, v: &Value) -> Self {
        Reply { status, body: format!("{v}\n").into_bytes(), headers: vec![("Content-Type".into(), "application/json".into()), ("Cache-Control".into(), "no-store".into())] }
    }
    pub fn with(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// What a request log line records about one HTTP request.
#[derive(Default)]
pub struct Entry {
    pub rid: String,
    pub path: String,
    pub rpc: Option<String>,
    pub sid: Option<String>,
    pub client: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub quiet: bool,
}

fn describe_rpc(body: &Value) -> String {
    let msgs: Vec<&Value> = match body {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    msgs.iter()
        .map(|m| {
            if m.get("method").and_then(Value::as_str) == Some("tools/call") {
                format!("tools/call {}", m["params"]["name"].as_str().unwrap_or("undefined"))
            } else if let Some(method) = m.get("method").and_then(Value::as_str) {
                method.to_string()
            } else if m.get("result").is_some() {
                "response".into()
            } else {
                "?".into()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn sdk_error(status: u16, code: i64, message: &str) -> Reply {
    Reply::json(status, &rpc_error(Value::Null, code, message))
}

fn handle(app: &App, req: &Request, entry: &mut Entry) -> Reply {
    let host = req.header("host").unwrap_or_default().to_lowercase();
    if host != format!("127.0.0.1:{}", app.port) && host != format!("localhost:{}", app.port) {
        entry.result = Some("forbidden-host".into());
        return Reply::json(403, &json!({ "error": "Forbidden: unexpected Host header" }));
    }
    if let Some(origin) = req.header("origin") {
        // Loopback is not an origin: a page on another local server is still cross-origin.
        // The listener is plain HTTP, and the browser must use this exact host and port.
        let authority = if app.port == 80 { host.trim_end_matches(":80") } else { &host };
        if !origin.eq_ignore_ascii_case(&format!("http://{authority}")) {
            entry.result = Some("forbidden-origin".into());
            return Reply::json(403, &json!({ "error": "Forbidden: cross-origin request" }));
        }
    }
    // Browsers say who sent a request: refuse what another site's page sent (an <img> or a form needs
    // no CORS approval, so the Origin check alone does not stop it). Apps and the tray send no such header.
    if req.header("sec-fetch-site").is_some_and(|s| !matches!(s.to_ascii_lowercase().as_str(), "same-origin" | "none")) {
        entry.result = Some("forbidden-site".into());
        return Reply::json(403, &json!({ "error": "Forbidden: sent by another site" }));
    }
    let url = req.url.clone();
    let (path, query) = url.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((url.clone(), String::new()));
    let path = percent_decode_path(&path);
    entry.path = path.clone();
    let method = req.method.as_str();
    if path == "/health" || path == "/status" {
        if method != "GET" {
            return Reply::json(405, &json!({ "error": "Use GET" })).with("Allow", "GET");
        }
        let st = app.state();
        if path == "/status" {
            entry.quiet = true;
            return Reply::json(200, &status_report(app, &st));
        }
        let mut h = Map::new();
        h.insert("ok".into(), json!(st.setup_error.is_none()));
        h.insert("name".into(), json!("agent-wiki"));
        h.insert("version".into(), json!(aw_core::VERSION));
        h.insert("build".into(), json!(build_hash()));
        h.insert("wikiDir".into(), st.wiki_dir.as_ref().map(|d| json!(wiki::disp(d))).unwrap_or(Value::Null));
        h.insert("pid".into(), json!(std::process::id()));
        h.insert("uptimeSec".into(), json!(((now_ms() - started_ms()) as f64 / 1000.0).round() as i64));
        h.insert("writeMode".into(), json!(if st.curated { "curated" } else { "direct" }));
        if let Some(e) = &st.setup_error {
            h.insert("error".into(), json!(e));
        }
        return Reply::json(if st.setup_error.is_some() { 503 } else { 200 }, &Value::Object(h));
    }
    if path == "/ui" || path.starts_with("/ui/") || path.starts_with("/api/") {
        let st = app.state();
        return uiapi::handle(app, &st, req, method, &path, &query, entry);
    }
    if path != "/mcp" {
        return Reply::json(404, &json!({ "error": "Not found: the MCP endpoint is /mcp, the tray window /ui/" }));
    }
    if method != "POST" {
        return Reply::json(405, &rpc_error(Value::Null, -32000, "Method not allowed: this server is stateless, use POST.")).with("Allow", "POST");
    }
    if app.closing.load(Ordering::SeqCst) {
        return Reply::json(503, &rpc_error(Value::Null, -32000, "agent-wiki is restarting; retry shortly."));
    }
    let raw = match read_body(req) {
        Ok(b) => b,
        Err(status) => {
            entry.result = Some("parse-error".into());
            return Reply::json(status, &rpc_error(Value::Null, -32700, "Parse error: request body too large"));
        }
    };
    let body: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            entry.result = Some("parse-error".into());
            return Reply::json(400, &rpc_error(Value::Null, -32700, &format!("Parse error: {e}")));
        }
    };
    entry.rpc = Some(describe_rpc(&body));
    let mut sid = req.header("mcp-session-id").filter(|s| !s.is_empty());
    let messages: Vec<Value> = match &body {
        Value::Array(a) => a.clone(),
        other => vec![other.clone()],
    };
    let init = messages.iter().find(|m| m.get("method").and_then(Value::as_str) == Some("initialize")).cloned();
    let mut reply_headers: Vec<(String, String)> = vec![];
    if let Some(init) = &init {
        let new_sid = random_uuid();
        let ci = &init["params"]["clientInfo"];
        let name = if ci.is_object() { format!("{} {}", ci["name"].as_str().unwrap_or(""), ci["version"].as_str().unwrap_or("")).trim().to_string() } else { "unknown".into() };
        app.sessions.set(&new_sid, &name);
        reply_headers.push(("Mcp-Session-Id".into(), new_sid.clone()));
        sid = Some(new_sid);
    }
    entry.sid = sid.clone();
    entry.client = sid.as_deref().and_then(|s| app.sessions.get(s));
    // The SDK's transport rules.
    let accept = req.header("accept").unwrap_or_default();
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return with_headers(sdk_error(406, -32000, "Not Acceptable: Client must accept both application/json and text/event-stream"), reply_headers);
    }
    let ct = req.header("content-type").unwrap_or_default();
    if !ct.to_lowercase().split(';').next().unwrap_or("").trim().eq("application/json") {
        return with_headers(sdk_error(415, -32000, "Unsupported Media Type: Content-Type must be application/json"), reply_headers);
    }
    if body.is_array() && messages.len() > MAX_BATCH {
        return with_headers(sdk_error(400, -32600, &format!("Invalid Request: Batch must not exceed {MAX_BATCH} messages")), reply_headers);
    }
    if !messages.iter().all(mcp::valid_message) {
        return with_headers(sdk_error(400, -32700, "Parse error: Invalid JSON-RPC message"), reply_headers);
    }
    if init.is_some() && messages.len() > 1 {
        return with_headers(sdk_error(400, -32600, "Invalid Request: Only one initialization request is allowed"), reply_headers);
    }
    if init.is_none()
        && let Some(v) = req.header("mcp-protocol-version")
        && !mcp::SUPPORTED_VERSIONS.contains(&v.as_str())
    {
        return with_headers(sdk_error(400, -32000, &format!("Bad Request: Unsupported protocol version: {v} (supported versions: {})", mcp::SUPPORTED_VERSIONS.join(", "))), reply_headers);
    }
    if !messages.iter().any(mcp::is_request) {
        return with_headers(Reply { status: 202, body: vec![], headers: vec![] }, reply_headers);
    }
    let st = app.state();
    let client = sid.as_deref().and_then(|s| app.sessions.get(s)).unwrap_or_default();
    let on_error = |m: &str| app.counters.error(m);
    let ctx = Ctx { transport: "http", rid: Some(entry.rid.clone()), client, reqlog: &app.reqlog, on_error: Some(&on_error) };
    let ci = Mutex::new(None);
    let responses: Vec<Value> = messages.iter().filter_map(|m| mcp::handle(&st, &ctx, m, &ci)).collect();
    let out = if responses.len() == 1 { responses[0].clone() } else { Value::Array(responses) };
    with_headers(Reply { status: 200, body: out.to_string().into_bytes(), headers: vec![("Content-Type".into(), "application/json".into())] }, reply_headers)
}

fn with_headers(mut r: Reply, extra: Vec<(String, String)>) -> Reply {
    r.headers.extend(extra);
    r
}

fn percent_decode_path(p: &str) -> String {
    // URL.pathname keeps percent-escapes; only %2F-style traversal matters to the /ui check, which rejects them.
    p.to_string()
}

/// The request body as text, or 413 when it was over httpd::MAX_BODY.
pub fn read_body(req: &Request) -> Result<String, u16> {
    req.body().map(|b| String::from_utf8_lossy(b).into_owned())
}

/// Serves until the parent closes stdin (`parent_stdin`), or, with `exit_on_upgrade`, until a new
/// build of this program replaces it (the service manager then starts the new one).
/// AGENT_WIKI_PROCESS must already be set (main does it before any thread starts).
pub fn run(port: u16, parent_stdin: bool, exit_on_upgrade: bool) -> ! {
    let stop = Waker::new();
    if parent_stdin {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut sink = [0u8; 4096];
            let mut stdin = std::io::stdin();
            while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
            stop.stop();
        });
    }
    match serve(port, stop, exit_on_upgrade) {
        Ok(Ended::Stopped) => std::process::exit(0),
        Ok(Ended::Upgraded) => std::process::exit(UPGRADED_EXIT),
        Err(e) => {
            log(&e);
            std::process::exit(1)
        }
    }
}

/// Exit code after an upgrade: not 0, so service managers restart the program.
pub const UPGRADED_EXIT: i32 = 75;

pub enum Ended {
    Stopped,
    Upgraded,
}

/// This program's file, to notice when an install replaces it: which file it is, its size and time.
fn exe_stamp(exe: &std::path::Path) -> Option<((u64, u64), u64, std::time::SystemTime)> {
    let m = std::fs::metadata(exe).ok()?;
    Some((aw_core::sys::file_identity(exe)?, m.len(), m.modified().ok()?))
}

/// Serves on 127.0.0.1:`port` until `stop` (graceful: in-flight requests get up to 5 s), or until this
/// program's file changes (`watch_exe`).
pub fn serve(port: u16, stop: Arc<Waker>, watch_exe: bool) -> Result<Ended, String> {
    started_ms();
    let state = mcp::setup();
    let reqlog = mcp::open_request_log("service", &state.config);
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let mode = if state.curated { "curated" } else { "direct" };
    let wiki_shown = state.wiki_dir.as_ref().map(|d| wiki::disp(d)).unwrap_or_else(|| "(unavailable)".into());
    let app = Arc::new(App {
        state: RwLock::new(state),
        reqlog,
        sessions: Sessions::load(AppPaths::current().http_sessions()),
        counters: Counters::new(),
        port: actual,
        closing: AtomicBool::new(false),
        inflight: AtomicUsize::new(0),
    });
    log(&format!("v{} listening on http://127.0.0.1:{actual}/mcp ({mode} writes); wiki at {wiki_shown}", aw_core::VERSION));

    let exe = std::env::current_exe().ok().filter(|_| watch_exe);
    let stamp = exe.as_deref().and_then(exe_stamp);
    let shared = app.clone();
    let handler = move |req: &Request| -> Response {
        app.inflight.fetch_add(1, Ordering::SeqCst);
        let t0 = now_ms();
        let rid = new_request_id();
        let ua = req.header("user-agent");
        let mut entry = Entry { rid: rid.clone(), ..Entry::default() };
        let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&app, req, &mut entry))).unwrap_or_else(|_| {
            log("request failed: internal error");
            entry.error = Some("internal error".into());
            Reply::json(500, &rpc_error(Value::Null, -32603, "Internal error"))
        });
        let status = reply.status;
        app.inflight.fetch_sub(1, Ordering::SeqCst);
        if !(entry.quiet && status < 400) {
            if status >= 500 {
                app.counters.error(&format!("HTTP {status} {}", entry.rpc.clone().unwrap_or_else(|| entry.path.clone())));
            }
            let path = if entry.path.is_empty() { req.url.clone() } else { entry.path.clone() };
            app.reqlog.write(vec![
                ("kind", json!("http")),
                ("rid", json!(rid)),
                ("transport", json!("http")),
                ("method", json!(req.method)),
                ("path", json!(path)),
                ("ua", ua.map(|u| json!(clip_str(&u, 80))).unwrap_or(Value::Null)),
                ("rpc", entry.rpc.map(Value::String).unwrap_or(Value::Null)),
                ("sid", entry.sid.map(Value::String).unwrap_or(Value::Null)),
                ("client", entry.client.map(|c| json!(clip_str(&c, 80))).unwrap_or(Value::Null)),
                ("result", entry.result.map(Value::String).unwrap_or(Value::Null)),
                ("error", entry.error.map(Value::String).unwrap_or(Value::Null)),
                ("status", json!(status)),
                ("ms", json!(now_ms() - t0)),
            ]);
        }
        Response { status, headers: reply.headers, body: reply.body }
    };
    let acceptor = {
        let stop = stop.clone();
        std::thread::spawn(move || httpd::serve(listener, Arc::new(handler), stop))
    };
    let mut ended = Ended::Stopped;
    while !stop.wait_stop(2000) {
        let Some(exe) = &exe else { continue };
        let now = exe_stamp(exe);
        // Once the new file has been in place for a second (an install copies, then renames).
        let settled = now.is_some_and(|(_, _, t)| t.elapsed().is_ok_and(|e| e > Duration::from_secs(1)));
        if now.is_some() && now != stamp && settled {
            log("a new build of agent-wiki was installed; restarting");
            ended = Ended::Upgraded;
            stop.stop();
        }
    }
    shared.closing.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    while shared.inflight.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    log(&format!("shutting down ({})", if matches!(ended, Ended::Upgraded) { "upgraded" } else { "stopped" }));
    let _ = acceptor.join();
    Ok(ended)
}
