# Models

The curator (filing notes, cleanups) and Ask (answering questions) each run a model through Codex.
The window's Status tab has a Models card: for each, a model from this ChatGPT account's list and a
reasoning level, or the defaults (config.json, then gpt-6.1-sol with medium reasoning for the
curator; Ask uses the curator's model with low reasoning). A change applies from the next run.

## Sub-features

- `models-list`: the curator copies Codex's list of the account's models (`models_cache.json` in its
  CODEX_HOME) into `.curator/models.json`, without the account's identity; `/api/models` serves it.
- `models-choose`: the window (`POST /api/models`) or `agent-wiki models set` saves the choice in
  `.curator/settings.json`, logs it, and the next run passes `-m <model>` and the reasoning effort.
- `models-refuse`: a model the account does not offer, or a reasoning level the model does not
  offer, is refused with what it does offer (the CLI's `--force` saves it anyway).
- `models-detect`: a chosen model the account no longer offers shows in `/status` reasons and the
  card; a model Codex refuses pauses the curator (no note fails) until another is chosen.
- `models-follow`: Ask uses the curator's model until it has its own.

## How to get to it (user POV)

- Window > Status > Models: two selects per role (model, reasoning).
- Tray > Curator: "Models: curator ... , Ask ..." and "Change models in the window...".
- Terminal: `agent-wiki models`, `agent-wiki models set curator|ask <model> [--effort <level>]`,
  `agent-wiki models reset curator|ask`.

## Driving it with verify.mjs

Preconditions: a fresh instance; `V doctor` is ok. The run's curator uses its own CODEX_HOME, which
has no `models_cache.json` until a real Codex has run there, so set one up to see the lists:

- **Set up.** Write a `models_cache.json` into the run's CODEX_HOME (`V api GET /api/status`, the
  curator's `codexHome`, or `config.json` `curator.codexHome`) with two or three models.
- **models-list.** Run the CLI (`agent-wiki models` with the run's `AGENT_WIKI_HOME` and
  `AGENT_WIKI_DIR`) or a watching curator; `V api GET /api/models` lists them under `available`.
- **models-choose.** `V api POST /api/models '{"role":"curator","model":"<slug>","reasoningEffort":"high"}'`
  returns the overview; log a note and `V curate`; the evidence folder's `model-calls.jsonl` has
  `-m <slug>`.
- **models-refuse.** The same POST with a slug not in the list returns 400 naming the offered ones.
- In the window: open Status, change the Curator model select, and check the toast and the
  "In use" line.

## Gotchas

- `test/curator.mjs` ("models: ...") covers the refused-model pause with the fake Codex's
  `refuse-model=<slug>` mode; the verify instance's fake model accepts any model name.
