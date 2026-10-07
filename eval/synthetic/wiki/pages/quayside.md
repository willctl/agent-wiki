---
title: Quayside storefront
type: project
summary: "The customer storefront: React Router 7 app at https://shop.example.test on Fly.io (region ord, moved from Heroku 2026-09-24), port 3000 locally."
tags: ["quayside","storefront","fly.io","react-router"]
created: 2026-09-06T11:00:00-05:00
updated: 2026-09-24T18:30:00-05:00
updated_by: curator
---

# Quayside storefront

Quayside is the customer-facing storefront. [[harbor]] feeds it inventory; [[lighthouse]] watches its `/status` endpoint.

## Where things live

- Repo: `C:/Projects/quayside`, GitHub `samriv/quayside`.
- Production: https://shop.example.test on Fly.io, region `ord` (moved from Heroku on 2026-09-24 to cut cost and cold starts).
- Local dev: `npm run dev` serves on port 3000.

## Stack

- React Router 7 in framework mode (migrated from Remix 2; the migration started 2026-09-24).
- Payments through Stripe; webhooks land on `/api/stripe/webhook`.
- Images on Cloudflare R2, bucket `quayside-images`.

## Open items

- Finish the React Router 7 migration of the checkout routes.
