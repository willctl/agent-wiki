# Platform architecture

Agent Wiki ships two Rust programs and a React window. The CLI provides MCP over stdio or loopback HTTP, the curator, the session hook, and installation commands. The tray hosts the window and starts the curator where needed.

## Storage

Use platform directories from aw_core::paths. Windows uses AppData, macOS uses Library, and Linux uses XDG directories. The wiki itself is a user-selected Markdown folder. Do not put credentials in that folder.

## Services and windows

Windows uses a service running as NT SERVICE\AgentWiki and a per-user tray with WebView2. macOS uses LaunchAgents and WKWebView. Linux uses systemd user units and WebKitGTK. Installation must preserve user data, stop or replace only the program it owns, and leave a running client's configuration alone.

HTTP binds to 127.0.0.1. This restricts network access, but does not authenticate local OS accounts. See [SECURITY.md](../SECURITY.md) before using a shared machine.

## Verification

Run the Rust checks and behavior suites on all supported platforms. Use an isolated wiki, spare port and fake model for local verification. Native tray and webview behavior also needs a platform-specific check. A cross-compile alone does not prove native behavior.
