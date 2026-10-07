//! `agent-wiki curator`: files queued notes into the wiki, and answers the window's questions (Ask),
//! as src/curator.mjs. It runs as the user (the tray hosts it), because it uses the user's ChatGPT
//! sign-in through the Codex CLI.
//!
//!   agent-wiki curator [--parent-stdin]   run until stopped; --parent-stdin: stop when stdin closes
//!   agent-wiki curator --once             file everything that is due now, ignoring the debounce, then exit
//!   agent-wiki curator --retry-dead       give failed (dead-lettered) notes another round of attempts
//!   agent-wiki curator --file-raw-dead    file failed notes into the log as they are, without the model
//!   agent-wiki curator --login-status     print {"signedIn":...} for the curator's Codex login
//!   agent-wiki curator --asks-only        answer the window's questions without filing notes

use aw_core::askworker::{AskCfg, AskWorker};
use aw_core::codex;
use aw_core::curator::{self, Curator, CuratorCfg, log};
use aw_core::inbox;
use aw_core::paths::{AppPaths, process_env};
use aw_core::reqlog::RequestLog;
use aw_core::waker::Waker;
use aw_core::wiki;
use serde_json::{Value, json};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn fatal(msg: &str) -> ! {
    log(&format!("fatal: {msg}"));
    std::process::exit(1)
}

pub fn run(args: &[String]) -> ! {
    let has = |f: &str| args.iter().any(|a| a == f);
    let env = process_env();
    let config = wiki::read_config(&env).unwrap_or_else(|e| fatal(&e.to_string()));
    aw_core::embed::configure(&config);
    let paths = AppPaths::current();
    let cfg = CuratorCfg::from_config(&config, &paths);
    let (wiki_dir, _) = wiki::resolve_wiki_dir(&env).unwrap_or_else(|e| fatal(&e.to_string()));
    wiki::ensure_wiki(&wiki_dir).unwrap_or_else(|e| fatal(&e.to_string()));
    let days = config["logs"]["retentionDays"].as_i64().filter(|d| *d > 0).unwrap_or(30);
    let reqlog = Arc::new(RequestLog::new(&paths.log_dir, "curator", days));

    if has("--login-status") {
        let (signed_in, detail) = codex::login_status(&cfg.model_cfg());
        println!("{}", json!({ "signedIn": signed_in, "detail": detail }));
        std::process::exit(0);
    }
    if has("--retry-dead") {
        let n = inbox::retry_notes(&wiki_dir, true).unwrap_or_else(|e| fatal(&e.to_string()));
        reqlog.write(vec![("kind", json!("curator")), ("event", json!("retry-dead")), ("notes", json!(n))]);
        println!("{}", json!({ "retried": n }));
        std::process::exit(0);
    }
    if has("--file-raw-dead") {
        let dead: Vec<String> = inbox::list_notes(&wiki_dir).unwrap_or_else(|e| fatal(&e.to_string())).into_iter().filter(|n| n.status == "dead").map(|n| n.id).collect();
        for id in &dead {
            if let Err(e) = curator::file_raw(&wiki_dir, id) {
                fatal(&format!("{e:?}"));
            }
        }
        reqlog.write(vec![("kind", json!("curator")), ("event", json!("file-raw")), ("notes", json!(dead))]);
        println!("{}", json!({ "filed": dead.len() }));
        std::process::exit(0);
    }

    if has("--lint") {
        // Review the named pages (or every page that is due) now, then exit.
        let named: Vec<String> = args.iter().skip_while(|a| *a != "--lint").skip(1).take_while(|a| !a.starts_with("--")).cloned().collect();
        let c = Curator::new(&wiki_dir, cfg.clone(), reqlog.clone(), true, Waker::new());
        let mut proposed = 0;
        let mut reviewed = vec![];
        if named.is_empty() {
            while let Some(slug) = aw_core::lint::next_due(&wiki_dir, &cfg) {
                proposed += c.lint_one(&slug);
                reviewed.push(slug);
            }
        } else {
            for slug in &named {
                proposed += c.lint_one(slug);
                reviewed.push(slug.clone());
            }
        }
        println!("{}", json!({ "reviewed": reviewed, "proposed": proposed }));
        std::process::exit(0);
    }
    let once = has("--once");
    let asks_only = has("--asks-only");
    let curator = (!asks_only).then(|| Arc::new(Curator::new(&wiki_dir, cfg.clone(), reqlog.clone(), once, Waker::new())));
    // The same process answers the window's questions (Ask): it is the one that runs as the user.
    let asker = (!once).then(|| {
        let server = std::env::current_exe().unwrap_or_else(|_| "agent-wiki".into());
        Arc::new(AskWorker::new(&wiki_dir, AskCfg::from_config(&config, &cfg), reqlog.clone(), server, paths.clone(), Waker::new()))
    });
    let stop = {
        let (curator, asker) = (curator.clone(), asker.clone());
        move || {
            if let Some(c) = &curator {
                c.abort.store(true, Ordering::SeqCst);
                c.waker.stop();
            }
            if let Some(a) = &asker {
                a.stop();
            }
        }
    };
    if has("--parent-stdin") {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut sink = [0u8; 4096];
            let mut stdin = std::io::stdin();
            while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
            stop();
        });
    }
    signals::on_stop(stop);
    let chosen = cfg.with_choice(&wiki_dir);
    log(&format!(
        "v{} started: wiki {}, model {} ({}; the window can change it), codex {}, CODEX_HOME {}{}",
        aw_core::VERSION,
        wiki::disp(&wiki_dir),
        chosen.model,
        chosen.reasoning_effort,
        cfg.codex_path,
        wiki::disp(&cfg.codex_home),
        if curator.is_some() { "" } else { ", answering questions only" }
    ));
    reqlog.write(vec![
        ("kind", json!("curator")),
        ("event", json!("start")),
        ("model", json!(chosen.model)),
        ("once", if once { json!(true) } else { Value::Null }),
        ("asksOnly", if curator.is_none() { json!(true) } else { Value::Null }),
    ]);
    let ask_thread = asker.clone().map(|a| std::thread::spawn(move || a.run()));
    let mut failed = None;
    if let Some(c) = &curator
        && let Err(e) = c.run()
    {
        failed = Some(format!("{e:?}"));
        if let Some(a) = &asker {
            a.stop();
        }
    }
    if let Some(t) = ask_thread {
        let _ = t.join();
    }
    reqlog.write(vec![("kind", json!("curator")), ("event", json!("stop"))]);
    if let Some(e) = failed {
        fatal(&e);
    }
    log("stopped");
    std::process::exit(0)
}

/// SIGINT and SIGTERM stop gracefully (heartbeat "stopped", locks released), as the Node curator does.
mod signals {
    #[cfg(unix)]
    pub fn on_stop(stop: impl Fn() + Send + 'static) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static GOT: AtomicBool = AtomicBool::new(false);
        extern "C" fn handler(_: libc::c_int) {
            GOT.store(true, Ordering::SeqCst);
        }
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        }
        std::thread::spawn(move || {
            while !GOT.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            stop();
        });
    }

    #[cfg(not(unix))]
    pub fn on_stop(_stop: impl Fn() + Send + 'static) {
        // Windows: the tray stops the curator by closing its stdin (--parent-stdin).
    }
}
