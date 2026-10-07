//! Windows: a notification-area icon (Shell_NotifyIcon) with balloons, a menu built fresh on each
//! right click, and the Agent Wiki window: a WebView2 view of /ui/ in a small window above the tray.
//! A closed window is hidden and freed after a few idle minutes, so the tray stays small.
//!
//! Single instance per session with the C# tray's names (`Local\<instance>`, `...-Quit`), so either
//! one's `--quit` stops the other during an upgrade; `...-Open` asks the running tray for its window.

use crate::actions::{self, Ctx, SLOW};
use crate::autostart::{self, State as Autostart};
use crate::config::Config;
use crate::curator_host::CuratorHost;
use crate::menu;
use crate::status::Status;
use crate::winutil::{fill, wide, wide_path};
use std::cell::RefCell;
use std::num::NonZeroIsize;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, INFINITE, OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject};
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use wry::raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};

const WM_TRAY: u32 = WM_APP + 1;
const WM_EVENT: u32 = WM_APP + 2;
const TIMER_POLL: usize = 1;
const TIMER_AUTOSTART: usize = 2;
const TIMER_FREE_WINDOW: usize = 3;
const FREE_AFTER_MS: u32 = 5 * 60_000;
const WIDTH: i32 = 460;
const HEIGHT: i32 = 760;
const MARGIN: i32 = 12;
const EVENT_MODIFY_STATE: u32 = 0x0002;
const SYNCHRONIZE: u32 = 0x0010_0000;
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1; // NINF_KEY
const NIN_BALLOONUSERCLICK: u32 = 0x0400 + 5; // WM_USER + 5: the balloon was clicked

enum Event {
    Status(Status),
    Autostart(Result<Autostart, String>),
    Quit,
    Open,
    Done { action: String, result: Result<Option<String>, String> },
}

static EVENTS: Mutex<Vec<Event>> = Mutex::new(Vec::new());
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);

fn post(e: Event) {
    EVENTS.lock().unwrap().push(e);
    let h = TRAY_HWND.load(Ordering::SeqCst);
    if h != 0 {
        // SAFETY: posting to our own window; harmless if it is gone.
        unsafe { PostMessageW(h as HWND, WM_EVENT, 0, 0) };
    }
}

struct App {
    cfg: Config,
    hwnd: HWND,
    icons: [HICON; 3],
    icon_added: bool,
    status: Status,
    last_state: Option<&'static str>,
    /// How many changes /status said were applied automatically at the last poll (the notification).
    announced: crate::status::Announced,
    started: Instant,
    autostart: Option<Autostart>,
    autostart_checked_at: String,
    curator: Option<Arc<CuratorHost>>,
    window: Option<UiWindow>,
    polling: Arc<AtomicBool>,
    checking: Arc<AtomicBool>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static STANDALONE: RefCell<Option<(Config, Option<UiWindow>)>> = const { RefCell::new(None) };
}

fn with_app<T>(f: impl FnOnce(&mut App) -> T) -> Option<T> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn names(cfg: &Config) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    (wide(&format!("Local\\{}", cfg.instance)), wide(&format!("Local\\{}-Quit", cfg.instance)), wide(&format!("Local\\{}-Open", cfg.instance)))
}

fn instance() -> HINSTANCE {
    // SAFETY: the module handle of this program.
    unsafe { GetModuleHandleW(std::ptr::null()) }
}

// ---------------------------------------------------------------- icons

fn load_icon(cfg: &Config, state: &str, size: i32) -> HICON {
    let file = wide_path(&cfg.icon_dir.join(format!("agent-wiki-{state}.ico")));
    // SAFETY: a NUL-terminated path; LR_LOADFROMFILE reads it.
    unsafe { LoadImageW(std::ptr::null_mut(), file.as_ptr(), IMAGE_ICON, size, size, LR_LOADFROMFILE) as HICON }
}

/// How many of the three state icons load at 16 px (--selftest).
pub fn icons_loaded(cfg: &Config) -> usize {
    ["healthy", "degraded", "down"]
        .iter()
        .filter(|s| {
            let h = load_icon(cfg, s, 16);
            if h.is_null() {
                return false;
            }
            // SAFETY: an icon we loaded.
            unsafe { DestroyIcon(h) };
            true
        })
        .count()
}

fn state_index(state: &str) -> usize {
    match state {
        "healthy" => 0,
        "degraded" => 1,
        _ => 2,
    }
}

// ---------------------------------------------------------------- the notification-area icon

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: a plain C struct; zero is a valid starting state.
    let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n
}

fn add_icon(app: &mut App) {
    if !app.cfg.show_icon {
        return;
    }
    let mut n = nid(app.hwnd);
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = app.icons[state_index(app.last_state.unwrap_or("down"))];
    fill(&mut n.szTip, &if app.last_state.is_some() { app.status.tooltip() } else { "Agent Wiki: starting".into() });
    // SAFETY: a fully initialized struct for our own window.
    unsafe {
        app.icon_added = Shell_NotifyIconW(NIM_ADD, &n) != 0;
        n.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &n);
    }
}

fn update_icon(app: &App) {
    if !app.icon_added {
        return;
    }
    let mut n = nid(app.hwnd);
    n.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    n.hIcon = app.icons[state_index(app.status.state())];
    fill(&mut n.szTip, &app.status.tooltip());
    // SAFETY: as above.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
}

#[derive(Clone, Copy)]
enum Level {
    Info,
    Warning,
    Error,
}

fn balloon(app: &App, title: &str, text: &str, level: Level) {
    if !app.icon_added {
        return;
    }
    let mut n = nid(app.hwnd);
    n.uFlags = NIF_INFO;
    fill(&mut n.szInfoTitle, title);
    fill(&mut n.szInfo, if text.is_empty() { " " } else { text });
    n.dwInfoFlags = match level {
        Level::Info => NIIF_INFO,
        Level::Warning => NIIF_WARNING,
        Level::Error => NIIF_ERROR,
    };
    // SAFETY: as above.
    unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
}

fn remove_icon(app: &mut App) {
    if app.icon_added {
        let n = nid(app.hwnd);
        // SAFETY: as above.
        unsafe { Shell_NotifyIconW(NIM_DELETE, &n) };
        app.icon_added = false;
    }
}

// ---------------------------------------------------------------- polling

fn poll(app: &App) {
    if app.polling.swap(true, Ordering::SeqCst) {
        return;
    }
    let (cfg, flag) = (app.cfg.clone(), app.polling.clone());
    std::thread::spawn(move || {
        let s = Status::fetch(&cfg);
        flag.store(false, Ordering::SeqCst);
        post(Event::Status(s));
    });
}

fn check_autostart(app: &App) {
    if app.checking.swap(true, Ordering::SeqCst) {
        return;
    }
    let (cfg, flag) = (app.cfg.clone(), app.checking.clone());
    std::thread::spawn(move || {
        let r = autostart::check(&cfg);
        flag.store(false, Ordering::SeqCst);
        post(Event::Autostart(r));
    });
}

fn apply_status(app: &mut App, s: Status) {
    app.status = s;
    let state = app.status.state();
    update_icon(app);
    if let Some(last) = app.last_state
        && last != state
    {
        let settled = app.started.elapsed().as_secs() > 60;
        if state == "down" && settled {
            balloon(app, "Agent Wiki service is not responding", "Apps fall back to their own server meanwhile. Right-click the icon to restart the service.", Level::Warning);
        } else if state == "degraded" {
            let r = app.status.reasons();
            balloon(app, "Agent Wiki needs attention", r.first().map(String::as_str).unwrap_or("See the tray menu."), Level::Warning);
        } else if state == "healthy" && settled {
            balloon(app, "Agent Wiki is back to normal", "", Level::Info);
        }
    }
    app.last_state = Some(state);
    if let Some((title, text)) = app.announced.take(&app.status) {
        balloon(app, &title, &text, Level::Info);
    }
    if let Some(line) = crate::keyhandoff::offer(&app.cfg, &app.status) {
        crate::log::tray(&app.cfg, &line);
    }
}

/// Logs the start-at-sign-in entries once, then every change with the time it was noticed, so a
/// vanished entry can be matched to whatever removed it. Only reports.
fn apply_autostart(app: &mut App, r: Result<Autostart, String>) {
    let a = match r {
        Ok(a) => a,
        Err(e) => {
            crate::log::tray(&app.cfg, &format!("start-at-sign-in check failed: {e}"));
            return;
        }
    };
    let before = app.autostart.replace(a.clone());
    let prev = std::mem::replace(&mut app.autostart_checked_at, chrono_now());
    match before {
        None => crate::log::tray(&app.cfg, &format!("start at sign-in: {a}")),
        Some(b) if b != a => {
            crate::log::tray(&app.cfg, &format!("start at sign-in changed: {b} -> {a} (between {prev} and now)"));
            if b.starts() && !a.starts() {
                balloon(app, "Agent Wiki will not start at sign-in", "Its logon task and Run key entry are gone. Right-click > Start at sign-in > Repair.", Level::Warning);
            }
        }
        _ => {}
    }
}

fn chrono_now() -> String {
    let s = aw_core::text::local_iso(&aw_core::text::now());
    s[..19].to_string()
}

// ---------------------------------------------------------------- the menu

fn show_menu(hwnd: HWND) {
    let Some(items) = with_app(|a| menu::items(&a.cfg, &a.status, a.autostart.as_ref())) else { return };
    let mut actions: Vec<String> = vec![];
    // SAFETY: menus we create and destroy here; strings outlive the AppendMenu calls.
    let chosen = unsafe {
        let root = CreatePopupMenu();
        let mut subs: Vec<(&'static str, HMENU)> = vec![];
        for it in &items {
            let target = if it.sub.is_empty() {
                root
            } else if let Some((_, h)) = subs.iter().find(|(n, _)| *n == it.sub) {
                *h
            } else {
                let h = CreatePopupMenu();
                let label = wide(it.sub);
                AppendMenuW(root, MF_POPUP, h as usize, label.as_ptr());
                subs.push((it.sub, h));
                h
            };
            if it.text == "-" {
                AppendMenuW(target, MF_SEPARATOR, 0, std::ptr::null());
                continue;
            }
            let text = wide(&it.text);
            if it.action.is_empty() {
                AppendMenuW(target, MF_STRING | MF_GRAYED, 0, text.as_ptr());
            } else {
                actions.push(it.action.clone());
                AppendMenuW(target, MF_STRING, actions.len(), text.as_ptr());
            }
        }
        SetMenuDefaultItem(root, 0, 1); // "Open Agent Wiki", in bold
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let id = TrackPopupMenuEx(root, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY, pt.x, pt.y, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(root); // destroys the submenus too
        id
    };
    if chosen > 0
        && let Some(action) = actions.get(chosen as usize - 1)
    {
        run_action(action);
    }
}

fn run_action(action: &str) {
    if action == "quit" {
        quit();
        return;
    }
    if action == "open-ui" {
        open_ui();
        return;
    }
    let Some((cfg, curator)) = with_app(|a| {
        if action == "restart-service" {
            balloon(a, "Restarting the Agent Wiki service", "Apps reconnect on their own.", Level::Info);
        }
        (a.cfg.clone(), a.curator.clone())
    }) else {
        return;
    };
    if SLOW.contains(&action) {
        let action = action.to_string();
        std::thread::spawn(move || {
            let result = actions::run(&cfg, &action, &Ctx { curator, open_ui: None });
            post(Event::Done { action, result });
        });
        return;
    }
    let result = actions::run(&cfg, action, &Ctx { curator, open_ui: None });
    done(action, result);
}

fn done(action: &str, result: Result<Option<String>, String>) {
    with_app(|a| {
        match &result {
            Ok(Some(m)) => balloon(a, "Agent Wiki", m, Level::Info),
            Err(e) => balloon(a, if SLOW.contains(&action) { "Agent Wiki: that did not work" } else { "Agent Wiki" }, e, Level::Error),
            _ => {}
        }
        poll(a);
        if action == "repair-autostart" {
            check_autostart(a);
        }
    });
}

fn quit() {
    let curator = with_app(|a| {
        // SAFETY: timers and the icon of our own window.
        unsafe {
            KillTimer(a.hwnd, TIMER_POLL);
            KillTimer(a.hwnd, TIMER_AUTOSTART);
        }
        remove_icon(a);
        a.curator.take()
    })
    .flatten();
    if let Some(c) = curator {
        c.stop();
    }
    let window = with_app(|a| a.window.take()).flatten();
    drop(window);
    // SAFETY: plain call.
    unsafe { PostQuitMessage(0) };
}

// ---------------------------------------------------------------- the window

struct Hwnd(HWND);

impl HasWindowHandle for Hwnd {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let h = NonZeroIsize::new(self.0 as isize).ok_or(HandleError::Unavailable)?;
        // SAFETY: a live window owned by this thread for the borrow's lifetime.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(h))) })
    }
}

struct UiWindow {
    hwnd: HWND,
    _webview: wry::WebView,
    _context: Box<wry::WebContext>,
}

impl Drop for UiWindow {
    fn drop(&mut self) {
        // SAFETY: our own window; the webview (a child) goes with it.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

/// Where a new window goes: above the tray, on the monitor under the mouse, sized for its DPI.
fn placement() -> (i32, i32, i32, i32) {
    // SAFETY: plain queries with valid out structs.
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = |v: i32| v * dx as i32 / 96;
        let wa = mi.rcWork;
        let w = scale(WIDTH).min(wa.right - wa.left - 2 * scale(MARGIN));
        let h = scale(HEIGHT).min(wa.bottom - wa.top - 2 * scale(MARGIN));
        (wa.right - w - scale(MARGIN), wa.bottom - h - scale(MARGIN), w, h)
    }
}

/// The selftest's description of where the window would open.
pub fn window_line(cfg: &Config) -> String {
    let (x, y, w, h) = placement();
    format!("webview {w}x{h} at {x},{y} data={}", cfg.webview_dir.display())
}

fn dark_mode() -> bool {
    crate::winutil::hkcu_get_dword(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "AppsUseLightTheme") == Some(0)
}

fn register_window_class(icon_big: HICON, icon_small: HICON) {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        let class = wide("AgentWikiWindow");
        // SAFETY: a fully initialized class with a static name.
        unsafe {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(ui_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance(),
                hIcon: icon_big,
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
                hIconSm: icon_small,
            };
            RegisterClassExW(&wc);
            std::mem::forget(class);
        }
    });
}

fn create_window(cfg: &Config) -> Result<UiWindow, String> {
    register_window_class(load_icon(cfg, "healthy", 32), load_icon(cfg, "healthy", 16));
    let (x, y, w, h) = placement();
    let (class, title) = (wide("AgentWikiWindow"), wide("Agent Wiki"));
    // SAFETY: a registered class; the window is owned by this thread.
    let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW, x, y, w, h, std::ptr::null_mut(), std::ptr::null_mut(), instance(), std::ptr::null()) };
    if hwnd.is_null() {
        return Err("cannot create the window".into());
    }
    if dark_mode() {
        let on: i32 = 1;
        // SAFETY: a valid window and attribute size.
        unsafe {
            windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(hwnd, 20 /* DWMWA_USE_IMMERSIVE_DARK_MODE */, (&on as *const i32).cast(), 4)
        };
    }
    let _ = std::fs::create_dir_all(&cfg.webview_dir);
    let mut context = Box::new(wry::WebContext::new(Some(cfg.webview_dir.clone())));
    let nav_origin = format!("http://127.0.0.1:{}/", cfg.port);
    let webview = {
        use wry::WebViewBuilderExtWindows;
        wry::WebViewBuilder::new_with_web_context(&mut context)
            .with_url(cfg.ui_url())
            .with_focused(true)
            .with_navigation_handler(move |url| {
                // The window shows Agent Wiki only; links elsewhere open in the default browser.
                if url.starts_with(&nav_origin) || url == "about:blank" {
                    return true;
                }
                let _ = crate::winutil::shell_open(&url);
                false
            })
            .with_new_window_req_handler(move |url, _| {
                if url.starts_with("http://") || url.starts_with("https://") {
                    let _ = crate::winutil::shell_open(&url);
                }
                wry::NewWindowResponse::Deny
            })
            .with_theme(if dark_mode() { wry::Theme::Dark } else { wry::Theme::Light })
            // A text window needs no GPU process or spare renderer: 40 MB less while it is open (measured
            // 2026-10-03). AGENT_WIKI_WEBVIEW_ARGS replaces these, for troubleshooting.
            .with_additional_browser_args(
                std::env::var("AGENT_WIKI_WEBVIEW_ARGS")
                    .unwrap_or_else(|_| "--disable-gpu --disable-features=msSmartScreenProtection,msWebOOUI,msPdfOOUI,SpareRendererForSitePerProcess --renderer-process-limit=1 --no-first-run".into()),
            )
            .build(&Hwnd(hwnd))
    };
    let webview = match webview {
        Ok(v) => v,
        Err(e) => {
            // SAFETY: our own window.
            unsafe { DestroyWindow(hwnd) };
            return Err(format!("cannot start WebView2: {e}"));
        }
    };
    // SAFETY: our own window.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
    Ok(UiWindow { hwnd, _webview: webview, _context: context })
}

fn focus(hwnd: HWND) {
    // SAFETY: plain calls on our own window.
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else {
            ShowWindow(hwnd, SW_SHOW);
        }
        SetForegroundWindow(hwnd);
    }
}

/// Opens the window, or brings the open (or hidden) one back.
fn open_ui() {
    let Some((cfg, existing, tray)) = with_app(|a| (a.cfg.clone(), a.window.as_ref().map(|w| w.hwnd), a.hwnd)) else { return };
    if !cfg.window {
        crate::log::tray(&cfg, "open-ui requested (window=0)");
        return;
    }
    if let Some(h) = existing {
        // SAFETY: our own timer.
        unsafe { KillTimer(tray, TIMER_FREE_WINDOW) };
        focus(h);
        return;
    }
    // Not under a borrow: creating WebView2 pumps messages, which re-enter the window procedures.
    match create_window(&cfg) {
        Ok(w) => {
            with_app(|a| a.window = Some(w));
        }
        Err(e) => {
            crate::log::tray(&cfg, &format!("window: {e}; opening the default browser"));
            let _ = crate::winutil::shell_open(&cfg.ui_url());
        }
    }
}

unsafe extern "system" fn ui_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CLOSE => {
            // Hide now, free it after a while: reopening right away is instant, an idle tray stays small.
            // SAFETY: our own windows.
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            let tray = TRAY_HWND.load(Ordering::SeqCst);
            if tray != 0 {
                unsafe { SetTimer(tray as HWND, TIMER_FREE_WINDOW, FREE_AFTER_MS, None) };
            } else {
                // The standalone window process: closing ends it.
                STANDALONE.with(|s| {
                    if let Ok(mut s) = s.try_borrow_mut()
                        && let Some((_, w)) = s.as_mut()
                    {
                        w.take();
                    }
                });
                unsafe { PostQuitMessage(0) };
            }
            0
        }
        WM_GETMINMAXINFO => {
            // SAFETY: lParam points at a MINMAXINFO for this message.
            let mmi = unsafe { &mut *(lp as *mut MINMAXINFO) };
            mmi.ptMinTrackSize = POINT { x: 360, y: 480 };
            0
        }
        WM_DPICHANGED => {
            // SAFETY: lParam points at the suggested RECT for this message.
            let r = unsafe { &*(lp as *const RECT) };
            unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
            0
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

// ---------------------------------------------------------------- the tray window procedure

static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if Some(&msg) == TASKBAR_CREATED.get() {
        // Explorer restarted: put the icon back.
        with_app(|a| {
            a.icon_added = false;
            add_icon(a);
        });
        return 0;
    }
    match msg {
        WM_TRAY => {
            match (lp & 0xffff) as u32 {
                NIN_SELECT | NIN_KEYSELECT | NIN_BALLOONUSERCLICK => open_ui(),
                WM_CONTEXTMENU => show_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_EVENT => {
            let events: Vec<Event> = std::mem::take(&mut *EVENTS.lock().unwrap());
            for e in events {
                match e {
                    Event::Status(s) => {
                        with_app(|a| apply_status(a, s));
                    }
                    Event::Autostart(r) => {
                        with_app(|a| apply_autostart(a, r));
                    }
                    Event::Quit => quit(),
                    Event::Open => open_ui(),
                    Event::Done { action, result } => done(&action, result),
                }
            }
            0
        }
        WM_TIMER => {
            match wp {
                TIMER_POLL => {
                    with_app(|a| poll(a));
                }
                TIMER_AUTOSTART => {
                    with_app(|a| check_autostart(a));
                }
                TIMER_FREE_WINDOW => {
                    // SAFETY: our own timer.
                    unsafe { KillTimer(hwnd, TIMER_FREE_WINDOW) };
                    let w = with_app(|a| {
                        let visible = a.window.as_ref().is_some_and(|w| unsafe { IsWindowVisible(w.hwnd) } != 0);
                        if visible { None } else { a.window.take() }
                    })
                    .flatten();
                    drop(w);
                }
                _ => {}
            }
            0
        }
        WM_ENDSESSION if wp != 0 => {
            quit();
            0
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn message_loop() {
    // SAFETY: the standard loop for windows owned by this thread.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn dpi_aware() {
    // SAFETY: plain call; fails harmlessly if already set.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// The tray. Returns the exit code: 0, or 3 when another instance runs in this session.
pub fn run(cfg: Config, from: &str) -> i32 {
    let (mutex_name, quit_name, open_name) = names(&cfg);
    // SAFETY: named kernel objects; handles live for the life of the process.
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 1, mutex_name.as_ptr()) };
    if mutex.is_null() || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // Already running in this session (both sign-in entries start it): bring its window up.
        // SAFETY: opening an existing event by name.
        let open = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, open_name.as_ptr()) };
        if !open.is_null() && from == "manual" {
            unsafe {
                SetEvent(open);
                CloseHandle(open);
            }
        }
        return 3;
    }
    dpi_aware();
    crate::log::tray(&cfg, &format!("started pid {} (from {from}): {}", std::process::id(), autostart::exe_path().display()));
    let class = wide("AgentWikiTrayWindow");
    let title = wide("Agent Wiki tray");
    // SAFETY: registering our class and creating a hidden top-level window (menus need one that can be foreground).
    let hwnd = unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(tray_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance(),
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);
        let _ = TASKBAR_CREATED.set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()));
        CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), instance(), std::ptr::null())
    };
    TRAY_HWND.store(hwnd as isize, Ordering::SeqCst);
    // SAFETY: plain metric query.
    let small = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let icons = ["healthy", "degraded", "down"].map(|s| {
        let h = load_icon(&cfg, s, small);
        // SAFETY: the stock icon when ours is missing.
        if h.is_null() { unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) } } else { h }
    });
    let curator = cfg.host_curator.then(|| CuratorHost::start(&cfg));
    let app = App {
        cfg: cfg.clone(),
        hwnd,
        icons,
        icon_added: false,
        status: Status::default(),
        last_state: None,
        announced: crate::status::Announced::load(&cfg.state_file),
        started: Instant::now(),
        autostart: None,
        autostart_checked_at: chrono_now(),
        curator,
        window: None,
        polling: Arc::new(AtomicBool::new(false)),
        checking: Arc::new(AtomicBool::new(false)),
    };
    APP.with(|a| *a.borrow_mut() = Some(app));
    with_app(|a| {
        add_icon(a);
        poll(a);
        check_autostart(a);
    });
    // SAFETY: timers on our own window.
    unsafe {
        SetTimer(hwnd, TIMER_POLL, 5000, None);
        SetTimer(hwnd, TIMER_AUTOSTART, (cfg.autostart_check_seconds * 1000) as u32, None);
    }
    // --quit and "open the window" from other processes: named events, each waited on by a thread.
    for (name, event) in [(quit_name, 0u8), (open_name, 1u8)] {
        // SAFETY: an auto-reset named event that lives for the life of the process.
        let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) } as isize;
        std::thread::spawn(move || {
            loop {
                // SAFETY: a valid event handle.
                if unsafe { WaitForSingleObject(h as HANDLE, INFINITE) } != 0 {
                    return;
                }
                post(if event == 0 { Event::Quit } else { Event::Open });
                if event == 0 {
                    return;
                }
            }
        });
    }
    message_loop();
    crate::log::tray(&cfg, "quit");
    // SAFETY: releasing our mutex before exit, so a waiting --quit sees it gone.
    unsafe {
        windows_sys::Win32::System::Threading::ReleaseMutex(mutex);
        CloseHandle(mutex);
    }
    0
}

/// `--quit`: asks the running instance to quit and waits (up to 20 s) until it has. 0 = not running any more.
pub fn signal_quit(cfg: &Config) -> i32 {
    let (mutex_name, quit_name, _) = names(cfg);
    // SAFETY: opening named objects; handles closed below.
    unsafe {
        let ev = OpenEventW(EVENT_MODIFY_STATE, 0, quit_name.as_ptr());
        if ev.is_null() {
            return 0;
        }
        SetEvent(ev);
        CloseHandle(ev);
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while Instant::now() < deadline {
            let m = OpenMutexW(SYNCHRONIZE, 0, mutex_name.as_ptr());
            if m.is_null() {
                return 0;
            }
            CloseHandle(m);
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    1
}

/// `--do open-ui` from another process: the running tray opens its window; with none running, a
/// window of its own.
pub fn request_open(cfg: &Config) -> Result<(), String> {
    let (_, _, open_name) = names(cfg);
    // SAFETY: opening a named event; closed below.
    unsafe {
        let ev = OpenEventW(EVENT_MODIFY_STATE, 0, open_name.as_ptr());
        if !ev.is_null() {
            SetEvent(ev);
            CloseHandle(ev);
            return Ok(());
        }
    }
    if !cfg.window {
        return Ok(());
    }
    std::process::Command::new(autostart::exe_path()).args(["--window", "--config"]).arg(&cfg.file).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// `--window`: only the window, in this process, until it is closed.
pub fn run_window(cfg: Config) -> i32 {
    dpi_aware();
    match create_window(&cfg) {
        Ok(w) => {
            STANDALONE.with(|s| *s.borrow_mut() = Some((cfg, Some(w))));
            message_loop();
            STANDALONE.with(|s| s.borrow_mut().take());
            0
        }
        Err(e) => {
            crate::log::tray(&cfg, &format!("window: {e}; opening the default browser"));
            let _ = crate::winutil::shell_open(&cfg.ui_url());
            1
        }
    }
}
