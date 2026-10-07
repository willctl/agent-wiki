---
title: Home lab
type: reference
summary: "NAS (Synology DS923+ at 10.0.4.20, 4 x 8 TB), nightly backups at 02:00 to Backblaze B2, UPS APC SMT1500."
tags: ["home-lab","nas","backup"]
created: 2026-09-02T20:00:00-05:00
updated: 2026-09-22T08:00:00-05:00
updated_by: curator
---

# Home lab

- NAS: Synology DS923+ at `10.0.4.20`, 4 × 8 TB disks in SHR-1. The bay 3 disk was replaced on 2026-09-21 after SMART errors.
- Backups: Hyper Backup runs nightly at 02:00 to the Backblaze B2 bucket `sam-nas-backup`; retention 30 versions.
- UPS: APC SMT1500; the NAS shuts down at 20% battery.
- Router: UniFi Dream Machine at `10.0.4.1`; the lab VLAN is 40.
