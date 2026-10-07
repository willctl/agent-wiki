# Revert

The window's Activity tab lists each page change the curator made. Revert puts the page back as it
was before that batch, if the page is still exactly what the batch wrote. Otherwise it offers to ask
the curator to undo the change on the current page, as a note from the user.

## Sub-features

- `revert-exact`: Revert restores the earlier page byte for byte.
- `revert-refused`: a second Revert, or one after a later edit, returns 409.
- `revert-ask`: "ask the curator instead" queues a note with `source: user`.

## How to get to it (user POV)

- Window > Activity: a "Page updated: <title>" entry with a `Revert` button.

## Driving it with verify.mjs

Preconditions: a fresh instance; `V doctor` is ok.

- **Set up.** `V file pages/harbor.md` (keep the text). Log a note for `harbor` from `claude-code` without a `source` and run `V curate`.
- **revert-exact.** `V api GET '/api/activity?days=1'`, take the entry titled `Page updated: Harbor` and its `batch`. `V api POST /api/revert '{"batch":"<batch>","slug":"harbor"}'` returns 200 and `V file pages/harbor.md` matches the text you kept.
- **revert-refused.** The same POST again returns 409.
- **revert-ask.** `V api POST /api/revert '{"batch":"<batch>","slug":"harbor","action":"ask"}'` returns a note id; `V file inbox/<note>.md` has `source: user`.

## Gotchas

- Revert compares against the audit record's hashes, so any edit to the page after the batch makes
  it refuse. That is intended.
