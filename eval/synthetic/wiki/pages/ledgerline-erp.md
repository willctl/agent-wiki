---
title: Ledgerline ERP
type: reference
summary: "The ERP Harbor syncs from: API v3 at https://api.ledgerline.example.test/v3, OAuth client credentials, 120 requests per minute; vendor contact Marta Ruiz."
tags: ["ledgerline","erp","harbor","vendor"]
created: 2026-09-08T10:30:00-05:00
updated: 2026-10-02T17:10:00-05:00
updated_by: curator
---

# Ledgerline ERP

Ledgerline is the warehouse ERP that [[harbor]] reads inventory from.

## API

- Base URL: https://api.ledgerline.example.test/v3 (Harbor moved from v2 to v3 on 2026-10-02; v2 is retired on 2026-12-31).
- Sandbox: https://sandbox.ledgerline.example.test/v3.
- Auth: OAuth 2 client credentials. Production credentials are in the 1Password item "Ledgerline API"; sandbox ones in "Ledgerline sandbox" (rotated 2026-09-26).
- Rate limit: 120 requests per minute per client; over it the API answers 429 with a Retry-After header.

## People

- Vendor contact: Marta Ruiz (Ledgerline customer success), marta.ruiz@ledgerline.example.test, US/Eastern.
- Support portal: https://support.ledgerline.example.test (account "samriv-harbor").
