# Agent Wiki: notes for coding agents

Agent Wiki is one shared memory for several AI apps: an MCP server over a plain-Markdown wiki, a
curator that files notes into pages, a tray app with a window, and an installer. README.md
describes what it does; this file is what an agent changing the code needs.

## Where things are

- `rust/`: the programs that ship. `crates/core` (aw-core: wiki, curator, held changes, search),
  `crates/cli` (`agent-wiki`: MCP server, HTTP service, window API, installer), `crates/tray`
  (`agent-wiki-tray`).
- `ui/`: the window (React), built by `npm run build` into `dist/runtime/ui`.
- `src/`, `service/`, `tray/*.cs`, `scripts/install.mjs`: the earlier Node runtime and C# programs.
  They are retired by R5 (`docs/rust-plan.md`). Do not add features there.
- `test/*.mjs`: the behaviour suites for both implementations. `eval/` and `scripts/eval.mjs`:
  the memory eval. `protocol/PROTOCOL.md`: what every connected app is told to do.

## Build and check

- `npm run build`: the window, and a check that `package.json` and `rust/Cargo.toml` agree on the
  version.
- `scripts/check-rust.sh` (Linux, WSL or macOS): rustfmt, clippy for this OS, Windows and macOS,
  and the Rust unit tests. Run it before pushing Rust changes. On Windows without cargo, run it in
  WSL.
- `AGENT_WIKI_IMPL=rust node --test test/<suite>.mjs`: the suites against the Rust programs in
  `rust/target/release` (or `AGENT_WIKI_RUST_BIN`). A plain `npm test` runs them against the Node
  runtime and skips the Rust-only tests (the trust gate, held changes, forget, the Rust installer
  and tray), so run both.
- `.claude/skills/verify-agent-wiki`: drive a change end to end on an isolated instance and keep
  the evidence.

## Rules and what enforces them

| Rule | Enforced by |
| --- | --- |
| Public commits use the repository identity; private paths and data stay out of tracked files | `scripts/check-public.mjs`, `.githooks/`, and CI; see `SECURITY.md` |
| Lint every OS target before pushing Rust code | `scripts/check-rust.sh` (local), the CI matrix (after the push) |
| `package.json` and `rust/Cargo.toml` carry the same version | `scripts/build.mjs` fails |
| A change to `protocol/PROTOCOL.md` records the old version's hash | `wiki::tests::an_edited_protocol_keeps_the_old_hash_for_upgrades` |
| Regexes match ASCII digits (`[0-9]`, never `\d`) | `wiki::tests::no_regex_in_the_crates_uses_unicode_digit_classes` |
| Tools are defined once; the `source` enum is `inbox::SOURCES` | `mcp::tool_tests::one_tool_list_with_the_direct_texts_and_the_core_sources` |
| Files in the wiki folder are untrusted input (held changes, journals, settings) | `held::parse_file`, `curator::journal_problem`, `settings::approvals_and_problem`, and their tests |
| The approval mode is read once per batch, under the commit's lock | `Journal.auto`, `held::apply_auto_locked`; the auto-mode tests in `test/curator.mjs` |
| The installer leaves an app's config alone while that app runs | Claude desktop: `install::desktop_running` and its sandbox test. Codex and Claude Code: judgment |
| Tests and verification never touch the installed service (port 47821) or the real wiki | The verify skill's isolated instance; judgment |
| Commit messages are Conventional Commits; finished work is committed and pushed | Judgment (the owner's standing preference) |
| Programs keep files in the platform's directories (XDG, AppData, Library), never a new `~/.something` | `aw_core::paths`; judgment |

When a mistake repeats, fix it at the highest level that works: remove the way to make it, then a
type, then a check whose error names the fix, then a test. Add the rule here last, with what
enforces it.
