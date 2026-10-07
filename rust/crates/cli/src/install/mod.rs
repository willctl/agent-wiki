//! `agent-wiki install` / `uninstall`: sets Agent Wiki up for every AI app on this computer, and takes
//! it out again. Idempotent: re-running upgrades in place and never touches wiki content. Every
//! existing config file is backed up once as <file>.bak-agent-wiki and merged, never replaced.
//!
//!   agent-wiki install [--wiki-dir <path>] [--port N] [--no-approve] [--from <dir>] [--sandbox <dir>]
//!   agent-wiki uninstall [--sandbox <dir>]
//!   agent-wiki install-service [--user-sid S-1-...] [--log <file>]   (Windows, elevated; install runs it)
//!   agent-wiki uninstall-service                                    (Windows, elevated)
//!
//! --from: the folder with agent-wiki, agent-wiki-tray, ui/ and icons/ (default: next to this program;
//! in the repository, rust/target/release with dist/ built). --sandbox: a fake home for tests: files
//! and configs only, no service, no sign-in entries, no app CLIs.

mod files;
mod places;
#[cfg(unix)]
mod posix;
mod sys;
#[cfg(windows)]
mod windows;

use files::{
    CLAUDE_ALLOW, POINTER, SERVER, backup_once, claude_allow_remove, claude_allow_set, edit_file, read_json, read_text, remove_block, render_plugin, to_json, toml_remove_approval, toml_set_approval,
    tree_hash, upsert_block, write_lf,
};
use places::{EXE, Places, fwd};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use sys::{claude_desktop_running, run, run_in, which};

#[derive(Debug)]
pub struct InstallError(pub String);

pub fn info(msg: &str) {
    println!("    {msg}");
}
pub fn warn(msg: &str) {
    println!("    ! {msg}");
}
fn step(n: &str, title: &str) {
    println!("\n[{n}] {title}");
}

const PLUGIN: &str = "agent-wiki";
const CLAUDE_MARKET: &str = "agent-wiki-local";
const CLAUDE_PLUGIN_ID: &str = "agent-wiki@agent-wiki-local";
const DEFAULT_PORT: u16 = 47821;
const AUTHOR: &str = "willctl";

struct Opts {
    wiki_dir: Option<PathBuf>,
    port: Option<u16>,
    no_approve: bool,
    from: Option<PathBuf>,
    sandbox: Option<PathBuf>,
}

/// The environment the installer computes locations from: the real one, or a fake home (--sandbox).
fn environment(sandbox: Option<&Path>) -> (aw_core::paths::Env, Vec<(String, String)>) {
    let mut env = aw_core::paths::process_env();
    let mut over: Vec<(String, String)> = vec![];
    if let Some(s) = sandbox {
        let p = |rel: &str| s.join(rel).to_string_lossy().into_owned();
        over = vec![
            ("HOME".into(), p("")),
            ("USERPROFILE".into(), p("")),
            ("APPDATA".into(), p("AppData/Roaming")),
            ("LOCALAPPDATA".into(), p("AppData/Local")),
            ("XDG_CONFIG_HOME".into(), p(".config")),
            ("XDG_DATA_HOME".into(), p(".local/share")),
            ("XDG_STATE_HOME".into(), p(".local/state")),
            ("XDG_CACHE_HOME".into(), p(".cache")),
            ("CODEX_HOME".into(), p(".codex")),
        ];
        for (k, v) in &over {
            env.insert(k.clone(), v.clone());
        }
    }
    (env, over)
}

/// Where the programs and the window's files come from.
struct Payload {
    agent: PathBuf,
    tray: PathBuf,
    ui: PathBuf,
    icons: PathBuf,
    loader: Option<PathBuf>,
}

fn payload(from: Option<&Path>) -> Result<Payload, InstallError> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    let dir = from.map(Path::to_path_buf).unwrap_or(exe_dir);
    // In the repository, the binaries are in rust/target/release and the window's files in dist/.
    let repo = dir.ancestors().nth(3).map(Path::to_path_buf);
    let pick = |own: PathBuf, dev: Option<PathBuf>| if own.exists() { own } else { dev.filter(|d| d.exists()).unwrap_or(own) };
    let p = Payload {
        agent: dir.join(format!("agent-wiki{EXE}")),
        tray: dir.join(format!("agent-wiki-tray{EXE}")),
        ui: pick(dir.join("ui"), repo.as_ref().map(|r| r.join("dist").join("runtime").join("ui"))),
        icons: pick(dir.join("icons"), repo.as_ref().map(|r| r.join("dist").join("icons"))),
        loader: Some(dir.join("WebView2Loader.dll")).filter(|l| cfg!(windows) && l.exists()),
    };
    for (what, path) in
        [("agent-wiki", &p.agent), ("agent-wiki-tray", &p.tray), ("the window's files (ui/index.html)", &p.ui.join("index.html")), ("the tray icons", &p.icons.join("agent-wiki-healthy.ico"))]
    {
        if !path.exists() {
            return Err(InstallError(format!("{what} not found at {} (build with `npm run build && npm run icons` and `cargo build --release`, or pass --from <folder>)", fwd(path))));
        }
    }
    Ok(p)
}

/// Puts `src` at `dest` unless it is already there. A running program cannot be overwritten on
/// Windows, but it can be renamed: it is moved aside (and removed once nothing runs it), and what runs
/// it notices the new file (the service and the tray's curator restart on it).
fn replace_file(src: &Path, dest: &Path) -> Result<&'static str, InstallError> {
    let next = std::fs::read(src).map_err(|e| InstallError(format!("cannot read {}: {e}", fwd(src))))?;
    if std::fs::read(dest).ok().as_deref() == Some(&next[..]) {
        return Ok("unchanged");
    }
    let err = |e: std::io::Error| InstallError(format!("cannot install {}: {e}", fwd(dest)));
    std::fs::create_dir_all(dest.parent().unwrap()).map_err(err)?;
    let fresh = PathBuf::from(format!("{}.new-{}", dest.display(), std::process::id()));
    std::fs::write(&fresh, &next).map_err(err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755));
    }
    let how = match std::fs::rename(&fresh, dest) {
        Ok(()) => "installed",
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5) | Some(32)) => {
            std::fs::rename(dest, format!("{}.old-{}", dest.display(), aw_core::text::now_ms())).map_err(err)?;
            std::fs::rename(&fresh, dest).map_err(err)?;
            "replaced (the running one was moved aside)"
        }
        Err(e) => return Err(err(e)),
    };
    remove_old_copies(dest);
    Ok(how)
}

/// Removes `<file>.old-*` copies that nothing runs any more.
fn remove_old_copies(file: &Path) {
    let (Some(dir), Some(base)) = (file.parent(), file.file_name().map(|b| b.to_string_lossy().into_owned())) else { return };
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with(&format!("{base}.old-")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Makes `to` hold exactly `from`'s files (unchanged ones are not rewritten).
fn sync_dir(from: &Path, to: &Path) -> Result<usize, InstallError> {
    let err = |e: std::io::Error| InstallError(format!("cannot copy {} to {}: {e}", fwd(from), fwd(to)));
    std::fs::create_dir_all(to).map_err(err)?;
    let mut names = vec![];
    let mut n = 0;
    for e in std::fs::read_dir(from).map_err(err)?.flatten() {
        let name = e.file_name();
        names.push(name.clone());
        let (src, dst) = (e.path(), to.join(&name));
        if src.is_dir() {
            n += sync_dir(&src, &dst)?;
            continue;
        }
        let next = std::fs::read(&src).map_err(err)?;
        if std::fs::read(&dst).ok().as_deref() != Some(&next[..]) {
            let tmp = to.join(format!("{}.new-{}", name.to_string_lossy(), std::process::id()));
            std::fs::write(&tmp, &next).map_err(err)?;
            std::fs::rename(&tmp, &dst).map_err(err)?;
        }
        n += 1;
    }
    for e in std::fs::read_dir(to).map_err(err)?.flatten() {
        if !names.contains(&e.file_name()) {
            let p = e.path();
            let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        }
    }
    Ok(n)
}

fn sha12(file: &Path) -> String {
    aw_core::text::sha256_hex(&std::fs::read(file).unwrap_or_default())[..12].to_string()
}

fn hook_command(p: &Places) -> String {
    #[cfg(windows)]
    let t = windows::shell_token(&p.agent);
    #[cfg(unix)]
    let t = posix::shell_token(&p.agent);
    format!("{t} hook")
}

struct Results(Vec<(String, String, String)>);

impl Results {
    fn note(&mut self, area: &str, status: &str, detail: impl Into<String>) {
        self.0.push((area.into(), status.into(), detail.into()));
    }
}

pub fn main(cmd: &str, args: &[String]) -> ! {
    let has = |f: &str| args.iter().any(|a| a == f);
    let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let opts = Opts {
        wiki_dir: opt("--wiki-dir").map(PathBuf::from),
        port: opt("--port").and_then(|p| p.parse().ok()),
        no_approve: has("--no-approve"),
        from: opt("--from").map(PathBuf::from),
        sandbox: opt("--sandbox").map(PathBuf::from),
    };
    let r = match cmd {
        "install" => install(&opts),
        "uninstall" => uninstall(&opts),
        #[cfg(windows)]
        "install-service" => service::install(opt("--user-sid"), opt("--log").map(PathBuf::from)),
        #[cfg(windows)]
        "uninstall-service" => service::uninstall(),
        _ => Err(InstallError(format!("{cmd}: not available on this system"))),
    };
    match r {
        Ok(()) => std::process::exit(0),
        Err(InstallError(m)) => {
            eprintln!("\n{} failed: {m}", if cmd.starts_with("un") { "Uninstall" } else { "Install" });
            std::process::exit(1)
        }
    }
}

fn load_state(p: &Places) -> Value {
    read_json(&p.state).ok().flatten().filter(Value::is_object).unwrap_or_else(|| json!({ "created": [] }))
}

fn created_list(state: &Value) -> Vec<String> {
    state["created"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------- install

/// Another app's JSON file, which must hold an object: indexing anything else would panic, and the
/// installer would stop halfway with no word about which file.
fn parse_object(text: &str, file: &Path) -> Result<Value, InstallError> {
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err(InstallError(format!("{} is not a JSON object: fix or remove it, then install again", fwd(file)))),
        Err(e) => Err(InstallError(format!("{} is not valid JSON ({e})", fwd(file)))),
    }
}

/// Whether Claude desktop is running (it would write its config back over an edit). In a --sandbox
/// test, a claude-desktop-running file in the fake home stands for it.
fn desktop_running(sandbox: Option<&Path>) -> bool {
    match sandbox {
        Some(s) => s.join("claude-desktop-running").exists(),
        None => claude_desktop_running(),
    }
}

fn install(o: &Opts) -> Result<(), InstallError> {
    let (env, child_env) = environment(o.sandbox.as_deref());
    let p = Places::new(&env);
    let sandbox = o.sandbox.is_some();
    let mut res = Results(vec![]);
    let mut state = load_state(&p);
    let mut created = created_list(&state);
    let mark_created = |f: &Path, created: &mut Vec<String>| {
        let f = fwd(f);
        if !created.contains(&f) {
            created.push(f);
        }
    };
    println!("Agent Wiki installer v{}{}", aw_core::VERSION, if sandbox { " (sandbox)" } else { "" });
    #[cfg(windows)]
    if !sandbox && let Some(pkg) = windows::packaged() {
        return Err(InstallError(format!(
            "This terminal runs inside the app package {pkg}. Windows redirects what it writes to AppData into that app's private storage, where the service, the tray at sign-in and the other AI apps cannot see it. Nothing was changed.\nRun it from a normal terminal instead: press Win+R, type cmd, press Enter, then run this again."
        )));
    }
    let pay = payload(o.from.as_deref())?;

    step("1", "Programs and the window's files");
    info(&format!("{}: {}", fwd(&p.agent), replace_file(&pay.agent, &p.agent)?));
    let n = sync_dir(&pay.ui, &p.runtime.join("ui"))?;
    info(&format!("window files -> {} ({n} files)", fwd(&p.runtime.join("ui"))));

    step("2", "config.json and the wiki");
    let prev = read_json(&p.config)?;
    let prev_s = |k: &str| prev.as_ref().and_then(|v| v[k].as_str()).map(String::from);
    let wiki_dir = o.wiki_dir.clone().map(|d| std::path::absolute(&d).unwrap_or(d)).or_else(|| prev_s("wikiDir").map(PathBuf::from)).unwrap_or_else(|| p.default_wiki.clone());
    if let Some(old) = prev_s("wikiDir")
        && Path::new(&old) != wiki_dir
    {
        warn(&format!("wiki folder changed from {old} (old folder left untouched)"));
    }
    let port = o.port.or_else(|| prev.as_ref().and_then(|v| v["httpPort"].as_u64()).and_then(|x| u16::try_from(x).ok())).unwrap_or(DEFAULT_PORT);
    let codex_cli = if sandbox { None } else { which("codex") };
    let prev_cur = prev.as_ref().map(|v| v["curator"].clone()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let mut curator = Map::new();
    curator.insert("model".into(), json!("gpt-6.1-sol"));
    curator.insert("reasoningEffort".into(), json!("medium"));
    for (k, v) in prev_cur.as_object().unwrap() {
        curator.insert(k.clone(), v.clone());
    }
    let codex_path = prev_cur["codexPath"].as_str().map(String::from).or_else(|| codex_cli.as_ref().map(|c| fwd(c))).unwrap_or_else(|| "codex".into());
    let codex_home = prev_cur["codexHome"].as_str().map(PathBuf::from).unwrap_or_else(|| p.curator_codex_home.clone());
    curator.insert("codexPath".into(), json!(codex_path.replace('\\', "/")));
    curator.insert("codexHome".into(), json!(fwd(&codex_home)));
    let mut config = prev.clone().filter(Value::is_object).unwrap_or_else(|| json!({}));
    let c = config.as_object_mut().unwrap();
    c.insert("wikiDir".into(), json!(fwd(&wiki_dir)));
    c.insert("version".into(), json!(aw_core::VERSION));
    c.insert("httpPort".into(), json!(port));
    let write_mode = prev_s("writeMode").unwrap_or_else(|| "curated".into());
    c.insert("writeMode".into(), json!(write_mode));
    c.insert("curator".into(), Value::Object(curator));
    let mut logs = json!({ "retentionDays": 30 });
    if let Some(Value::Object(l)) = prev.as_ref().map(|v| v["logs"].clone()) {
        for (k, v) in l {
            logs[k] = v;
        }
    }
    c.insert("logs".into(), logs);
    write_lf(&p.config, &to_json(&config))?;
    info(&format!("config -> {} (writes: {write_mode})", fwd(&p.config)));
    aw_core::wiki::ensure_wiki(&wiki_dir).map_err(|e| InstallError(format!("cannot set up the wiki at {}: {e}", fwd(&wiki_dir))))?;
    info(&format!("wiki -> {}", fwd(&wiki_dir)));
    match aw_core::wiki::refresh_protocol(&wiki_dir) {
        Ok(true) => info("PROTOCOL.md -> this release's (you had not edited it; the old one is in .history/)"),
        Ok(false) => {}
        Err(e) => warn(&format!("PROTOCOL.md left as it is: {e}")),
    }

    step("3", "The service (HTTP on 127.0.0.1 for every app)");
    let mcp_url = format!("http://127.0.0.1:{port}/mcp");
    let mut transport = "stdio";
    if sandbox {
        info("sandbox: no service; apps start the server themselves (stdio)");
    } else {
        transport = service_step(&p, &wiki_dir, port, write_mode == "curated")?;
    }
    res.note("Server transport", if transport == "http" { "ok" } else { "stdio" }, if transport == "http" { format!("service at {mcp_url}") } else { "per-app stdio (service not running)".into() });

    step("4", "Curator: its own Codex home and sign-in");
    std::fs::create_dir_all(&codex_home).map_err(|e| InstallError(e.to_string()))?;
    if write_mode != "curated" {
        info("writeMode is \"direct\": notes are written immediately; the curator has nothing to do");
        res.note("Curator", "off", "writeMode \"direct\"");
    } else if sandbox {
        info("sandbox: sign-in not checked");
    } else if codex_cli.is_none() && prev_cur["codexPath"].is_null() {
        warn("codex CLI not found: the curator cannot run. Notes will queue in the wiki inbox until it can.");
        res.note("Curator", "manual", "codex CLI not found");
    } else {
        let cfg = aw_core::codex::ModelCfg { model: String::new(), reasoning_effort: String::new(), codex_path: codex_path.clone(), codex_home: codex_home.clone(), timeout_seconds: 30.0 };
        let (signed_in, _) = aw_core::codex::login_status(&cfg);
        info(&format!("CODEX_HOME {}: {}", fwd(&codex_home), if signed_in { "signed in" } else { "not signed in yet" }));
        res.note(
            "Curator",
            if signed_in { "ok" } else { "click" },
            if signed_in { "signed in".to_string() } else { "sign in once: tray icon > Curator > Sign in to ChatGPT for the curator...".into() },
        );
    }

    step("5", "Tray app and the Agent Wiki window");
    info(&format!("{}: {}", fwd(&p.tray_exe), replace_file(&pay.tray, &p.tray_exe)?));
    if let Some(l) = &pay.loader {
        replace_file(l, &p.tray_dir.join("WebView2Loader.dll"))?;
    }
    sync_dir(&pay.icons, &p.icons)?;
    let n = |x: &Path| if cfg!(windows) { x.to_string_lossy().replace('/', "\\") } else { x.to_string_lossy().into_owned() };
    let mut ini = vec![
        "# Written by the agent-wiki installer. Read by agent-wiki-tray.".to_string(),
        format!("agent={}", n(&p.agent)),
        format!("wikiDir={}", n(&wiki_dir)),
        format!("logDir={}", n(&p.app.log_dir)),
        format!("icons={}", n(&p.icons)),
        format!("webviewDir={}", n(&p.webview)),
        format!("port={port}"),
        format!("codex={}", n(Path::new(&codex_path))),
        format!("codexHome={}", n(&codex_home)),
        format!("curator={}", if cfg!(windows) && write_mode == "curated" { 1 } else { 0 }),
    ];
    #[cfg(windows)]
    ini.extend([format!("service={}", windows::SERVICE_NAME), format!("task={}", windows::TRAY_TASK), format!("taskXml={}", n(&p.task_xml)), format!("runValue={}", windows::TRAY_RUN_VALUE)]);
    ini.push(String::new());
    write_lf(&p.tray_ini, &ini.join("\n"))?;
    if sandbox {
        info("sandbox: the tray is not started or registered");
    } else {
        tray_step(&p, &mut state, &mut res)?;
    }

    step("6", "Plugin and local marketplaces");
    let hook = hook_command(&p);
    let map = vec![
        ("__VERSION__", aw_core::VERSION.to_string()),
        ("__HOOK_COMMAND__", hook.clone()),
        ("__POINTER__", POINTER.trim().to_string()),
        ("__PROTOCOL__", aw_core::wiki::DEFAULT_PROTOCOL.replace("\r\n", "\n").trim().to_string()),
        ("__NODE__", String::new()),
        ("__RUNTIME__", String::new()),
    ];
    let server = if transport == "http" { json!({ "type": "http", "url": mcp_url }) } else { json!({ "command": fwd(&p.agent), "args": ["serve"] }) };
    let rendered = render_plugin(&p.rendered, &map, &server)?;
    info(&format!("plugin -> {} ({} files, {})", fwd(&p.rendered), rendered.len(), if transport == "http" { format!("MCP over HTTP {mcp_url}") } else { "MCP over stdio".into() }));
    info(&format!("hook command: {hook}"));
    let description = "Shared long-term memory across Claude, ChatGPT and other AI apps.";
    write_lf(
        &p.market.join(".claude-plugin").join("marketplace.json"),
        &to_json(&json!({
            "name": CLAUDE_MARKET, "owner": { "name": AUTHOR }, "metadata": { "description": "Local marketplace for the Agent Wiki plugin" },
            "plugins": [{ "name": PLUGIN, "source": "./plugins/agent-wiki", "description": description, "version": aw_core::VERSION, "author": { "name": AUTHOR } }],
        })),
    )?;
    write_lf(
        &p.market.join(".agents").join("plugins").join("marketplace.json"),
        &to_json(&json!({
            "name": CLAUDE_MARKET, "interface": { "displayName": "Agent Wiki (local)" },
            "plugins": [{ "name": PLUGIN, "source": { "source": "local", "path": "./plugins/agent-wiki" }, "policy": { "installation": "INSTALLED_BY_DEFAULT", "authentication": "ON_INSTALL" }, "category": "Productivity" }],
        })),
    )?;
    let claude = if sandbox { None } else { which("claude") };
    if let Some(cl) = &claude {
        for target in [&p.rendered, &p.market] {
            let v = run(cl, &["plugin", "validate", &target.to_string_lossy()]);
            if !v.ok {
                return Err(InstallError(format!("claude plugin validate {} failed:\n{}{}", fwd(target), v.stdout, v.stderr)));
            }
        }
        info("claude plugin validate: plugin and marketplace OK");
    }

    step("7", "ChatGPT desktop / Codex: personal marketplace");
    let rel = format!("./{}", fwd(p.rendered.strip_prefix(&p.home).unwrap_or(&p.rendered)));
    if !p.personal_market.exists() {
        mark_created(&p.personal_market, &mut created);
    }
    let mut personal_name = "personal".to_string();
    edit_file(&p.personal_market, |text| {
        let mut m: Value = match text {
            Some(t) => parse_object(t, &p.personal_market)?,
            None => json!({ "name": "personal", "interface": { "displayName": "Personal" }, "plugins": [] }),
        };
        personal_name = m["name"].as_str().unwrap_or("personal").to_string();
        let entry =
            json!({ "name": PLUGIN, "source": { "source": "local", "path": rel }, "policy": { "installation": "INSTALLED_BY_DEFAULT", "authentication": "ON_INSTALL" }, "category": "Productivity" });
        if !m["plugins"].is_array() {
            m["plugins"] = json!([]);
        }
        let list = m["plugins"].as_array_mut().unwrap();
        match list.iter().position(|x| x["name"] == PLUGIN) {
            Some(i) => list[i] = entry,
            None => list.push(entry),
        }
        Ok(Some(to_json(&m)))
    })?;
    info(&format!("{}: entry {PLUGIN} -> {rel}", fwd(&p.personal_market)));
    let codex_id = format!("{PLUGIN}@{personal_name}");
    state["codexPluginId"] = json!(codex_id);
    let codex = if sandbox { None } else { which("codex") };
    if let Some(cx) = &codex {
        backup_once(&p.codex_config)?;
        let markets = run(cx, &["plugin", "marketplace", "list"]);
        if !markets.stdout.contains(&personal_name) {
            return Err(InstallError(format!("codex does not list the personal marketplace \"{personal_name}\":\n{}{}", markets.stdout, markets.stderr)));
        }
        let add = run(cx, &["plugin", "add", &codex_id]);
        if !add.ok {
            return Err(InstallError(format!("codex plugin add {codex_id} failed:\n{}{}", add.stdout, add.stderr)));
        }
        info(&format!("codex plugin add {codex_id}: {}", add.last_line()));
        let list = run(cx, &["plugin", "list", "--json"]);
        res.note("ChatGPT/Codex plugin", if list.ok && list.stdout.contains(&codex_id) { "ok" } else { "check" }, format!("{codex_id} installed"));
    } else if !sandbox {
        warn("codex CLI not found: install the plugin from the ChatGPT app (Plugins > Personal).");
        res.note("ChatGPT/Codex plugin", "manual", "codex CLI not found");
    }

    step("8", "Claude Code plugin and Claude desktop");
    if let Some(cl) = &claude {
        claude_plugin(cl, &p, &mut res)?;
    } else if !sandbox {
        warn("claude CLI not found: skipped the Claude Code plugin.");
        res.note("Claude Code plugin", "manual", "claude CLI not found");
    }
    let desktop = p.claude_desktop_configs();
    if desktop.is_empty() {
        res.note("Claude desktop MCP", "manual", "no Claude desktop config folder found");
    }
    let known: Vec<String> = state["claudeDesktopConfigs"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    // A running Claude desktop writes its own copy of the file back later, with the entry it started with.
    let entry = json!({ "command": fwd(&p.agent), "args": ["serve"] });
    let desktop_busy = !desktop.is_empty() && desktop_running(o.sandbox.as_deref());
    if desktop_busy && desktop.iter().all(|cfg| read_json(cfg).ok().flatten().is_some_and(|c| c["mcpServers"][SERVER] == entry)) {
        info("Claude desktop is running and its config already has this install's agent-wiki entry: left alone.");
        res.note("Claude desktop MCP", "ok", "already registered (Claude desktop is running, so its config was left alone)");
    } else if desktop_busy {
        warn("Claude desktop is running: it keeps its config in memory and writes it back, so the agent-wiki entry was not added. Quit Claude desktop (its tray icon > Quit), then install again.");
        res.note("Claude desktop MCP", "manual", "quit Claude desktop (tray icon > Quit), then install again");
    }
    for cfg in desktop.iter().filter(|_| !desktop_busy) {
        if !cfg.exists() {
            mark_created(cfg, &mut created);
        }
        let before = read_json(cfg).ok().flatten();
        if known.contains(&fwd(cfg)) && before.as_ref().is_none_or(|b| b["mcpServers"][SERVER].is_null()) {
            warn("Claude desktop removed the agent-wiki entry since the last install (\"Allow user-added MCP servers\" may be off for this account). Re-adding it anyway.");
        }
        edit_file(cfg, |text| {
            let mut c: Value = match text {
                Some(t) => parse_object(t, cfg)?,
                None => json!({}),
            };
            if !c["mcpServers"].is_object() {
                c["mcpServers"] = json!({});
            }
            c["mcpServers"][SERVER] = entry.clone();
            Ok(Some(to_json(&c)))
        })?;
        info(&format!("registered {SERVER} in {}", fwd(cfg)));
        res.note("Claude desktop MCP", "ok", fwd(cfg));
    }
    state["claudeDesktopConfigs"] = json!(desktop.iter().map(|d| fwd(d)).collect::<Vec<_>>());

    step("9", "Global instructions");
    for file in [&p.codex_agents, &p.claude_md] {
        if !file.exists() {
            mark_created(file, &mut created);
        }
        let changed = edit_file(file, |t| Ok(Some(upsert_block(t, POINTER))))?;
        info(&format!("{}: managed block {}", fwd(file), if changed { "written" } else { "already current" }));
    }

    step("10", "Tool approval: ChatGPT/Codex and Claude Code");
    if o.no_approve {
        let a = edit_file(&p.codex_config, |t| toml_remove_approval(t, &codex_id))?;
        info(if a { "removed ChatGPT/Codex auto-approval" } else { "ChatGPT/Codex auto-approval not set (--no-approve)" });
        let b = edit_file(&p.claude_settings, claude_allow_remove)?;
        info(if b { "removed the Claude Code allow rules" } else { "Claude Code allow rules not set (--no-approve)" });
    } else {
        let a = edit_file(&p.codex_config, |t| toml_set_approval(t, &codex_id))?;
        info(&format!("{}: [plugins.\"{codex_id}\".mcp_servers.{SERVER}] default_tools_approval_mode = \"approve\" ({})", fwd(&p.codex_config), if a { "written" } else { "already set" }));
        let b = edit_file(&p.claude_settings, claude_allow_set)?;
        info(&format!("{}: permissions.allow has {} ({})", fwd(&p.claude_settings), CLAUDE_ALLOW.join(", "), if b { "written" } else { "already set" }));
    }
    if let Some(cx) = &codex {
        let check = run(cx, &["plugin", "list", "--json"]);
        if !check.ok {
            return Err(InstallError(format!("codex cannot load config.toml after the edit; restore {}.bak-agent-wiki:\n{}", fwd(&p.codex_config), check.stderr)));
        }
    }

    step("11", "Paste-in text for app settings");
    write_lf(&p.paste, &format!("{}\n", POINTER.trim()))?;
    #[cfg(windows)]
    let clip = if sandbox { Err("sandbox".to_string()) } else { windows::set_clipboard_from_file(&p.paste) };
    #[cfg(not(windows))]
    let clip: Result<(), String> = Err("not copied".to_string());
    info(&format!("{} written{}", fwd(&p.paste), if clip.is_ok() { " and copied to the clipboard" } else { "" }));

    step("12", "Self-test of the installed server and hook");
    self_test(&p, &wiki_dir, port, transport, &hook, &child_env)?;

    state["version"] = json!(aw_core::VERSION);
    state["installedAt"] = json!(aw_core::text::local_iso(&aw_core::text::now()));
    state["wikiDir"] = json!(fwd(&wiki_dir));
    state["transport"] = json!(transport);
    state["created"] = json!(created);
    write_lf(&p.state, &to_json(&state))?;

    println!("\nInstalled.");
    for (area, status, detail) in &res.0 {
        let d: String = if detail.chars().count() > 160 { format!("{}...", detail.chars().take(157).collect::<String>()) } else { detail.clone() };
        println!("  {status:<6} {area}: {d}");
    }
    println!("\n  Wiki:    {}", fwd(&wiki_dir));
    println!("  Runtime: {}", fwd(&p.runtime));
    println!("  Server:  {}", if transport == "http" { format!("service at {mcp_url}") } else { "launched by each app over stdio".into() });
    println!("\n  Restart the Claude and ChatGPT desktop apps to load the plugin and server.");
    println!("  In ChatGPT, accept the Agent Wiki hook trust prompt when it appears.");
    println!("  Paste {} into each app's personal instructions.", fwd(&p.paste));
    Ok(())
}

/// The service: registered (one UAC prompt on Windows the first time), running this build, healthy.
#[cfg(windows)]
fn service_step(p: &Places, wiki_dir: &Path, port: u16, _curated: bool) -> Result<&'static str, InstallError> {
    let n = |x: &Path| x.to_string_lossy().replace('/', "\\");
    write_lf(
        &p.service_ini,
        &[
            "# Written by the agent-wiki installer. Read by the service (agent-wiki service).".to_string(),
            format!("wikiDir={}", n(wiki_dir)),
            format!("configDir={}", n(&p.app.config_dir)),
            format!("dataDir={}", n(&p.app.data_dir)),
            format!("stateDir={}", n(&p.app.state_dir)),
            format!("port={port}"),
            format!("logDir={}", n(&p.app.log_dir)),
            // Who may hand the service the embeddings key (keypipe.rs): you.
            format!("owner={}", windows::user_sid().unwrap_or_default()),
            String::new(),
        ]
        .join("\n"),
    )?;
    std::fs::create_dir_all(&p.app.log_dir).map_err(|e| InstallError(e.to_string()))?;
    let mut svc = windows::service_state();
    if svc.is_some() {
        // The service's account reads config.json and the programs, and writes its logs. These are your own
        // folders, so granting it access needs no admin rights.
        for (target, perm) in [(wiki_dir, "M"), (p.app.config_dir.as_path(), "RX"), (p.app.data_dir.as_path(), "RX"), (p.app.log_dir.as_path(), "M")] {
            if let Err(InstallError(e)) = windows::grant(target, perm) {
                warn(&e);
            }
        }
        info("service account access: Modify on the wiki and logs, Read on config and program files");
    }
    let bin = windows::service_bin();
    let registered_here = bin.as_ref().is_some_and(|b| b.to_string_lossy().to_lowercase() == p.agent.to_string_lossy().to_lowercase());
    if !registered_here {
        info(&match &bin {
            Some(b) => format!("the service starts {}; re-registering it at {} (approve the UAC prompt)", fwd(b), fwd(&p.agent)),
            None => format!("registering the service at {} (approve the UAC prompt)", fwd(&p.agent)),
        });
        let log = p.app.log_dir.join("install-service.log");
        let _ = std::fs::remove_file(&log);
        let r = windows::run_elevated(&["install-service".into(), "--log".into(), log.to_string_lossy().into_owned()]);
        for line in std::fs::read_to_string(&log).unwrap_or_default().lines() {
            info(&format!("  | {line}"));
        }
        match r {
            Ok(0) => {}
            Ok(code) => warn(&format!("install-service exited with {code}")),
            Err(e) => warn(&format!("install-service did not run ({e}); the apps use stdio until it does. Run it later from an elevated terminal: agent-wiki install-service")),
        }
        svc = windows::service_state();
    } else if svc.as_deref() == Some("STOPPED") {
        info(&format!("starting the service {}", windows::SERVICE_NAME));
        let _ = windows::sc(&["start", windows::SERVICE_NAME]);
        windows::wait_state("RUNNING", Duration::from_secs(20));
        svc = windows::service_state();
    }
    if svc.as_deref() != Some("RUNNING") {
        warn(&format!("service {}; clients will start the server themselves (stdio).", svc.map(|s| format!("is {s}")).unwrap_or_else(|| "is not installed".into())));
        return Ok("stdio");
    }
    // A replaced program stops the running service, which the SCM restarts on the new build: wait for exactly it.
    let build = sha12(&p.agent);
    let h = sys::wait_for_health(port, |x| x["ok"] == true && x["version"] == aw_core::VERSION && x["build"] == build.as_str(), Duration::from_secs(45));
    match h {
        Some(h) if h["build"] == build.as_str() && h["wikiDir"].as_str().map(PathBuf::from).as_deref() == Some(wiki_dir) => {
            info(&format!("service running: v{} (build {}), pid {}, http://127.0.0.1:{port}/mcp", h["version"].as_str().unwrap_or("?"), h["build"].as_str().unwrap_or("?"), h["pid"]));
            remove_node_runtime(p);
            Ok("http")
        }
        other => {
            warn(&format!(
                "the service is RUNNING but /health returned {}; staying on stdio. See {}/service.log",
                other.map(|v| v.to_string()).unwrap_or_else(|| "nothing".into()),
                fwd(&p.app.log_dir)
            ));
            Ok("stdio")
        }
    }
}

/// What the Node runtime installed and this one replaces: its bundles next to the programs and the C#
/// service wrapper. Only once the service runs `agent-wiki` (a declined UAC prompt leaves the old
/// wrapper registered, and it still needs them).
#[cfg(windows)]
fn remove_node_runtime(p: &Places) {
    let mut removed = 0;
    for e in std::fs::read_dir(&p.runtime).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.ends_with(".mjs") || name.ends_with(".mjs.map") {
            removed += usize::from(std::fs::remove_file(e.path()).is_ok());
        }
    }
    let service_dir = p.service_ini.parent().unwrap_or(&p.runtime).to_path_buf();
    for e in std::fs::read_dir(&service_dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("AgentWikiService.") && name != "AgentWikiService.ini" {
            removed += usize::from(std::fs::remove_file(e.path()).is_ok());
        }
    }
    if removed > 0 {
        info(&format!("removed {removed} file(s) of the Node runtime and the C# service wrapper"));
    }
}

#[cfg(unix)]
fn service_step(p: &Places, _wiki_dir: &Path, port: u16, curated: bool) -> Result<&'static str, InstallError> {
    for line in posix::install_services(p, port, curated)? {
        info(&line);
    }
    let build = sha12(&p.agent);
    match sys::wait_for_health(port, |x| x["ok"] == true && x["build"] == build.as_str(), Duration::from_secs(30)) {
        Some(h) if h["build"] == build.as_str() => {
            info(&format!("service running: v{} (build {}), pid {}", h["version"].as_str().unwrap_or("?"), build, h["pid"]));
            Ok("http")
        }
        other => {
            warn(&format!("the service did not answer as this build ({}); staying on stdio", other.map(|v| v.to_string()).unwrap_or_else(|| "no answer".into())));
            Ok("stdio")
        }
    }
}

/// The tray: earlier trays stopped, start-at-sign-in registered, started, and checked.
fn tray_step(p: &Places, state: &mut Value, res: &mut Results) -> Result<(), InstallError> {
    let tray = |args: &[&str]| run_in(&p.tray_exe, args, None, &[], Duration::from_secs(60));
    // Stop whichever tray runs (the C# one and this one share the single-instance names).
    for exe in [&p.tray_exe, &p.legacy_tray_exe] {
        if exe.exists() && exe != &p.tray_exe {
            let _ = run_in(exe, &["--quit"], None, &[], Duration::from_secs(40));
        }
    }
    let q = tray(&["--quit", "--config", &p.tray_ini.to_string_lossy()]);
    if !q.ok {
        warn(&format!("the running tray did not quit (exit {:?}); end it from its menu, then run this again", q.code));
    }
    // The C# tray and its Edge window profile are replaced.
    for f in [&p.legacy_tray_exe, &p.legacy_tray_ini] {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_file(PathBuf::from(format!("{}.src-sha256", p.legacy_tray_exe.display())));
    if p.legacy_ui_profile.exists() {
        match std::fs::remove_dir_all(&p.legacy_ui_profile) {
            Ok(()) => info(&format!("removed the old window's Edge profile {}", fwd(&p.legacy_ui_profile))),
            Err(_) => warn(&format!("could not remove {} (in use): delete it later", fwd(&p.legacy_ui_profile))),
        }
    }
    #[cfg(windows)]
    {
        if let Some(sid) = windows::user_sid() {
            std::fs::write(&p.task_xml, windows::task_xml_bytes(&windows::task_xml(&p.tray_exe, &sid, true)?)).map_err(|e| InstallError(e.to_string()))?;
            state["userSid"] = json!(sid);
        } else {
            warn("could not determine your SID: no logon task, only the Run key");
        }
    }
    let r = tray(&["--do", "repair-autostart", "--config", &p.tray_ini.to_string_lossy()]);
    info(&format!("start at sign-in: {}", r.stdout.trim().trim_start_matches("ok: ")));
    let show = {
        #[cfg(unix)]
        {
            posix::has_display()
        }
        #[cfg(windows)]
        {
            true
        }
    };
    if show {
        let mut c = std::process::Command::new(&p.tray_exe);
        c.args(["--from", "install", "--config"]).arg(&p.tray_ini).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let _ = c.spawn();
        std::thread::sleep(Duration::from_millis(1500));
    }
    let st = tray(&["--selftest", "--config", &p.tray_ini.to_string_lossy()]);
    let field = |k: &str| st.stdout.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap_or("unknown").to_string();
    info(&format!("tray: it sees the service as {}; start at sign-in: {}", field("state"), field("autostart")));
    #[cfg(windows)]
    {
        let (promoted, text) = windows::tray_icon_placement(&p.tray_exe);
        info(&format!("tray icon: {text}"));
        if promoted != Some(true) {
            res.note("Tray icon", "click", format!("in the ^ overflow; to pin it: {}", windows::PIN_TRAY_ICON));
        }
        state["tray"] = json!({ "exe": fwd(&p.tray_exe), "runValue": windows::TRAY_RUN_VALUE, "task": windows::TRAY_TASK });
    }
    #[cfg(unix)]
    {
        state["tray"] = json!({ "exe": fwd(&p.tray_exe) });
    }
    let ok = field("state") != "down" && field("autostart").contains("on");
    res.note("Tray app", if ok { "ok" } else { "check" }, format!("{}; state {}; start at sign-in {}", fwd(&p.tray_exe), field("state"), field("autostart")));
    Ok(())
}

fn claude_plugin(cl: &Path, p: &Places, res: &mut Results) -> Result<(), InstallError> {
    backup_once(&p.claude_settings)?;
    let json_of = |r: &sys::Ran| -> Value {
        let t = r.stdout.trim();
        t.find(['[', '{']).and_then(|i| serde_json::from_str(&t[i..]).ok()).unwrap_or(Value::Null)
    };
    let must = |args: &[&str]| -> Result<sys::Ran, InstallError> {
        let r = run(cl, args);
        if !r.ok {
            return Err(InstallError(format!("claude {} failed:\n{}{}", args.join(" "), r.stdout, r.stderr)));
        }
        Ok(r)
    };
    let markets = json_of(&run(cl, &["plugin", "marketplace", "list", "--json"]));
    let known = markets.as_array().and_then(|a| a.iter().find(|m| m["name"] == CLAUDE_MARKET)).cloned();
    let mut have = known.is_some();
    if let Some(path) = known.as_ref().and_then(|k| k["path"].as_str().or(k["installLocation"].as_str()))
        && PathBuf::from(path).to_string_lossy().to_lowercase() != p.market.to_string_lossy().to_lowercase()
    {
        must(&["plugin", "marketplace", "remove", CLAUDE_MARKET])?;
        info(&format!("marketplace {CLAUDE_MARKET} was registered at {path}; registering it at {}", fwd(&p.market)));
        have = false;
    }
    if have {
        must(&["plugin", "marketplace", "update", CLAUDE_MARKET])?;
    } else {
        must(&["plugin", "marketplace", "add", &p.market.to_string_lossy()])?;
    }
    info(&format!("marketplace {CLAUDE_MARKET} {}", if have { "updated" } else { "added" }));
    let listed = || -> Option<Value> {
        let all = json_of(&run(cl, &["plugin", "list", "--json"]));
        let list = all.as_array().cloned().or_else(|| all["plugins"].as_array().cloned()).unwrap_or_default();
        list.into_iter().find(|x| x.to_string().contains(CLAUDE_PLUGIN_ID))
    };
    if listed().is_some() {
        let u = run(cl, &["plugin", "update", CLAUDE_PLUGIN_ID]);
        info(&format!("claude plugin update: {}", u.last_line()));
        let _ = run(cl, &["plugin", "enable", CLAUDE_PLUGIN_ID]);
    } else {
        must(&["plugin", "install", CLAUDE_PLUGIN_ID])?;
        info(&format!("installed {CLAUDE_PLUGIN_ID}"));
    }
    // Claude's cache is keyed by version: a re-render at the same version (stdio -> HTTP) is not picked up
    // by `plugin update`. Reinstall whenever the cached copy differs from what was just rendered.
    let mut mine = listed();
    let cached = |m: &Option<Value>| m.as_ref().and_then(|x| x["installPath"].as_str()).map(PathBuf::from);
    if let Some(dir) = cached(&mine)
        && tree_hash(&dir) != tree_hash(&p.rendered)
    {
        must(&["plugin", "uninstall", CLAUDE_PLUGIN_ID])?;
        must(&["plugin", "install", CLAUDE_PLUGIN_ID])?;
        mine = listed();
        if cached(&mine).map(|d| tree_hash(&d)) != Some(tree_hash(&p.rendered)) {
            return Err(InstallError(format!("Claude's cached plugin still differs from {}", fwd(&p.rendered))));
        }
        info("cached copy was stale: reinstalled the plugin");
    }
    res.note("Claude Code plugin", if mine.is_some() { "ok" } else { "check" }, mine.map(|m| m.to_string()).unwrap_or_else(|| "not listed".into()));
    Ok(())
}

/// The installed server over stdio (and HTTP when the service runs), and the hook under the shells
/// the apps use.
fn self_test(p: &Places, wiki_dir: &Path, port: u16, transport: &str, hook: &str, child_env: &[(String, String)]) -> Result<(), InstallError> {
    let want = "wiki_log,wiki_read,wiki_search,wiki_start,wiki_upsert_page";
    let check = |label: &str, r: Result<sys::McpCheck, String>| -> Result<(), InstallError> {
        let c = r.map_err(|e| InstallError(format!("MCP over {label}: {e}")))?;
        if c.tools.join(",") != want {
            return Err(InstallError(format!("MCP over {label}: unexpected tools {}", c.tools.join(","))));
        }
        if !c.instructions.contains(&fwd(wiki_dir)) {
            return Err(InstallError(format!("MCP over {label}: the server instructions do not name the wiki folder")));
        }
        if !c.start_ok {
            return Err(InstallError(format!("MCP over {label}: wiki_start failed")));
        }
        info(&format!("MCP over {label} OK: {}, 5 tools, wiki_start OK", c.server));
        Ok(())
    };
    let stdio = with_env(child_env, || sys::mcp_stdio(&p.agent, true));
    check("stdio", stdio)?;
    if transport == "http" {
        check("HTTP service", sys::mcp_http(port))?;
    }
    let env: Vec<(&str, &str)> = child_env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut shells: Vec<(PathBuf, Vec<String>)> = vec![];
    #[cfg(windows)]
    {
        shells.push((PathBuf::from("cmd.exe"), vec!["/d".into(), "/c".into(), hook.into()]));
        shells.push((PathBuf::from("powershell.exe"), vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), hook.into()]));
        if let Some(pw) = which("pwsh") {
            shells.push((pw, vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), hook.into()]));
        }
        let bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        if bash.exists() {
            shells.push((bash, vec!["-c".into(), hook.into()]));
        }
    }
    #[cfg(unix)]
    {
        for sh in ["sh", "bash", "zsh"] {
            if let Some(s) = which(sh) {
                shells.push((s, vec!["-c".into(), hook.into()]));
            }
        }
    }
    for (shell, args) in shells {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = run_in(&shell, &a, None, &env, Duration::from_secs(20));
        let ctx = serde_json::from_str::<Value>(&r.stdout).ok().and_then(|v| v["hookSpecificOutput"]["additionalContext"].as_str().map(String::from)).unwrap_or_default();
        if !r.ok || !ctx.starts_with("Agent Wiki is the user") {
            return Err(InstallError(format!("hook command failed under {}: exit {:?}\n{}\n{}", shell.display(), r.code, r.stdout, r.stderr)));
        }
        info(&format!("hook OK under {}", shell.file_name().unwrap_or_default().to_string_lossy()));
    }
    Ok(())
}

/// Runs `f` with these variables set for the children it starts (the sandbox's fake home).
fn with_env<T>(vars: &[(String, String)], f: impl FnOnce() -> T) -> T {
    let saved: Vec<(String, Option<String>)> = vars.iter().map(|(k, _)| (k.clone(), std::env::var(k).ok())).collect();
    for (k, v) in vars {
        // SAFETY: the installer is single-threaded here (no other threads read the environment).
        unsafe { std::env::set_var(k, v) };
    }
    let r = f();
    for (k, v) in saved {
        // SAFETY: as above.
        unsafe {
            match v {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
    }
    r
}

// ---------------------------------------------------------------- uninstall

fn uninstall(o: &Opts) -> Result<(), InstallError> {
    let (env, _) = environment(o.sandbox.as_deref());
    let p = Places::new(&env);
    let sandbox = o.sandbox.is_some();
    let mut state = load_state(&p);
    let created = created_list(&state);
    let codex_id = state["codexPluginId"].as_str().unwrap_or("agent-wiki@personal").to_string();
    let cfg = read_json(&p.config).ok().flatten();
    println!("Agent Wiki uninstaller");
    #[cfg(windows)]
    if !sandbox && let Some(pkg) = windows::packaged() {
        return Err(InstallError(format!(
            "This terminal runs inside the app package {pkg}, where Windows redirects AppData writes into that app's private storage. Run it from a normal terminal: Win+R, cmd."
        )));
    }

    step("1", "Tray app and its start at sign-in");
    if !sandbox {
        for exe in [&p.tray_exe, &p.legacy_tray_exe] {
            if exe.exists() {
                let q = run_in(exe, &["--quit", "--config", &p.tray_ini.to_string_lossy()], None, &[], Duration::from_secs(40));
                info(if q.ok { "tray stopped (its curator stopped with it)" } else { "tray --quit did not finish" });
            }
        }
        #[cfg(windows)]
        {
            let task = state["tray"]["task"].as_str().unwrap_or(windows::TRAY_TASK).to_string();
            info(if windows::delete_task(&task) { "removed the logon task" } else { "no logon task" });
            info(if windows::reg_delete(windows::RUN_KEY, windows::TRAY_RUN_VALUE) { "removed the Run key entry" } else { "no Run key entry" });
        }
        #[cfg(target_os = "macos")]
        {
            let f = p.home.join("Library").join("LaunchAgents").join("com.agentwiki.tray.plist");
            if std::fs::remove_file(&f).is_ok() {
                info("removed the login item (LaunchAgent)");
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let f = p.app.config_dir.parent().map(|d| d.join("autostart").join("agent-wiki-tray.desktop"));
            if f.is_some_and(|f| std::fs::remove_file(f).is_ok()) {
                info("removed the autostart entry");
            }
        }
        #[cfg(unix)]
        for line in posix::uninstall_services(&p) {
            info(&line);
        }
    }
    for _ in 0..10 {
        if std::fs::remove_dir_all(&p.tray_dir).is_ok() || !p.tray_dir.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500)); // the program stays locked for a moment after it exits
    }
    info(&format!("removed {}", fwd(&p.tray_dir)));
    let _ = std::fs::remove_dir_all(&p.legacy_ui_profile);
    if p.curator_codex_home.exists() {
        info(&format!("kept the curator's Codex sign-in in {} (delete the folder to sign it out for good)", fwd(&p.curator_codex_home)));
    }

    step("2", "Plugin and local marketplaces");
    if !sandbox {
        if let Some(cl) = which("claude") {
            let u = run(cl.as_path(), &["plugin", "uninstall", CLAUDE_PLUGIN_ID]);
            info(&format!("claude plugin uninstall: {}", if u.ok { "done".into() } else { u.last_line() }));
            let m = run(cl.as_path(), &["plugin", "marketplace", "remove", CLAUDE_MARKET]);
            info(&format!("claude plugin marketplace remove: {}", if m.ok { "done".into() } else { m.last_line() }));
        }
        if let Some(cx) = which("codex") {
            let r = run(cx.as_path(), &["plugin", "remove", &codex_id]);
            info(&format!("codex plugin remove {codex_id}: {}", if r.ok { "done".into() } else { r.last_line() }));
        }
    }
    let _ = std::fs::remove_dir_all(&p.market);
    info(&format!("removed {}", fwd(&p.market)));

    step("3", "ChatGPT personal marketplace entry");
    let mut emptied = false;
    edit_file(&p.personal_market, |text| {
        let Some(t) = text else { return Ok(None) };
        let Ok(mut m) = parse_object(t, &p.personal_market) else { return Ok(None) };
        let Some(list) = m["plugins"].as_array_mut() else { return Ok(None) };
        let before = list.len();
        list.retain(|x| x["name"] != PLUGIN);
        if list.len() == before {
            return Ok(None);
        }
        emptied = list.is_empty();
        Ok(Some(to_json(&m)))
    })?;
    if emptied && created.contains(&fwd(&p.personal_market)) {
        let _ = std::fs::remove_file(&p.personal_market);
        info(&format!("deleted {} (the installer created it)", fwd(&p.personal_market)));
    }

    step("4", "Claude desktop registration");
    let mut desktop: Vec<PathBuf> = state["claudeDesktopConfigs"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(PathBuf::from)).collect()).unwrap_or_default();
    desktop.extend(p.claude_desktop_configs());
    desktop.dedup();
    if !desktop.is_empty() && desktop_running(o.sandbox.as_deref()) {
        warn("Claude desktop is running and would write the agent-wiki entry back: quit it (its tray icon > Quit), then uninstall again to remove it there.");
        desktop.clear();
    }
    for file in desktop {
        let changed = edit_file(&file, |text| {
            let Some(t) = text else { return Ok(None) };
            let Ok(mut c) = parse_object(t, &file) else { return Ok(None) };
            let Some(servers) = c["mcpServers"].as_object_mut() else { return Ok(None) };
            if servers.remove(SERVER).is_none() {
                return Ok(None);
            }
            if servers.is_empty() {
                c.as_object_mut().unwrap().remove("mcpServers");
            }
            Ok(Some(to_json(&c)))
        })?;
        if changed {
            info(&format!("removed {SERVER} from {}", fwd(&file)));
        }
    }

    step("5", "Global instruction blocks");
    for file in [&p.codex_agents, &p.claude_md] {
        let text = read_text(file)?.map(|t| t.replace("\r\n", "\n"));
        let Some(next) = remove_block(text.as_deref()) else { continue };
        if next.is_empty() && created.contains(&fwd(file)) {
            let _ = std::fs::remove_file(file);
            info(&format!("deleted {} (the installer created it)", fwd(file)));
        } else {
            edit_file(file, |_| Ok(Some(next.clone())))?;
            info(&format!("{}: block removed", fwd(file)));
        }
    }

    step("6", "Tool approval: ChatGPT/Codex and Claude Code");
    let a = edit_file(&p.codex_config, |t| toml_remove_approval(t, &codex_id))?;
    info(if a { "removed the ChatGPT/Codex approval setting" } else { "nothing to remove in config.toml" });
    let b = edit_file(&p.claude_settings, claude_allow_remove)?;
    info(if b { "removed the Claude Code allow rules" } else { "no Claude Code allow rules to remove" });

    step("7", "Paste file");
    let _ = std::fs::remove_file(&p.paste);

    state["uninstalledAt"] = json!(aw_core::text::local_iso(&aw_core::text::now()));
    state["created"] = json!(created.into_iter().filter(|f| Path::new(f).exists()).collect::<Vec<_>>());
    write_lf(&p.state, &to_json(&state))?;
    println!("\nUninstalled. Backups (*.bak-agent-wiki) were kept.");
    println!("Your wiki is untouched at: {}", cfg.as_ref().and_then(|c| c["wikiDir"].as_str()).map(String::from).unwrap_or_else(|| fwd(&p.default_wiki)));
    #[cfg(windows)]
    println!("The service stays until you run `agent-wiki uninstall-service` in an elevated terminal.");
    println!("Programs and config remain in {} and {} (delete them to remove them).", fwd(&p.app.data_dir), fwd(&p.app.config_dir));
    Ok(())
}

// ---------------------------------------------------------------- the Windows service (elevated)

#[cfg(windows)]
mod service {
    use super::places::{Places, fwd};
    use super::windows::*;
    use super::{InstallError, info, read_json, step, warn};
    use std::path::PathBuf;
    use std::time::Duration;

    struct Log(Option<PathBuf>);

    impl Log {
        fn say(&self, msg: &str) {
            println!("{msg}");
            if let Some(f) = &self.0 {
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
                    let _ = writeln!(file, "{msg}");
                }
            }
        }
    }

    /// Registers the AgentWiki service: `agent-wiki service` serving HTTP on 127.0.0.1, starting at boot,
    /// restarted on failure (and on upgrade), as the low-privilege virtual account NT SERVICE\AgentWiki
    /// with Modify on the wiki and logs and Read on Agent Wiki's settings and program folders. It also
    /// lets your account start and stop this one service (the tray's "Restart service").
    pub fn install(user_sid: Option<String>, log: Option<PathBuf>) -> Result<(), InstallError> {
        let log = Log(log);
        let r = (|| -> Result<(), InstallError> {
            if !is_elevated() {
                return Err(InstallError("This needs administrator rights: run it from a terminal opened with \"Run as administrator\".".into()));
            }
            let p = Places::new(&aw_core::paths::process_env());
            let cfg = read_json(&p.config)?.ok_or_else(|| InstallError(format!("run `agent-wiki install` in a normal terminal first (missing {})", fwd(&p.config))))?;
            if !p.agent.exists() || !p.service_ini.exists() {
                return Err(InstallError(format!("run `agent-wiki install` in a normal terminal first (missing {} or {})", fwd(&p.agent), fwd(&p.service_ini))));
            }
            let wiki_dir = PathBuf::from(cfg["wikiDir"].as_str().unwrap_or(""));
            let port = cfg["httpPort"].as_u64().and_then(|x| u16::try_from(x).ok()).unwrap_or(47821);
            log.say(&format!("Agent Wiki service installer: {SERVICE_NAME} as {SERVICE_ACCOUNT}, port {port}"));
            let bin = format!("\"{}\" service --config \"{}\"", p.agent.display(), p.service_ini.display());
            log.say(&format!("program: {bin}"));
            match service_state() {
                Some(state) => {
                    if state != "STOPPED" {
                        let _ = sc(&["stop", SERVICE_NAME]);
                        if !wait_state("STOPPED", Duration::from_secs(20)) {
                            log.say("! the service did not report STOPPED within 20 s");
                        }
                    }
                    sc_ok(&["config", SERVICE_NAME, "binPath=", &bin, "start=", "auto", "obj=", SERVICE_ACCOUNT, "DisplayName=", "Agent Wiki"])?;
                    log.say(&format!("updated the existing service ({state})"));
                }
                None => {
                    sc_ok(&["create", SERVICE_NAME, "binPath=", &bin, "start=", "auto", "obj=", SERVICE_ACCOUNT, "DisplayName=", "Agent Wiki"])?;
                    log.say("created the service");
                }
            }
            sc_ok(&["description", SERVICE_NAME, "Shared AI memory (agent-wiki MCP server) on http://127.0.0.1 for Claude, ChatGPT and other local AI apps."])?;
            // The service also stops with an error code when an install replaces its program: these restarts are
            // how an upgrade takes effect without admin rights.
            sc_ok(&["failure", SERVICE_NAME, "reset=", "86400", "actions=", "restart/2000/restart/5000/restart/30000"])?;
            sc_ok(&["failureflag", SERVICE_NAME, "1"])?;
            log.say("start: automatic at boot; on failure or upgrade: restart after 2 s, 5 s, 30 s");
            std::fs::create_dir_all(&p.app.log_dir).map_err(|e| InstallError(e.to_string()))?;
            std::fs::create_dir_all(&p.app.config_dir).map_err(|e| InstallError(e.to_string()))?;
            for (target, perm) in [(wiki_dir.as_path(), "M"), (p.app.config_dir.as_path(), "RX"), (p.app.data_dir.as_path(), "RX"), (p.app.log_dir.as_path(), "M")] {
                log.say(&format!("granted {}", grant(target, perm)?));
            }
            let state = read_json(&p.state).ok().flatten().unwrap_or_default();
            let sid = user_sid
                .clone()
                .or_else(|| state["userSid"].as_str().map(String::from))
                .or_else(user_sid_fn)
                .ok_or_else(|| InstallError("could not determine your account SID; pass --user-sid S-1-...".into()))?;
            let before = sc_ok(&["sdshow", SERVICE_NAME])?.trim().to_string();
            let after = sddl_with_start_stop(&before, &sid)?;
            if after == before {
                log.say(&format!("already granted: {sid} may start and stop {SERVICE_NAME}"));
            } else {
                sc_ok(&["sdset", SERVICE_NAME, &after])?;
                log.say(&format!("granted {sid} start/stop/query on {SERVICE_NAME} only (ACE (A;;{TRAY_SERVICE_RIGHTS};;;{sid}))"));
            }
            sc_ok(&["start", SERVICE_NAME])?;
            if !wait_state("RUNNING", Duration::from_secs(20)) {
                log.say("! the service did not report RUNNING within 20 s");
            }
            match super::sys::wait_for_health(port, |h| h["ok"] == true, Duration::from_secs(20)) {
                Some(h) if h["ok"] == true => {
                    log.say(&format!("healthy: agent-wiki v{}, pid {}, wiki {}", h["version"].as_str().unwrap_or("?"), h["pid"], h["wikiDir"].as_str().unwrap_or("?")));
                    Ok(())
                }
                other => {
                    let tail: Vec<String> = std::fs::read_to_string(p.app.log_dir.join("service.log")).unwrap_or_default().lines().rev().take(15).map(String::from).collect();
                    Err(InstallError(format!(
                        "The service is not healthy: {}\nLast log lines:\n{}",
                        other.map(|v| v.to_string()).unwrap_or_default(),
                        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
                    )))
                }
            }
        })();
        if let Err(InstallError(m)) = &r {
            log.say(&format!("Service install failed: {m}"));
        }
        r
    }

    fn user_sid_fn() -> Option<String> {
        super::windows::user_sid()
    }

    pub fn uninstall() -> Result<(), InstallError> {
        if !is_elevated() {
            return Err(InstallError("This needs administrator rights: run it from a terminal opened with \"Run as administrator\".".into()));
        }
        let p = Places::new(&aw_core::paths::process_env());
        let cfg = read_json(&p.config).ok().flatten();
        step("1", "Stop and delete the service");
        match service_state() {
            Some(state) => {
                if state != "STOPPED" {
                    let _ = sc(&["stop", SERVICE_NAME]);
                    wait_state("STOPPED", Duration::from_secs(20));
                }
                let d = sc(&["delete", SERVICE_NAME]);
                info(&if d.ok { "service deleted".to_string() } else { format!("sc delete: {}", d.last_line()) });
            }
            None => info("service not installed"),
        }
        step("2", "Remove folder grants");
        let wiki = cfg.as_ref().and_then(|c| c["wikiDir"].as_str()).map(PathBuf::from);
        for target in [wiki, Some(p.app.config_dir.clone()), Some(p.app.data_dir.clone()), Some(p.app.log_dir.clone())].into_iter().flatten().filter(|t| t.exists()) {
            let r = ungrant(&target);
            if r.ok { info(&format!("removed {SERVICE_ACCOUNT} from {}", fwd(&target))) } else { warn(&format!("icacls {}: {}", fwd(&target), r.last_line())) }
        }
        println!("\nService removed. Run `agent-wiki install` in a normal terminal to switch the apps back to stdio.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_and_sync() {
        let dir = std::env::temp_dir().join(format!("aw-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src").join("sub")).unwrap();
        std::fs::write(dir.join("src").join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("src").join("sub").join("b.txt"), "b").unwrap();
        let to = dir.join("to");
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(to.join("stale.txt"), "x").unwrap();
        assert_eq!(sync_dir(&dir.join("src"), &to).unwrap(), 2);
        assert_eq!(files::walk(&to), vec!["a.txt", "sub/b.txt"]);
        let dest = dir.join("bin").join("prog");
        assert_eq!(replace_file(&dir.join("src").join("a.txt"), &dest).unwrap(), "installed");
        assert_eq!(replace_file(&dir.join("src").join("a.txt"), &dest).unwrap(), "unchanged");
        assert_eq!(replace_file(&dir.join("src").join("sub").join("b.txt"), &dest).unwrap(), "installed");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "b");
        let _ = std::fs::remove_dir_all(dir);
    }
}
