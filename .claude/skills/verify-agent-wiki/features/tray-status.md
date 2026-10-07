# Tray status

The tray polls the service's `/status` and shows its state as the icon, a tooltip and the menu:
healthy, needs attention (with the reasons) or down. `agent-wiki-tray --selftest` prints that view
as text.

## Sub-features

- `tray-healthy`: a fresh instance reads as `state=healthy`.
- `tray-attention`: a change waiting for an OK makes it `degraded`, with the reason in the menu.
- `tray-down`: with the server stopped, `state=down`.

## How to get to it (user POV)

- The Agent Wiki icon in the notification area, its tooltip and its menu.

## Driving it with verify.mjs

Preconditions: a fresh instance; `V doctor` is ok; `agent-wiki-tray` is built next to `agent-wiki`.

- **tray-healthy.** `V tray` prints `state=healthy` and a tooltip starting `Agent Wiki <version>: OK`.
- **tray-attention.** Choose "Ask me first" (`V api POST /api/settings '{"approvals":"manual"}'`), log an external note for `harbor`, `V curate`, then `V tray`: `state=degraded` and an item naming `1 change(s) need your OK`.
- **tray-down.** After `V stop`, the helper cannot run `tray` (no instance). Prove `down` with the real tray only when you started one yourself.

## Gotchas

- `V tray` writes a throwaway tray.ini with the curator and the icon off, so it never starts a second
  curator or touches the user's autostart.
