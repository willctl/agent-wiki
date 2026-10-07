---
title: SSH keys
type: reference
summary: "SSH keys live in the 1Password SSH agent: ed25519 sam@laptop-2026 for GitHub and commit signing; db-01 only accepts the bastion key."
tags: ["ssh","keys","security"]
created: 2026-09-11T16:00:00-05:00
updated: 2026-09-11T16:00:00-05:00
updated_by: curator
---

# SSH keys

Private keys never leave the 1Password SSH agent; nothing is stored on disk or in notes.

- `sam@laptop-2026` (ed25519): GitHub access and commit signing.
- `bastion-2026` (ed25519): the only key `db-01` accepts, and only through `bastion.office.lan`.
- Old RSA key `sam@old-laptop` was removed from GitHub on 2026-09-11.
