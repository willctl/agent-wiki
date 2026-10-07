---
title: Backup restore drill
type: decision
summary: "Quarterly restore test of the NAS backups: last on 2026-09-30 (50 GB from B2 in 3 h 12 min, photos task found missing); next on 2026-12-15."
tags: ["backup","home-lab","nas","restore"]
created: 2026-09-30T20:30:00-05:00
updated: 2026-09-30T20:30:00-05:00
updated_by: curator
---

# Backup restore drill

Decided to restore a sample of the [[home-lab]] NAS backups every quarter, because a backup that has never been restored is a guess.

- Last drill: 2026-09-30. Restored 50 GB from the B2 bucket `sam-nas-backup` to a scratch share in 3 hours 12 minutes; all checksums matched.
- Finding: the Hyper Backup task "photos" had been excluded since the July DSM update; re-enabled it the same day.
- Next drill: 2026-12-15.
