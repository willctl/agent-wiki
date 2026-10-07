# Agent Wiki verification map

This folder is the maintained list of user-facing features and how to prove each one. Read this
index before driving, then use the feature's file as the recipe. `V` is
`node .claude/skills/verify-agent-wiki/helpers/verify.mjs`, run from the repository root.

## Baseline

- `V start --run <name>` started a fresh instance over a copy of `eval/synthetic/wiki`, and
  `V doctor` says ok. Each recipe assumes that fresh state unless it says otherwise.
- Approvals are at the default (Automatic) until a recipe changes them.
- Never drive the installed service on port 47821: it serves the user's real wiki.

## Conventions

- Apps reach the wiki through MCP tools (`V mcp`). The person reaches it through the window
  (browser pane at `/ui/`, or `V api` for the same requests the window sends).
- After each action, check the result from a second, read-only view: a page file, `/api/...`, or
  the window.
- In the window, find controls by role and accessible name, not position.

## Proof and skips

- Record the feature, the entry point you used, and the transcript lines that show it.
- If an entry point cannot be reached (for example no browser pane), say which and why. Do not
  report it as verified through a different entry point.

## Features

- [Log and file](./log-and-file.md): an app logs a note, the curator files it into a page and the log.
- [Approvals](./approvals.md): flagged changes and cleanups are applied automatically with a
  notification and Undo, or wait under "Ask me first".
- [Search and read](./search-and-read.md): apps and the window find pages, sections and notes.
- [Revert](./revert.md): the window puts back a page the curator changed.
- [Tray status](./tray-status.md): the tray's state, tooltip and menu follow the service.
- [Models](./models.md): the curator's and Ask's models, chosen from the account's list in the
  window or with `agent-wiki models`, checked, and used from the next run.
