//! macOS and Linux: a menu bar / StatusNotifierItem icon (tray-icon), the same menu, and the window
//! (WKWebView on macOS, WebKitGTK on Linux) on a tao event loop. Single instance through a Unix
//! socket in the state folder, which also carries `--quit` and "open the window".

use crate::actions::{self, Ctx, SLOW};
use crate::autostart::{self, State as Autostart};
use crate::config::Config;
use crate::curator_host::CuratorHost;
use crate::menu;
use crate::status::Status;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const WIDTH: f64 = 460.0;
const HEIGHT: f64 = 760.0;
const FREE_AFTER: Duration = Duration::from_secs(5 * 60);

enum User {
    Status(Status),
    Autostart(Result<Autostart, String>),
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Quit,
    Open,
    Done { action: String, result: Result<Option<String>, String> },
}

fn socket_path(cfg: &Config) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| aw_core::paths::AppPaths::current().state_dir);
    base.join(format!("{}.sock", cfg.instance))
}

/// Sends one word to the running tray; false when none answers.
fn send(cfg: &Config, word: &str) -> bool {
    match UnixStream::connect(socket_path(cfg)) {
        Ok(mut s) => {
            let _ = s.set_read_timeout(Some(Duration::from_secs(25)));
            let _ = s.write_all(word.as_bytes());
            let _ = s.shutdown(std::net::Shutdown::Write);
            let mut ack = String::new();
            let _ = s.read_to_string(&mut ack);
            true
        }
        Err(_) => false,
    }
}

fn icon(cfg: &Config, state: &str) -> Option<tray_icon::Icon> {
    let bytes = std::fs::read(cfg.icon_dir.join(format!("agent-wiki-{state}.ico"))).ok()?;
    let (rgba, size) = super::ico_rgba(&bytes, 32)?;
    tray_icon::Icon::from_rgba(rgba, size, size).ok()
}

/// How many of the three state icons load (--selftest).
pub fn icons_loaded(cfg: &Config) -> usize {
    ["healthy", "degraded", "down"].iter().filter(|s| std::fs::read(cfg.icon_dir.join(format!("agent-wiki-{s}.ico"))).ok().and_then(|b| super::ico_rgba(&b, 16)).is_some()).count()
}

pub fn window_line(cfg: &Config) -> String {
    format!("webview {}x{} data={}", WIDTH as i32, HEIGHT as i32, cfg.webview_dir.display())
}

fn notify(title: &str, text: &str) {
    if cfg!(target_os = "macos") {
        let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = std::process::Command::new("osascript").args(["-e", &format!("display notification \"{}\" with title \"{}\"", q(text), q(title))]).spawn();
    } else {
        let _ = std::process::Command::new("notify-send").args(["-a", "Agent Wiki", title, text]).spawn();
    }
}

struct UiWindow {
    window: Window,
    _webview: wry::WebView,
    _context: Box<wry::WebContext>,
    hidden_at: Option<Instant>,
}

fn create_window(target: &tao::event_loop::EventLoopWindowTarget<User>, cfg: &Config) -> Result<UiWindow, String> {
    let window = WindowBuilder::new()
        .with_title("Agent Wiki")
        .with_inner_size(tao::dpi::LogicalSize::new(WIDTH, HEIGHT))
        .with_min_inner_size(tao::dpi::LogicalSize::new(360.0, 480.0))
        .build(target)
        .map_err(|e| e.to_string())?;
    // Under the menu bar on macOS (top right), above the panel elsewhere (bottom right).
    if let Some(m) = window.current_monitor() {
        let (ms, mp, scale) = (m.size(), m.position(), m.scale_factor());
        let (w, h) = ((WIDTH * scale) as i32, (HEIGHT * scale) as i32);
        let margin = (12.0 * scale) as i32;
        let x = mp.x + ms.width as i32 - w - margin;
        let y = if cfg!(target_os = "macos") { mp.y + (30.0 * scale) as i32 } else { mp.y + ms.height as i32 - h - margin * 4 };
        window.set_outer_position(tao::dpi::PhysicalPosition::new(x, y.max(mp.y)));
    }
    let _ = std::fs::create_dir_all(&cfg.webview_dir);
    let mut context = Box::new(wry::WebContext::new(Some(cfg.webview_dir.clone())));
    let origin = format!("http://127.0.0.1:{}/", cfg.port);
    let open_external = |url: &str| {
        let _ = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }).arg(url).spawn();
    };
    let builder = wry::WebViewBuilder::new_with_web_context(&mut context)
        .with_url(cfg.ui_url())
        .with_navigation_handler(move |url| {
            if url.starts_with(&origin) || url == "about:blank" {
                return true;
            }
            open_external(&url);
            false
        })
        .with_new_window_req_handler(move |url, _| {
            if url.starts_with("http://") || url.starts_with("https://") {
                open_external(&url);
            }
            wry::NewWindowResponse::Deny
        });
    #[cfg(target_os = "macos")]
    let webview = builder.build(&window);
    #[cfg(not(target_os = "macos"))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().ok_or("no GTK container")?)
    };
    let webview = webview.map_err(|e| e.to_string())?;
    Ok(UiWindow { window, _webview: webview, _context: context, hidden_at: None })
}

fn build_menu(items: &[menu::Item]) -> (Menu, Vec<(tray_icon::menu::MenuId, String)>) {
    let root = Menu::new();
    let mut subs: Vec<(&'static str, Submenu)> = vec![];
    let mut ids = vec![];
    for it in items {
        let text = it.text.replace("&&", "&");
        let entry: Box<dyn tray_icon::menu::IsMenuItem> = if it.text == "-" {
            Box::new(PredefinedMenuItem::separator())
        } else {
            let m = MenuItem::new(&text, !it.action.is_empty(), None);
            if !it.action.is_empty() {
                ids.push((m.id().clone(), it.action.clone()));
            }
            Box::new(m)
        };
        if it.sub.is_empty() {
            let _ = root.append(entry.as_ref());
        } else {
            if !subs.iter().any(|(n, _)| *n == it.sub) {
                let s = Submenu::new(it.sub, true);
                let _ = root.append(&s);
                subs.push((it.sub, s));
            }
            let s = &subs.iter().find(|(n, _)| *n == it.sub).unwrap().1;
            let _ = s.append(entry.as_ref());
        }
    }
    (root, ids)
}

pub fn run(cfg: Config, from: &str) -> i32 {
    let sock = socket_path(&cfg);
    if send(&cfg, if from == "manual" { "open" } else { "ping" }) {
        return 3;
    }
    let _ = std::fs::remove_file(&sock);
    if let Some(d) = sock.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agent-wiki-tray: cannot listen on {}: {e}", sock.display());
            return 2;
        }
    };
    crate::log::tray(&cfg, &format!("started pid {} (from {from}): {}", std::process::id(), autostart::exe_path().display()));
    let event_loop = EventLoopBuilder::<User>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(User::Tray(e));
        }));
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(User::Menu(e));
        }));
    }
    {
        let p = proxy.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let mut conn = conn;
                let mut word = String::new();
                let _ = conn.read_to_string(&mut word);
                match word.trim() {
                    "quit" => {
                        let _ = p.send_event(User::Quit);
                        // Answer once gone: the caller waits for the socket to close.
                        std::thread::sleep(Duration::from_millis(100));
                        let _ = conn.write_all(b"bye");
                        std::mem::forget(conn); // closed when the process exits
                        continue;
                    }
                    "open" => {
                        let _ = p.send_event(User::Open);
                    }
                    _ => {}
                }
                let _ = conn.write_all(b"ok");
            }
        });
    }
    spawn_loop(&proxy, cfg.clone(), Duration::from_secs(5), |c| User::Status(Status::fetch(c)));
    spawn_loop(&proxy, cfg.clone(), Duration::from_secs(cfg.autostart_check_seconds), |c| User::Autostart(autostart::check(c)));

    let curator = cfg.host_curator.then(|| CuratorHost::start(&cfg));
    let mut tray: Option<TrayIcon> = None;
    let mut ids: Vec<(tray_icon::menu::MenuId, String)> = vec![];
    let mut last_menu: Vec<String> = vec![];
    let mut status = Status::default();
    let mut last_state: Option<&'static str> = None;
    let mut announced = crate::status::Announced::load(&cfg.state_file);
    let mut auto: Option<Autostart> = None;
    let mut checked_at = aw_core::text::local_iso(&aw_core::text::now())[..19].to_string();
    let mut window: Option<UiWindow> = None;
    let started = Instant::now();
    let quit_flag = Arc::new(AtomicBool::new(false));

    event_loop.run(move |event, target, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(30));
        let mut refresh_menu = false;
        match event {
            Event::NewEvents(StartCause::Init) => {
                if cfg.show_icon {
                    let items = menu::items(&cfg, &status, auto.as_ref());
                    let (m, i) = build_menu(&items);
                    ids = i;
                    last_menu = items.iter().map(|x| format!("{}|{}|{}", x.sub, x.text, x.action)).collect();
                    let mut b = TrayIconBuilder::new().with_menu(Box::new(m)).with_tooltip("Agent Wiki: starting").with_menu_on_left_click(false);
                    if let Some(i) = icon(&cfg, "down") {
                        b = b.with_icon(i);
                    }
                    tray = b.build().ok();
                }
            }
            Event::UserEvent(User::Status(s)) => {
                status = s;
                let state = status.state();
                if let Some(t) = &tray {
                    if last_state != Some(state) {
                        let _ = t.set_icon(icon(&cfg, state));
                    }
                    let _ = t.set_tooltip(Some(status.tooltip()));
                }
                if let Some(last) = last_state
                    && last != state
                {
                    let settled = started.elapsed().as_secs() > 60;
                    if state == "down" && settled {
                        notify("Agent Wiki service is not responding", "Apps fall back to their own server meanwhile.");
                    } else if state == "degraded" {
                        notify("Agent Wiki needs attention", status.reasons().first().map(String::as_str).unwrap_or("See the menu."));
                    } else if state == "healthy" && settled {
                        notify("Agent Wiki is back to normal", "");
                    }
                }
                last_state = Some(state);
                if let Some((title, text)) = announced.take(&status) {
                    notify(&title, &text);
                }
                refresh_menu = true;
            }
            Event::UserEvent(User::Autostart(r)) => match r {
                Ok(a) => {
                    let before = auto.replace(a.clone());
                    let prev = std::mem::replace(&mut checked_at, aw_core::text::local_iso(&aw_core::text::now())[..19].to_string());
                    match before {
                        None => crate::log::tray(&cfg, &format!("start at sign-in: {a}")),
                        Some(b) if b != a => {
                            crate::log::tray(&cfg, &format!("start at sign-in changed: {b} -> {a} (between {prev} and now)"));
                            if b.starts() && !a.starts() {
                                notify("Agent Wiki will not start at sign-in", "Its autostart entry is gone. Use the menu: Repair.");
                            }
                        }
                        _ => {}
                    }
                    refresh_menu = true;
                }
                Err(e) => crate::log::tray(&cfg, &format!("start-at-sign-in check failed: {e}")),
            },
            Event::UserEvent(User::Tray(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. })) | Event::UserEvent(User::Open) => {
                if !cfg.window {
                    crate::log::tray(&cfg, "open-ui requested (window=0)");
                } else if let Some(w) = &mut window {
                    w.window.set_visible(true);
                    w.window.set_focus();
                    w.hidden_at = None;
                } else {
                    match create_window(target, &cfg) {
                        Ok(w) => window = Some(w),
                        Err(e) => {
                            crate::log::tray(&cfg, &format!("window: {e}; opening the default browser"));
                            let _ = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }).arg(cfg.ui_url()).spawn();
                        }
                    }
                }
            }
            Event::UserEvent(User::Menu(e)) => {
                if let Some((_, action)) = ids.iter().find(|(id, _)| *id == e.id) {
                    let action = action.clone();
                    match action.as_str() {
                        "quit" => {
                            let _ = proxy.send_event(User::Quit);
                        }
                        "open-ui" => {
                            let _ = proxy.send_event(User::Open);
                        }
                        a if SLOW.contains(&a) => {
                            let (cfg, curator, p) = (cfg.clone(), curator.clone(), proxy.clone());
                            std::thread::spawn(move || {
                                let result = actions::run(&cfg, &action, &Ctx { curator, open_ui: None });
                                let _ = p.send_event(User::Done { action, result });
                            });
                        }
                        _ => {
                            let result = actions::run(&cfg, &action, &Ctx { curator: curator.clone(), open_ui: None });
                            let _ = proxy.send_event(User::Done { action, result });
                        }
                    }
                }
            }
            Event::UserEvent(User::Done { action, result }) => match result {
                Ok(Some(m)) => notify("Agent Wiki", &m),
                Err(e) => notify(if SLOW.contains(&action.as_str()) { "Agent Wiki: that did not work" } else { "Agent Wiki" }, &e),
                _ => {}
            },
            Event::UserEvent(User::Quit) => {
                if !quit_flag.swap(true, Ordering::SeqCst) {
                    tray.take();
                    window.take();
                    if let Some(c) = &curator {
                        c.stop();
                    }
                    crate::log::tray(&cfg, "quit");
                    let _ = std::fs::remove_file(socket_path(&cfg));
                    *flow = ControlFlow::Exit;
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                if let Some(w) = &mut window {
                    w.window.set_visible(false);
                    w.hidden_at = Some(Instant::now());
                }
            }
            _ => {}
        }
        // A window closed a while ago is freed, so an idle tray stays small.
        if window.as_ref().and_then(|w| w.hidden_at).is_some_and(|t| t.elapsed() > FREE_AFTER) {
            window = None;
        }
        if refresh_menu && let Some(t) = &tray {
            let items = menu::items(&cfg, &status, auto.as_ref());
            let now: Vec<String> = items.iter().map(|x| format!("{}|{}|{}", x.sub, x.text, x.action)).collect();
            if now != last_menu {
                let (m, i) = build_menu(&items);
                t.set_menu(Some(Box::new(m)));
                ids = i;
                last_menu = now;
            }
        }
    })
}

fn spawn_loop(proxy: &EventLoopProxy<User>, cfg: Config, every: Duration, f: fn(&Config) -> User) {
    let p = proxy.clone();
    std::thread::spawn(move || {
        loop {
            if p.send_event(f(&cfg)).is_err() {
                return;
            }
            std::thread::sleep(every);
        }
    });
}

pub fn signal_quit(cfg: &Config) -> i32 {
    if !send(cfg, "quit") {
        return 0;
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if UnixStream::connect(socket_path(cfg)).is_err() {
            return 0;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    1
}

pub fn request_open(cfg: &Config) -> Result<(), String> {
    if send(cfg, "open") || !cfg.window {
        return Ok(());
    }
    std::process::Command::new(autostart::exe_path()).args(["--window", "--config"]).arg(&cfg.file).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// `--window`: only the window, until it is closed.
pub fn run_window(cfg: Config) -> i32 {
    let event_loop = EventLoopBuilder::<User>::with_user_event().build();
    let mut window: Option<UiWindow> = None;
    event_loop.run(move |event, target, flow| {
        *flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => match create_window(target, &cfg) {
                Ok(w) => window = Some(w),
                Err(e) => {
                    crate::log::tray(&cfg, &format!("window: {e}"));
                    *flow = ControlFlow::Exit;
                }
            },
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                window.take();
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}
