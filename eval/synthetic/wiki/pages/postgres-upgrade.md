---
title: Postgres 16 upgrade
type: decision
summary: "Upgrade db-01 from Postgres 15 to 16: decided 2026-09-20, done 2026-09-27 with pg_upgrade --link; rollback by snapshot."
tags: ["postgres","db-01","harbor","decision"]
created: 2026-09-20T14:00:00-05:00
updated: 2026-09-27T07:45:00-05:00
updated_by: curator
---

# Postgres 16 upgrade

Decided 2026-09-20 to upgrade `db-01` from Postgres 15 to 16 for [[harbor]].

- Why: Postgres 15 support on the managed image ends in November 2026, and 16 speeds up the sync's bulk upserts.
- How: `pg_upgrade --link` during the Sunday maintenance window, after a ZFS snapshot `db01@pre-pg16`.
- Rollback: restore the snapshot; expected downtime 10 minutes.
- Outcome: done 2026-09-27; downtime was 6 minutes.
