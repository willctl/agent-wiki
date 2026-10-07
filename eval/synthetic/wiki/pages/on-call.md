---
title: On-call
type: howto
summary: "Payments on-call: PagerDuty schedule \"Payments primary\", handover Mondays 10:00 US/Central, escalate after 15 minutes to Dana, then Sam."
tags: ["on-call","pagerduty","payments","incident"]
created: 2026-09-22T10:10:00-05:00
updated: 2026-09-29T12:20:00-05:00
updated_by: curator
---

# On-call

The payments team covers [[harbor]], [[quayside]] checkout and billing.

## Rotation

- PagerDuty schedule "Payments primary", weekly; handover Mondays at 10:00 US/Central.
- Order: Sam, [[priya-shah]], [[dana-okafor]].

## When paged

1. Acknowledge in PagerDuty within 5 minutes.
2. Open an incident channel in Slack named `#inc-<number>`.
3. Unacknowledged pages escalate after 15 minutes to Dana, then to Sam.
4. Write the timeline in the incident doc and file a short note in the wiki afterwards.

## Past incidents

- INC-0042 (2026-09-29): Harbor sync stalled for 40 minutes; see the log for that day.
