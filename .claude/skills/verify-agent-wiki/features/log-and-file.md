# Log and file

An app tells the wiki what happened with `wiki_log`. The note is saved at once and shows as pending
in `wiki_start` and `wiki_search`. The curator then files it: it updates or creates the right page
and writes a log entry that links the note as its source.

## Sub-features

- `log-saved`: `wiki_log` returns a note id and the note is in `inbox/`.
- `log-pending`: before filing, `wiki_start` lists the note under pending notes.
- `file-page`: after the curator's pass, the page named in the note has the new fact.
- `file-log`: the day's log has an entry with a `sources: note:<id>` line.
- `secret-refused`: a note containing a secret-looking string is refused.

## How to get to it (user POV)

- Any connected app (Claude, ChatGPT, Codex) calls `wiki_log`.
- The window's Inbox tab lists notes waiting to be filed.

## Driving it with verify.mjs

Preconditions: a fresh instance; `V doctor` is ok.

- **log-saved.** `V mcp wiki_log '{"app":"claude-code","source":"user","title":"Harbor batch size is 750","body":"Raised from 500 on 2026-10-06.","pages":["harbor"]}'` prints `Saved note <id>`. `V file inbox/<id>.md` shows it.
- **log-pending.** `V mcp wiki_start '{"app":"claude-code","topic":"harbor"}'` lists the note under pending notes. `V api GET /api/inbox` lists it too.
- **file-page.** `V curate`, then `V file pages/harbor.md` contains `Harbor batch size is 750`.
- **file-log.** Today's log has an entry titled `Harbor batch size is 750` with `sources: note:<id>`; `V mcp wiki_read '{"target":"note:<id>"}'` opens the note as sent.
- **secret-refused.** `V mcp wiki_log` with a body containing `AKIA` followed by 16 capital letters or digits returns an error and no note is saved.

## Gotchas

- The fake model files a note only into the pages listed in `pages`; with none, it writes just the
  log entry.
- Notes are filed in batches after a short debounce; `V curate` forces one pass now.
