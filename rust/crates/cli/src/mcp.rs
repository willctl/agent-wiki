//! The MCP side of the server (src/server.mjs): state, the five tools, JSON-RPC message handling.
//! The tools are defined once, in mcp/tools.json (curated writes, the default). With writes "direct"
//! two of them do something else, so they read differently: mcp/tools-direct-text.json replaces
//! only their title and description.

use aw_core::inbox::{self, NoteInput, PageInput};
use aw_core::lock::cleanup_locks;
use aw_core::paths::{AppPaths, Env};
use aw_core::reqlog::{RequestLog, clip, clip_str, new_request_id, summarize_args};
use aw_core::text::{defang_remote_images, local_iso, now, now_ms};
use aw_core::wiki::{self, Error, UpsertInput};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::Mutex;

pub const SUPPORTED_VERSIONS: [&str; 5] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];
const ASK_TOOLS: [&str; 2] = ["wiki_search", "wiki_read"];
const TOOLS: &str = include_str!("mcp/tools.json");
const TOOLS_DIRECT_TEXT: &str = include_str!("mcp/tools-direct-text.json");

pub fn log(msg: &str) {
    eprintln!("[agent-wiki {}] {msg}", chrono_utc_iso());
}

fn chrono_utc_iso() -> String {
    let n = now();
    n.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[derive(Clone)]
pub struct State {
    pub wiki_dir: Option<PathBuf>,
    pub setup_error: Option<String>,
    pub config: Value,
    pub curated: bool,
    pub read_only: bool,
}

pub fn env() -> Env {
    aw_core::paths::process_env()
}

pub fn setup() -> State {
    let env = env();
    let mut st = State { wiki_dir: None, setup_error: None, config: json!({}), curated: true, read_only: false };
    st.config = wiki::read_config(&env).unwrap_or_else(|_| json!({}));
    aw_core::embed::configure(&st.config);
    let mode = env.get("AGENT_WIKI_WRITE_MODE").cloned().or_else(|| st.config.get("writeMode").and_then(Value::as_str).map(String::from));
    st.curated = mode.as_deref() != Some("direct");
    let r = (|| -> wiki::Result<PathBuf> {
        let (dir, _) = wiki::resolve_wiki_dir(&env)?;
        wiki::ensure_wiki(&dir)?;
        cleanup_locks(&dir);
        wiki::with_lock(&dir, || wiki::refresh_index(&dir, None).map(|_| ()))?;
        Ok(dir)
    })();
    match r {
        Ok(d) => st.wiki_dir = Some(d),
        Err(e) => {
            log(&format!("setup failed: {e}"));
            st.setup_error = Some(e.to_string());
            st.wiki_dir = wiki::resolve_wiki_dir(&env).ok().map(|(d, _)| d);
        }
    }
    st
}

/// setup() for --read-only: resolves the wiki and writes nothing.
pub fn setup_read_only() -> State {
    let env = env();
    let mut st = State { wiki_dir: None, setup_error: None, config: json!({}), curated: true, read_only: true };
    st.config = wiki::read_config(&env).unwrap_or_else(|_| json!({}));
    aw_core::embed::configure(&st.config);
    match wiki::resolve_wiki_dir(&env) {
        Ok((d, _)) if d.exists() => st.wiki_dir = Some(d),
        Ok((d, _)) => {
            let m = format!("There is no wiki at {}.", wiki::disp(&d));
            log(&format!("setup failed: {m}"));
            st.setup_error = Some(m);
        }
        Err(e) => {
            log(&format!("setup failed: {e}"));
            st.setup_error = Some(e.to_string());
        }
    }
    st
}

pub fn open_request_log(proc_name: &str, config: &Value) -> RequestLog {
    let days = config.get("logs").and_then(|l| l.get("retentionDays")).and_then(Value::as_i64).filter(|d| *d > 0).unwrap_or(30);
    RequestLog::new(&AppPaths::current().log_dir, proc_name, days)
}

fn lead_in(wiki_dir: &str, curated: bool) -> String {
    let writes = if curated {
        "Tell it what happened with wiki_log (and wiki_upsert_page for page content): a curator agent organizes notes into pages, so hand over the facts and skip the formatting."
    } else {
        "Record decisions, outcomes, preferences, facts and follow-ups worth keeping with wiki_log, and keep topic pages current with wiki_upsert_page."
    };
    format!(
        "Shared long-term memory for this user across Claude, ChatGPT and other AI apps (wiki folder: {wiki_dir}). Call wiki_start once at the beginning of EVERY conversation, before your first substantive reply, and follow the protocol it returns. Search the wiki (wiki_search) before asking the user for context they may have given before. {writes} Never store secrets."
    )
}

pub fn instructions(st: &State) -> String {
    let dir = st.wiki_dir.as_ref().map(|d| wiki::disp(d)).unwrap_or_else(|| "~/AgentWiki".into());
    if st.read_only {
        return format!("Read-only access to this user's shared wiki ({dir}): wiki_search finds pages, log entries and notes not yet filed; wiki_read opens one.");
    }
    let protocol = st.wiki_dir.as_ref().map(|d| wiki::read_protocol(d)).unwrap_or_else(|| wiki::DEFAULT_PROTOCOL.to_string());
    format!("{}\n\n{}", lead_in(&dir, st.curated), protocol.trim())
}

fn tool_list(curated: bool) -> Vec<Value> {
    let mut all: Vec<Value> = serde_json::from_str(TOOLS).unwrap_or_default();
    if !curated {
        let text: Value = serde_json::from_str(TOOLS_DIRECT_TEXT).unwrap_or_default();
        for t in &mut all {
            if let Some(o) = text[t["name"].as_str().unwrap_or("")].as_object() {
                for (k, v) in o {
                    t[k] = v.clone();
                }
            }
        }
    }
    all
}

pub fn tools(st: &State) -> Vec<Value> {
    tool_list(st.curated).into_iter().filter(|t| !st.read_only || ASK_TOOLS.contains(&t["name"].as_str().unwrap_or(""))).collect()
}

/// Per-connection (stdio) or per-request (HTTP) context.
pub struct Ctx<'a> {
    pub transport: &'a str,
    pub rid: Option<String>,
    pub client: String,
    pub reqlog: &'a RequestLog,
    pub on_error: Option<&'a (dyn Fn(&str) + Sync)>,
}

// ---------------------------------------------------------------- argument validation (the zod schemas)

fn type_name(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

/// Checks `args` against a tool's inputSchema; returns zod-like messages.
fn validate(schema: &Value, args: &Value) -> Vec<String> {
    let mut problems = vec![];
    let empty = Map::new();
    let obj = match args {
        Value::Object(o) => o,
        Value::Null => &empty,
        other => return vec![format!("Invalid input: expected object, received {}", type_name(Some(other)))],
    };
    let props = schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    for (name, spec) in &props {
        let v = obj.get(name);
        if v.is_none() || v == Some(&Value::Null) && !required.contains(&name.as_str()) {
            if required.contains(&name.as_str()) {
                problems.push(format!("Invalid input: expected {}, received undefined at {name}", spec["type"].as_str().unwrap_or("value")));
            }
            continue;
        }
        let v = v.unwrap();
        let want = spec["type"].as_str().unwrap_or("");
        let ok = match want {
            "string" => v.is_string(),
            "integer" => v.as_f64().is_some_and(|f| f.fract() == 0.0),
            "number" => v.is_number(),
            "array" => v.as_array().is_some_and(|a| {
                let item = spec["items"]["type"].as_str().unwrap_or("");
                item != "string" || a.iter().all(Value::is_string)
            }),
            "boolean" => v.is_boolean(),
            _ => true,
        };
        if !ok {
            let want_label = if want == "integer" && v.is_number() { "int".to_string() } else { want.to_string() };
            problems.push(format!("Invalid input: expected {want_label}, received {} at {name}", type_name(Some(v))));
            continue;
        }
        if let Some(options) = spec.get("enum").and_then(Value::as_array)
            && !options.contains(v)
        {
            let list = options.iter().map(|o| o.to_string()).collect::<Vec<_>>().join("|");
            problems.push(format!("Invalid option: expected one of {list} at {name}"));
        }
        if let (Some(p), Some(s)) = (spec.get("pattern").and_then(Value::as_str), v.as_str())
            && !regex::Regex::new(p).is_ok_and(|re| re.is_match(s))
        {
            problems.push(format!("Invalid string: must match pattern /{p}/ at {name}"));
        }
        if let Some(n) = v.as_f64() {
            if let Some(min) = spec.get("minimum").and_then(Value::as_f64).filter(|m| n < *m) {
                problems.push(format!("Too small: expected number to be >={} at {name}", wiki::js_number(min)));
            }
            if let Some(max) = spec.get("maximum").and_then(Value::as_f64).filter(|m| n > *m) {
                problems.push(format!("Too big: expected number to be <={} at {name}", wiki::js_number(max)));
            }
        }
    }
    problems
}

fn text_result(text: String, is_error: bool) -> Value {
    let mut r = json!({ "content": [{ "type": "text", "text": text }] });
    if is_error {
        r["isError"] = Value::Bool(true);
    }
    r
}

fn s(args: &Value, k: &str) -> String {
    args.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn opt_s(args: &Value, k: &str) -> Option<String> {
    args.get(k).and_then(Value::as_str).map(String::from)
}

fn queued(r: &inbox::Submitted) -> String {
    if r.duplicate {
        format!("Already saved as note {} ({}); nothing new was queued.", r.id, r.status)
    } else {
        format!(
            "Saved note {} ({}). The curator will file it into the wiki shortly; until then it is listed under \"Pending notes\" in wiki_start and found by wiki_search. No need to check on it.",
            r.id, r.rel
        )
    }
}

/// Runs one tool; failures become isError results. Logs the call.
pub fn call_tool(st: &State, ctx: &Ctx, name: &str, args: &Value) -> Value {
    let list = tools(st);
    let Some(def) = list.iter().find(|t| t["name"] == name) else {
        return text_result(format!("MCP error -32602: Tool {name} not found"), true);
    };
    let problems = validate(&def["inputSchema"], args);
    if !problems.is_empty() {
        return text_result(format!("MCP error -32602: Input validation error: Invalid arguments for tool {name}: {}", problems.join("; ")), true);
    }
    let t0 = now_ms();
    let outcome: Result<String, Error> = (|| {
        let Some(dir) = st.wiki_dir.as_ref().filter(|_| st.setup_error.is_none()) else {
            return Err(Error::Wiki(st.setup_error.clone().unwrap_or_else(|| "The wiki folder could not be resolved.".into())));
        };
        match name {
            // Read results never carry a remote image an app would fetch on its own (M2).
            "wiki_start" => wiki::start_context(dir, &s(args, "app"), &s(args, "topic")).map(|t| defang_remote_images(&t).into_owned()),
            "wiki_search" => {
                let query = s(args, "query");
                let scope = opt_s(args, "scope").unwrap_or_else(|| "all".into());
                let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(8);
                let filters = aw_core::search::Filters { since: opt_s(args, "since"), until: opt_s(args, "until"), app: opt_s(args, "app").filter(|a| !a.trim().is_empty()) };
                // Other wordings searched with it and fused (at most 5, each clipped like a query).
                let mut queries = vec![query.clone()];
                for q in args.get("queries").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(|q| clip_str(q.trim(), 300)) {
                    if !q.is_empty() && !queries.contains(&q) && queries.len() < 6 {
                        queries.push(q);
                    }
                }
                let hits = aw_core::search::search_many(dir, &queries, &scope, limit, &filters)?;
                let what = if queries.len() > 1 { format!("\"{query}\" and {} other wording(s)", queries.len() - 1) } else { format!("\"{query}\"") };
                if hits.is_empty() {
                    return Ok(format!("No results for {what}. The wiki has nothing on this yet, or it uses other words: try names, the broader topic, or what the answer would say."));
                }
                Ok(defang_remote_images(&format!("{} result(s) for {what}:\n\n{}", hits.len(), wiki::format_search_results(&hits))).into_owned())
            }
            "wiki_read" => {
                let (rel, text) = wiki::read_section(dir, &s(args, "target"), opt_s(args, "section").as_deref())?;
                Ok(format!("File: {rel}\n\n{}", defang_remote_images(&text)))
            }
            "wiki_log" => {
                if st.curated {
                    let note = NoteInput {
                        kind: "log".into(),
                        source: s(args, "source"),
                        app: s(args, "app"),
                        title: s(args, "title"),
                        body: s(args, "body"),
                        tags: args.get("tags").cloned().unwrap_or(Value::Null),
                        pages: args.get("pages").cloned().unwrap_or(Value::Null),
                        page: None,
                        idempotency_key: opt_s(args, "idempotency_key"),
                    };
                    return Ok(queued(&inbox::submit_note(dir, &note, ctx.transport, &ctx.client)?));
                }
                let (rel, heading) = wiki::append_log(dir, &s(args, "app"), &s(args, "title"), &s(args, "body"), args.get("tags").unwrap_or(&Value::Null), args.get("pages").unwrap_or(&Value::Null))?;
                Ok(format!("Logged to {rel}: {}", heading.trim_start_matches("## ")))
            }
            "wiki_upsert_page" => {
                if st.curated {
                    let slug = opt_s(args, "slug");
                    let note = NoteInput {
                        kind: "page".into(),
                        source: s(args, "source"),
                        app: s(args, "app"),
                        title: s(args, "title"),
                        body: s(args, "content"),
                        tags: args.get("tags").cloned().unwrap_or(Value::Null),
                        pages: slug.clone().filter(|s| !s.is_empty()).map(|s| json!([s])).unwrap_or(json!([])),
                        page: Some(PageInput { slug, title: opt_s(args, "title"), kind: opt_s(args, "type"), summary: opt_s(args, "summary"), mode: opt_s(args, "mode") }),
                        idempotency_key: opt_s(args, "idempotency_key"),
                    };
                    return Ok(queued(&inbox::submit_note(dir, &note, ctx.transport, &ctx.client)?));
                }
                let r = wiki::upsert_page(
                    dir,
                    &UpsertInput {
                        app: s(args, "app"),
                        title: s(args, "title"),
                        slug: opt_s(args, "slug"),
                        kind: opt_s(args, "type"),
                        summary: opt_s(args, "summary"),
                        tags: args.get("tags").cloned().filter(|t| !t.is_null()),
                        content: s(args, "content"),
                        mode: opt_s(args, "mode"),
                    },
                )?;
                let verb = match r.action {
                    "created" => "Created",
                    "updated" => "Appended to",
                    _ => "Rewrote",
                };
                let hist = r.history_rel.map(|h| format!(" (previous version kept at {h})")).unwrap_or_default();
                Ok(format!("{verb} {}{hist}. index.md regenerated.", r.rel))
            }
            _ => Err(Error::Wiki(format!("Tool {name} not found"))),
        }
    })();
    let (text, is_error, result, error) = match outcome {
        Ok(t) => (t, false, "ok", None),
        Err(Error::Wiki(m)) => {
            let r = if m.starts_with("Refused") { "refused" } else { "rejected" };
            (m.clone(), true, r, Some(m))
        }
        Err(e) => {
            let m = format!("agent-wiki error: {e}");
            log(&m);
            if let Some(f) = ctx.on_error {
                f(&m);
            }
            (m.clone(), true, "error", Some(m))
        }
    };
    ctx.reqlog.write(vec![
        ("kind", json!("tool")),
        ("rid", json!(ctx.rid.clone().unwrap_or_else(new_request_id))),
        ("transport", json!(ctx.transport)),
        ("client", json!(clip_str(&ctx.client, 80))),
        ("app", clip(args.get("app").unwrap_or(&Value::Null), 40).map(Value::String).unwrap_or(Value::Null)),
        ("ask", std::env::var("AGENT_WIKI_ASK").ok().filter(|s| !s.is_empty()).map(Value::String).unwrap_or(Value::Null)),
        ("tool", json!(name)),
        ("args", summarize_args(args).unwrap_or(Value::Null)),
        ("ms", json!(now_ms() - t0)),
        ("result", json!(result)),
        ("error", error.map(|e| Value::String(clip_str(&e, 300))).unwrap_or(Value::Null)),
    ]);
    text_result(text, is_error)
}

// ---------------------------------------------------------------- JSON-RPC

pub fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub fn is_request(m: &Value) -> bool {
    m.get("method").is_some() && m.get("id").is_some_and(|i| !i.is_null())
}

/// A JSON-RPC message the SDK accepts: request, notification or response.
pub fn valid_message(m: &Value) -> bool {
    let Some(o) = m.as_object() else { return false };
    if o.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return false;
    }
    if let Some(method) = o.get("method") {
        return method.is_string() && o.get("id").is_none_or(|i| i.is_string() || i.is_number());
    }
    o.get("id").is_some() && (o.contains_key("result") || o.contains_key("error"))
}

/// Handles one message; returns the response for requests. `initialized`: called on notifications/initialized.
pub fn handle(st: &State, ctx: &Ctx, msg: &Value, client_info: &Mutex<Option<String>>) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let id = id?; // notifications get no response
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if SUPPORTED_VERSIONS.contains(&asked) { asked } else { SUPPORTED_VERSIONS[0] };
            if let Some(ci) = params.get("clientInfo") {
                let name = ci.get("name").and_then(Value::as_str).unwrap_or("");
                let v = ci.get("version").and_then(Value::as_str).unwrap_or("");
                *client_info.lock().unwrap() = Some(format!("{name} {v}").trim().to_string());
            }
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "agent-wiki", "title": "Agent Wiki", "version": aw_core::VERSION },
                "instructions": instructions(st),
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools(st) }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            call_tool(st, ctx, name, params.get("arguments").unwrap_or(&Value::Null))
        }
        _ => return Some(rpc_error(id, -32601, "Method not found")),
    };
    Some(json!({ "result": result, "jsonrpc": "2.0", "id": id }))
}

pub fn now_iso() -> String {
    local_iso(&now())
}

#[cfg(test)]
mod tool_tests {
    use super::*;

    #[test]
    fn one_tool_list_with_the_direct_texts_and_the_core_sources() {
        let (curated, direct) = (tool_list(true), tool_list(false));
        assert_eq!(curated.len(), 5);
        let text: Value = serde_json::from_str(TOOLS_DIRECT_TEXT).unwrap();
        for (name, o) in text.as_object().unwrap() {
            assert!(curated.iter().any(|t| t["name"] == *name), "tools-direct-text.json names {name}, which tools.json does not have");
            assert!(o.as_object().unwrap().keys().all(|k| k == "title" || k == "description"), "only titles and descriptions differ: {name}");
        }
        for (c, d) in curated.iter().zip(&direct) {
            assert_eq!(c["inputSchema"], d["inputSchema"], "{}", c["name"]);
        }
        // The `source` enum the tools offer is the list the inbox accepts.
        for t in curated.iter().filter(|t| !t["inputSchema"]["properties"]["source"].is_null()) {
            let offered: Vec<&str> = t["inputSchema"]["properties"]["source"]["enum"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
            assert_eq!(offered, aw_core::inbox::SOURCES, "{}: keep the source enum equal to aw_core::inbox::SOURCES", t["name"]);
        }
    }
}
