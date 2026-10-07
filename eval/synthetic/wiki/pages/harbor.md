---
title: Harbor
type: project
summary: "Inventory sync service: Postgres 16 on db-01, staging and production on port 8443, repo moved to C:/Projects/harbor."
tags: ["harbor","inventory","postgres","service"]
created: 2026-09-01T09:12:00-05:00
updated: 2026-10-02T17:10:00-05:00
updated_by: curator
---

# Harbor

Harbor syncs warehouse inventory from the Ledgerline ERP into the storefront every five minutes. Sam owns it; [[dana-okafor]] owns the billing API it calls. Related: [[postgres-upgrade]].

## Where things live

- Repo: `C:/Projects/harbor` (moved 2026-09-14; previously `C:/Dev/harbor`), GitHub `samriv/harbor`, branch `main`.
- Staging: https://harbor-staging.example.test (port 8443). Production: https://harbor.example.test (port 8443).
- Database: Postgres on `db-01` (upgraded from 15 to 16 on 2026-09-27; see [[postgres-upgrade]]).
- Logs: `/var/log/harbor/sync.log` on app-02.
- ERP: [[ledgerline-erp]] API v3 (since 2026-10-02; previously v2).

## How it runs

- A systemd timer `harbor-sync.timer` runs the sync every 5 minutes.
- Config: `/etc/harbor/harbor.toml`; secrets come from the 1Password vault "Harbor" (never in the repo).
- CI: the GitHub Actions workflow `ci.yml` runs lint, tests and a staging deploy on merge to `main`.
- Calls to Ledgerline back off on 429 (Retry-After, at most 5 retries) and stay under 100 a minute, since INC-0042 on 2026-09-29.

## Decisions

- 2026-09-02: chose Postgres over SQLite because two sync workers write at once.
- 2026-09-10: batch size set to 500 rows after timeouts at 2,000.

## Open items

- Add a dead-letter table for rows the ERP rejects.
