---
name: verify-agent-wiki
description: Drive Agent Wiki the way a user and an app do (MCP tools, the curator, the window at /ui/ and its /api, the tray's self-test) on an isolated copy of the synthetic wiki and a spare port, and keep the evidence. Use it to prove a change to the Rust server, curator, window or tray works end to end before you call it done, or when a user-facing behaviour looks wrong.
---

# Verify Agent Wiki

The installed service on port 47821 serves the user's real wiki. Never drive it. This skill starts a
second server on its own port, over a copy of `eval/synthetic/wiki` (15 pages about a fictional team),
with the fake model (`test/fixtures/fake-codex.mjs`) as the curator's model. Everything goes through
one helper, `helpers/verify.mjs`, which records each request and response in
`.tmp/verify-evidence/<run>/transcript.jsonl`.

Run every command from the repository root. Below, `V` stands for
`node .claude/skills/verify-agent-wiki/helpers/verify.mjs`.

## Launch

1. Build what you changed. The window: `npm run build` (writes `dist/runtime/ui`). The Rust programs:
   `cargo build --release` in `rust/`. The helper uses `rust/target/release`, then
   `rust/target/debug`, or `AGENT_WIKI_RUST_BIN`. A Windows machine without cargo can build in WSL
   with `cargo build --release --target x86_64-pc-windows-gnu` and copy `agent-wiki.exe` and
   `agent-wiki-tray.exe` into `rust/target/release`.
2. Start: `V start --run <name>`. Options: `--port <n>` (default 47897), `--wiki <dir>` (another wiki
   to copy), `--real-model` (the curator uses your own config's model and Codex sign-in; it costs
   model calls, so use it only to check model behaviour).
3. Ready means `start` printed `started "<name>"` with the window and MCP URLs. It fails, and says
   why, when the port already answers or the server does not come up within 15 s
   (`server.log` in the evidence folder has its output).

Only one instance runs at a time; `start` refuses a second one.

## Doctor

`V doctor` checks that the port is served by the process this run started, over this run's wiki,
at the version in `package.json`. It exits 1 with the reason otherwise (most often: the Rust build
is older than the source, so rebuild). Run it first, and again whenever a result looks wrong.

## Drive

| What a user or app does | Command |
| --- | --- |
| An app calls a tool | `V mcp wiki_log '{"app":"codex","source":"external","title":"...","body":"...","pages":["harbor"]}'` (tools: `wiki_start`, `wiki_search`, `wiki_read`, `wiki_log`, `wiki_upsert_page`) |
| The curator files what is waiting | `V curate` (one pass with the fake model, then exits); `V curate --lint` runs the scheduled page review (the cleanup) now |
| The window asks the service | `V api GET /api/held`, `V api POST /api/settings '{"approvals":"manual"}'` (the helper sends the `X-Agent-Wiki: ui` header the window sends) |
| Look at a file the way the user would | `V file pages/harbor.md`, `V file .curator/settings.json` |
| The tray reads the service | `V tray` prints the tray's `--selftest`: state, tooltip and menu |
| A person uses the window | Open `http://127.0.0.1:<port>/ui/` in the browser pane. Find controls by role and name: tabs `Pages`, `Ask`, `Activity`, `Inbox`, `Status`; radios `Automatic` and `Ask me first`; buttons `Undo`, `Approve`, `Reject`, `Send back`, `Revert`. |

The fake model files each note into the page named in `pages` (it creates the page if needed) and
writes one log entry per note. A page review replaces the word `LINTME` with `tidied`. See the top
of `test/fixtures/fake-codex.mjs` for its other modes.

`features/README.md` maps the user-facing features, with a recipe for each.

## Evidence

A proof is the action and the state it produced, both from the outside:

- The transcript lines for the calls you made (the helper writes them).
- A second, read-only view of the result: `V file <page>` after a change, `V api GET` for what the
  window shows, and a browser-pane screenshot or `read_page` for UI behaviour.
- For anything the curator does, the log entry it wrote (`V file log/<yyyy>/<yyyy-mm-dd>.md`).

Do not prove behaviour through internal setters or by editing `.curator/` files, except to set up a
state the user could reach (such as `.curator/settings.json`, which the window writes). The fake
model is the only stand-in; everything else is the shipped program.

## Gotchas

- In Git Bash on Windows, set `MSYS_NO_PATHCONV=1`. Otherwise Git Bash turns `/status` into
  `C:/Program Files/Git/status` (the helper stops and says so).
- A fresh copy of the synthetic wiki has one pending note dated 2026-10-03 (part of the eval
  fixture), so `/status` reads `degraded` with a backlog reason until the first `V curate`.
- The window asks for confirmation with the browser's own `confirm()` before Undo and some other
  actions. The browser pane cannot answer that dialog, so set
  `window.confirm = () => true` with the pane's JavaScript tool before clicking, and say so in the
  proof.
- If the browser pane does not draw (the Claude window is behind another window), screenshots and
  coordinate clicks time out. `read_page`, `find` and the JavaScript tool still work.

## Cleanup

`V stop` stops the server this run started (only if it still serves the run's port), deletes the
run's copy of the wiki in `.tmp/verify/<run>/`, and keeps `.tmp/verify-evidence/<run>/`. Run it after
every attempt, including failed ones. It never kills by process name.
