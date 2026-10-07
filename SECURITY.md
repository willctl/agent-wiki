# Security

Agent Wiki is for a trusted local machine. The HTTP service binds to loopback and rejects unrelated browser origins. It does not authenticate OS users. Any local process, including another account on the machine, can call its read and write APIs. The UI header prevents browser cross-site requests; it is not a password. Do not forward the port or use this service to isolate mutually untrusted users.

## Data and model access

The wiki, history, inbox, request records and vector cache are local files. Protect their permissions and backups. The secret detector is a best-effort guard, not a guarantee that every credential or personal detail will be recognized.

The curator and Ask send selected content to the configured model through its CLI. Optional embeddings send wiki sections and queries to OpenRouter. Enable external processing only when that is appropriate for the data. Provider retention settings are requests to the provider, not a local confidentiality boundary.

Notes and saved change records are untrusted input. Paths from those records must remain inside the wiki. Links and Windows reparse points below the configured wiki root are refused by the audited storage paths. The configured root itself may be a link. These checks do not defend against a process with write access racing filesystem changes, or against hard links created by the owner.

Automatic approval applies flagged curator changes. Choose manual approval when changes should wait for review. Source labels come from the caller and are not authenticated proof of authorship.

## Development and publication

Use synthetic data and an isolated server for verification. Never test against an installed service or a private wiki. Keep generated artifacts, machine reports, credentials and local evaluation questions out of Git.

The public Git identity is `willctl <will@willctl.com>`. Configure it locally and enable the hooks with `git config core.hooksPath .githooks`. Run `node scripts/check-public.mjs --history` before publishing. Local private blocked terms may be stored in `.git/privacy-blocked.json`; never commit that file. The checks reduce accidental disclosure but do not prove anonymity.

See [the audit](docs/security-audit.md) for verified fixes, dependency warnings and remaining work. Report a suspected vulnerability through GitHub private vulnerability reporting when available; do not put secrets in a public issue.
