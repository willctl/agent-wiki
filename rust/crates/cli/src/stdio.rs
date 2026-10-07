//! `serve` over stdio: one JSON-RPC message per line on stdin, responses on stdout (and nothing else
//! there: logging goes to stderr). Requests run concurrently on a few worker threads, as the Node
//! server's async handlers do, so a slow call never holds up a quick one. Ends when stdin closes.

use crate::mcp::{self, Ctx, log};
use aw_core::reqlog::{RequestLog, clip_str};
use aw_core::wiki;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, mpsc};

const WORKERS: usize = 8;

pub fn run(read_only: bool) -> ! {
    let st = Arc::new(if read_only { mcp::setup_read_only() } else { mcp::setup() });
    let proc_name = if read_only { std::env::var("AGENT_WIKI_PROCESS").unwrap_or_else(|_| "read-only".into()) } else { "stdio".into() };
    let reqlog: Arc<RequestLog> = Arc::new(mcp::open_request_log(&proc_name, &st.config));
    let client_info: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let mode = if read_only { "read-only".to_string() } else { format!("{} writes", if st.curated { "curated" } else { "direct" }) };
    let shown = st.wiki_dir.as_ref().map(|d| wiki::disp(d)).unwrap_or_else(|| "(unavailable)".into());
    log(&format!("v{} ready (stdio, {mode}); wiki at {shown}", aw_core::VERSION));

    let (tx, rx) = mpsc::channel::<Value>();
    let rx = Arc::new(Mutex::new(rx));
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (rx, st, reqlog, client_info, stdout) = (rx.clone(), st.clone(), reqlog.clone(), client_info.clone(), stdout.clone());
            std::thread::spawn(move || {
                loop {
                    let Ok(m) = rx.lock().unwrap().recv() else { break };
                    let client = client_info.lock().unwrap().clone().unwrap_or_default();
                    let ctx = Ctx { transport: "stdio", rid: None, client, reqlog: &reqlog, on_error: None };
                    let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mcp::handle(&st, &ctx, &m, &client_info)))
                        .unwrap_or_else(|_| m.get("id").cloned().map(|id| mcp::rpc_error(id, -32603, "Internal error")));
                    if let Some(resp) = resp {
                        let mut out = stdout.lock().unwrap();
                        let _ = writeln!(out, "{resp}");
                        let _ = out.flush();
                    }
                }
            })
        })
        .collect();

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            log("ignored a line that is not JSON");
            continue;
        };
        let messages = match msg {
            Value::Array(a) => a,
            other => vec![other],
        };
        for m in messages {
            match m.get("method").and_then(Value::as_str) {
                Some("notifications/initialized") => {
                    let client = client_info.lock().unwrap().clone().unwrap_or_default();
                    reqlog.write(vec![("kind", json!("session")), ("transport", json!("stdio")), ("client", json!(clip_str(&client, 80))), ("result", json!("initialized"))]);
                }
                // initialize sets the client name the later calls are logged with: answer it in order.
                Some("initialize") => {
                    let ctx = Ctx { transport: "stdio", rid: None, client: String::new(), reqlog: &reqlog, on_error: None };
                    if let Some(resp) = mcp::handle(&st, &ctx, &m, &client_info) {
                        let mut out = stdout.lock().unwrap();
                        let _ = writeln!(out, "{resp}");
                        let _ = out.flush();
                    }
                }
                _ => {
                    let _ = tx.send(m);
                }
            }
        }
    }
    drop(tx);
    for w in workers {
        let _ = w.join();
    }
    let _ = stdout.lock().unwrap().flush();
    std::process::exit(0)
}
