# Security and code quality audit

Date: 2026-10-07. Scope: the Rust runtime, transports, storage, curator, held changes, Forget, credential handoff, UI rendering, installer, dependencies, publication files and Git metadata. Legacy Node and C# code received a limited comparison review and is not a supported installation target.

The review used pstack [interrogate](https://github.com/cursor/plugins/blob/main/pstack/skills/interrogate/SKILL.md) and [unslop](https://github.com/cursor/plugins/blob/main/pstack/skills/unslop/SKILL.md). Two independent reviewers examined the same scope, followed by a focused HTTP and UI review. The available reviewers used the same model; this was not the multi-model comparison suggested by the skill. Findings were checked against source and isolated reproductions before changes were made.

## Fixed findings

| Severity | Problem | Fix and evidence |
| --- | --- | --- |
| High | A forged idempotency record could supply a parent-traversal note ID and make a normal note write create a file outside the wiki. | Validate saved IDs and confine inbox, key, state, archive and temporary paths. An isolated Windows reproduction created an external file before the fix; regression tests reject it. |
| High | Forget followed directory links and junctions, allowing redaction of external files. | Reject links and reparse points during traversal, including request-log traversal. An isolated Windows junction reproduction modified an external fixture before the fix. |
| High | Edited held-change or audit records could make Undo/Revert copy an arbitrary external file into a wiki page. | Require page-specific history paths, paired hashes and confined reads and writes. A synthetic API reproduction returned external text before the fix. Tests cover traversal, absolute paths, wrong-page history and linked read/write destinations. |
| High | Planted page, history, journal or temporary-directory links could redirect curator reads, recovery or writes. | Share confinement checks across storage boundaries and reject invalid cross-platform history paths. Tests cover planted page, history and journal links. |
| Medium | Embedding request bodies were written to predictable temporary files with default permissions. | Send the body and credential through curl stdin, disable ambient curl configuration, validate bearer syntax and reap failed subprocesses. A real curl request to an isolated fake endpoint verifies JSON escaping. No live provider or real credential was used. |
| Medium | Forget consumed a request before a successful redaction, and shifted indices could make a stale window select another item. | Hold one write lock, preserve failed requests, report unreadable files and keep indices stable. Tests reproduce the old loss and verify stable decisions. The UI retains input and displays partial failures. |
| Low | The Rust HTTP Origin check accepted other loopback ports and schemes. | Require the request's exact HTTP authority. Regression requests cover mismatched port, scheme and hostname. Other browser defenses already reduced exploitability. |
| Reliability | Concurrent Ask requests rewrote the same schema file, intermittently failing on Windows with a mapped-file error. | Give each model request its own schema and remove it after the child exits. The concurrent Ask regression checks independent schemas and cleanup; the 77-question synthetic evaluation passes. |
| Privacy | Public-facing source metadata, examples and historical planning notes contained private identity and machine context. | Use the public identity in metadata, replace identifying fixtures, remove private planning and benchmark details, and add source/history checks and hooks. |

The storage fix is one shared path check, rather than separate string tests at each caller. It rejects descendant symlinks and Windows reparse points, including parents of paths not yet created. Cache invalidation handles canonical path aliases. The configured root may itself be a link.

## Remaining findings and limits

- **Local accounts are trusted.** Loopback HTTP has no caller authentication. A local process from another OS account can read and mutate the wiki through the service. Host, Origin and the fixed UI header defend against browsers, not local accounts. This is now explicit in SECURITY.md and the README. Authenticated local transport requires coordinated changes to the service, tray, UI and MCP clients.
- **Filesystem races and hard links are outside the guarantee.** The new checks stop static planted links and path traversal. They do not provide handle-based protection against a writer swapping files between validation and access, or an owner creating hard links. The wiki's operating-system permissions still matter.
- **Model trust is not identity verification.** Callers supply provenance labels. Automatic approval applies flagged changes; users who need review must select manual approval. Prompt and secret filters cannot prove the absence of injected instructions or unknown secret formats.
- **Retired code remains in source.** A complete deletion is a separate migration. The standard installer now refuses the Node option and cannot silently fall back to it. Comparison tests do not make the retired runtime a supported secure deployment.
- **Large modules remain.** Curator orchestration and UI state still warrant smaller domain-focused modules. Splitting them during a security repair would increase the behavior under review. Shared confinement and removal of the install fallback address concrete duplication and unsafe behavior now.

## Dependencies

The npm audit reported zero advisories. Cargo audit reported zero vulnerability-class advisories, plus these warnings:

- [RUSTSEC-2024-0429](https://rustsec.org/advisories/RUSTSEC-2024-0429.html): glib 0.18.5 has unsound VariantStrIter methods. It is a transitive native UI dependency. No direct use of those methods was found in this repository; that does not prove transitive code cannot reach them. Replacing it requires a compatible GTK/WebKit dependency migration.
- [RUSTSEC-2024-0370](https://rustsec.org/advisories/RUSTSEC-2024-0370.html): proc-macro-error 1.0.4 is unmaintained. It is a transitive build dependency.

These warnings remain open. An audit exit status alone must not be reported as a clean dependency bill of health.

## Verification

The full Rust check script passed formatting, clippy on Linux and Windows, clippy for macOS core/CLI, and 74 Linux unit tests. The macOS tray requires native macOS validation.

The legacy Node behavior run passed 101 tests, skipped seven and failed one Windows Edge-profile preparation test. The failed test also failed when run alone: Edge exited successfully without creating its temporary profile. No legacy installer code was changed to hide that result.

The Windows Rust release build succeeded. Its full behavior run passed 111 tests, skipped four and failed the same legacy Edge-profile test. All 77 synthetic Ask questions completed after the concurrency fix. A separate Ask regression verifies simultaneous requests use separate schema files. These runs used fake model responses; they do not establish live-provider answer quality.

An isolated browser check reproduced partial Forget failure with a locked file, confirmed that the error stays visible and the input is preserved, and verified the revised page rendering. Temporary services were stopped afterward.

The public source snapshot passed Gitleaks 8.30.1. Historical matches were synthetic secret-rejection values and a synthetic note deduplication key, not verified live credentials. Two exact test lines have documented scanner exceptions; the synthetic note key was made explicitly recognizable. No file-wide allowlist is used.

All exploit reproductions and regression runs used temporary synthetic wikis and spare ports. The installed service and private wiki were not used as test targets. Native release behavior, browser verification and publication checks are recorded with the final handoff.

The initial hosted CI attempt did not start because of an account billing restriction. Local results above are independent of that hosted run. The installed service was not upgraded during this audit.

## Publication rules

Only reviewed source and intentional fixtures belong in Git. Generated logs, private evaluation questions, artifacts and caches stay ignored. The public author and committer are checked independently. CI scans all available history for both identity and secret patterns.

Rewriting a branch does not erase old pull-request references or cached commits on GitHub. Use a fresh public repository object for a clean publication, or keep the existing repository private until GitHub removes its old references. See [GitHub's removal guidance](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/removing-sensitive-data-from-a-repository).

This audit records tested defects and limits. It is not a certification of security or anonymity, and cannot erase information already copied outside the repository.
