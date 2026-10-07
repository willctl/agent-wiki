---
title: VPN setup
type: howto
summary: "Connect to the office network with WireGuard: profile from 1Password, wg-quick up office, DNS 10.10.0.53."
tags: ["vpn","wireguard","office"]
created: 2026-09-04T09:00:00-05:00
updated: 2026-09-04T09:00:00-05:00
updated_by: curator
---

# VPN setup

1. Get the `office.conf` WireGuard profile from the 1Password item "Office VPN".
2. Save it as `/etc/wireguard/office.conf` (Windows: import it into the WireGuard app).
3. Connect with `wg-quick up office`; the office DNS server is `10.10.0.53`.
4. Check the connection with `ping db-01.office.lan`.

Split tunnel: only 10.10.0.0/16 goes through the VPN.
