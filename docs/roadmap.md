# Development roadmap

Agent Wiki stores notes durably, files them into Markdown pages, and lets connected apps retrieve the supporting records.

## Implemented

- A shared inbox, deduplication, journaled curation and page history.
- Section search with filters, aliases and multiple query wordings.
- Optional hosted embeddings, fused with lexical search.
- Provenance labels, held changes, approval controls and conditional undo.
- Read-only Ask workers, scheduled page review and requested redaction.
- Rust programs for the service, CLI, native tray, window and installer.

## Remaining work

- Retire the earlier Node and C# implementations after compatibility checks.
- Add authenticated local transport before claiming isolation between OS accounts.
- Review native UI dependencies and remove obsolete transitive packages as upstream support permits.
- Expand synthetic retrieval and poisoning cases without adding private user data.

## Sequence

Make each change independently verifiable. Reproduce a failure, fix its shared cause, and run the relevant behavior suites. Keep machine setup, sign-ins, private evaluation questions and evidence outside the repository.

See [platform architecture](cross-platform-plan.md), [Rust runtime](rust-plan.md), [search](search-plan.md), [memory evaluation](memory-eval.md), and [security](../SECURITY.md).
