---
title: Lighthouse
type: project
summary: "Public status dashboard in SvelteKit at https://status.example.test on Fly.io (since 2026-10-01); builds on Node 24; SSO for the admin page still to do."
tags: ["lighthouse","status","sveltekit","dashboard"]
created: 2026-09-05T13:00:00-05:00
updated: 2026-10-01T16:00:00-05:00
updated_by: curator
---

# Lighthouse

Lighthouse is the public status dashboard for [[harbor]] and the storefront, built with SvelteKit.

## Where things live

- Repo: `C:/Projects/lighthouse`, GitHub `samriv/lighthouse`.
- Production: https://status.example.test on Fly.io (moved 2026-10-01; previously Cloudflare Pages).
- Dev server: `npm run dev` on port 4173.

## Build

- Node 24 (switched from Node 22 on 2026-09-19 after the Vite 7 upgrade).
- Checks come from Harbor's `/healthz` and the storefront's `/status` endpoint every 60 seconds.

## Open items

- Add SSO for the admin page (Authentik or Cloudflare Access; not decided).
- Dark mode for the incident timeline.
