# Approvals

The curator flags some page changes: anything backed by `external` content, and, unless every note
behind it is from `user`, changes to preference pages, instruction-like lines and new links.
Scheduled cleanups (daily at 03:00 by default; the Cleanup schedule) propose page cleanups. With Automatic approvals (the default) these are applied at
once, the tray shows a notification, and the Inbox lists them for 7 days with Undo. With
"Ask me first" they wait in the Inbox for Approve, Reject or Send back. Forget requests always wait.

## Sub-features

- `auto-apply`: a flagged change is applied right after the curator's pass and logged as
  "Change applied automatically".
- `auto-notify`: `/status` reports `autoApplied` (total and last), which drives the tray notification.
- `auto-undo`: Undo in the Inbox restores the page, once, while nobody edited it since.
- `ask-first`: with "Ask me first" the change waits and the page is untouched.
- `switch-back`: switching to Automatic applies what was waiting.
- `startup-apply`: a curator that starts in Automatic mode applies what was left waiting.
- `cleanup-auto`: a scheduled cleanup is applied and logged as "Cleanup applied automatically".
- `cleanup-schedule`: the Cleanup schedule bar (Daily, Weekly, Custom cron with a preview of the
  next three times, Off) saves `cleanupSchedule` in `.curator/settings.json`; Off stops reviews,
  and on a schedule a pass runs once per scheduled time (catching up a missed one).

## How to get to it (user POV)

- Window > Inbox: the Approvals bar at the top with the `Automatic` and `Ask me first` radios, the
  Cleanup schedule bar under it (`Change` opens Daily, Weekly, Custom and Off), the
  "Needs your OK" section, and "Applied automatically · last 7 days" with `Undo` buttons.
- Window > Status > Curator: the `Approvals` row links to the Inbox.
- The tray notification ("Agent Wiki applied a change"); on Windows, clicking it opens the window.

## Driving it with verify.mjs

Preconditions: a fresh instance; `V doctor` is ok.

- **auto-apply.** Log external content: `V mcp wiki_log '{"app":"codex","source":"external","title":"Forum says Harbor moved","body":"A forum post says Harbor moved to port 9443.","pages":["harbor"]}'`, then `V curate`. `V file pages/harbor.md` contains `Forum says Harbor moved`. Today's log (`V file log/<yyyy>/<yyyy-mm-dd>.md`) has `curator · Change applied automatically: Harbor [[harbor]]` and no "waiting for your OK".
- **auto-notify.** `V api GET /status` has `"approvals":"auto"` and `autoApplied.total` 1 with `last.slug` `harbor`. `V tray` still reports the service state (the notification itself is a desktop balloon; its text comes from `autoApplied.last`).
- **auto-undo.** `V api GET /api/held` lists the change under `auto` with `status` `approved`. Undo it: `V api POST /api/held '{"batch":"<batch>","index":<index>,"action":"undo"}'` returns 200, `V file pages/harbor.md` no longer has the line, and a second identical POST returns 409. In the window, the card shows as undone.
- **ask-first.** In the window choose `Ask me first` (or `V api POST /api/settings '{"approvals":"manual"}'`). Log another external note for `harbor` and run `V curate`. The page does not change; `V api GET /api/held` lists it under `changes`; `/status` has `held` 1.
- **switch-back.** Choose `Automatic` (or POST `{"approvals":"auto"}`). The response says `"applied":1`; the page now has the change.
- **Proof.** Keep the transcript, plus a browser-pane screenshot of the Inbox showing the Approvals bar and the "Applied automatically" card.

## Gotchas

- `V curate` runs one pass and exits; a change appears only after it.
- Undo refuses (409) when the page changed after the change was applied. That is the intended
  behaviour, not a failure.
- The 7-day list and `autoApplied` read `.curator/held/`. A fresh copy of the synthetic wiki starts
  with none.
- The tray notification needs a running tray and a desktop session. The helper's `tray` command
  shows the tray's view of `/status`, not the balloon.
