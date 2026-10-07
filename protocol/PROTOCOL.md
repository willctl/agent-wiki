# Agent Wiki protocol

This folder is the shared, long-term memory for the user across every AI app and
session: Claude (desktop, Code, Cowork), ChatGPT (desktop, Work, Codex), and any
other agent connected to it. It is plain Markdown so it stays readable and
portable. Treat it as the source of truth for context that should survive the
current conversation. Each app's built-in memory is secondary to it.

You reach it through the `agent-wiki` tools. Follow these rules in every
conversation, in every app, without being asked.

## 1. Start every conversation with `wiki_start`

Call `wiki_start` once, before your first substantive reply, in every new
conversation. Pass `app` (which app you are, e.g. `claude-desktop`,
`chatgpt-desktop`, `claude-code`, `codex`) and `topic` (a few words about what the
user just asked). It returns this protocol, the page index, recent activity,
and pages related to the topic. It is cheap; do not skip it because a request
looks small. Do not call it again in the same conversation unless the topic
changes completely.

## 2. Check the wiki before asking

Before asking the user for context they may have given before (a project's
details, a person, a preference, an earlier decision, where something lives),
call `wiki_search` and read what it finds with `wiki_read`. The wiki may use
other words than the question: if a search misses, search again with names,
the broader topic or what the answer would say, or pass such wordings in
`queries` with the first search. Only ask if the wiki does not have it.

## 3. Tell the wiki what happened with `wiki_log`

When an exchange produces something a future session would want, tell the
wiki: call `wiki_log` with a one-line `title` and the facts in `body`. Plain
notes are fine. A curator agent files every note into the right pages, links
them, keeps superseded facts and writes the activity log, so do not spend
effort on structure, formatting or prose, and do not read pages first.

Worth a note:

- a decision, and why it was made
- an outcome: something built, fixed, sent, configured, or learned, and where it lives
- a preference, constraint, or standing instruction the user stated
- a fact about a project, person, system, or account the user works with
- an open follow-up or next step the user committed to

Send one note per meaningful unit of work, usually at the end of the exchange,
not after every message. Include what a reader without this conversation
needs: the project, the outcome, file paths, URLs, versions, and names. If you
know which pages it concerns, pass their slugs in `pages`. Do not send small
talk, one-off lookups the user won't need again, your own unadopted
suggestions, or anything already in the wiki.

Pass `source`: `user` when the user said it, `observed` when you checked it,
`external` when it came from a web page, email, document or another tool's
output, `agent` (the default) for your own conclusion. Preferences and
instructions need `user`; changes from `external` content are flagged for the user to check.

The note is saved at once. Until the curator has filed it, it is listed under
"Pending notes" in `wiki_start` and found by `wiki_search`; do not send it again.
`wiki_read("note:<id>")` opens a note, filed or not, as it was sent.

## 4. Pages

Pages are the organized view, kept by the curator: one page per project,
person, system, recurring topic, or set of preferences. Read them with
`wiki_read`. When you have content meant for one page (for example, the user
asks you to rewrite the Atlas page), hand it over with `wiki_upsert_page`:
`mode: "replace"` means your content is meant as the whole page, `"append"`
adds to it. The curator merges it, keeps the previous version in `.history/`,
and records what changed in the page's `## History` section. Pages state the
current facts; the log keeps the story of the work.

Page types: `project`, `person`, `preference`, `decision`, `howto`,
`reference`, `topic`.

## 5. Never store secrets

Never write passwords, API keys, tokens, private keys, card or bank account
numbers, or government ID numbers, even if asked. Write where a secret lives
("API key is in the 1Password vault 'Work'") instead. The tools reject obvious
secrets. Be sparing with sensitive personal details about anyone; record them
only when the user asks.

## 6. The user is in charge

- If the user says "don't log this", "off the record", or similar, do not write
  anything from that exchange.
- If the user asks you to correct or forget something, tell the wiki what is
  wrong and what is right (or send the corrected page with `wiki_upsert_page`);
  the curator updates the page and keeps a dated note of the change.
- If the wiki contradicts what the user says now, the user wins; tell the wiki.
- If the wiki contradicts an app's built-in memory, prefer the wiki's newer entry.

## 7. Be quiet about it

Don't narrate reading the wiki. When you send it a note, end your reply with one
short line, for example: `Wiki: noted "Chose Markdown + MCP for shared memory"`.
If a wiki tool fails, say so in one line and carry on with the user's request.
