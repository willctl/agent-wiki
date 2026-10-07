# Search and read

Apps search the wiki before asking the user for context. `wiki_search` ranks pages, log entries and
notes and returns the matching section; `wiki_read` opens a page, a section or a note. The window
has the same search in its header.

## Sub-features

- `search-section`: a query returns the page and the section that matched.
- `search-scope`: `scope` limits results to pages, the log or notes.
- `read-page`: `wiki_read` returns the page text, with remote images turned into plain links.
- `window-search`: the window's search box shows the same pages.

## How to get to it (user POV)

- Apps call `wiki_search` and `wiki_read`.
- The window: the search box at the top ("Search pages, notes and the log"; `/` focuses it).

## Driving it with verify.mjs

Preconditions: a fresh instance; `V doctor` is ok.

- **search-section.** `V mcp wiki_search '{"query":"postgres upgrade rollback"}'` lists `postgres-upgrade` first, with the matching section's heading.
- **search-scope.** `V mcp wiki_search '{"query":"harbor","scope":"pages"}'` returns only pages.
- **read-page.** `V mcp wiki_read '{"target":"harbor"}'` returns the page with its frontmatter summary.
- **window-search.** In the browser pane, type `lisbon` in the search box; the results include `Lisbon trip`. `V api GET '/api/search?q=lisbon'` returns the same page.

## Gotchas

- Search is lexical (BM25 over sections); use words that appear in the page.
- Results include pending notes, so a note logged in the same run can outrank a page.
