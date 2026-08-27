---
type: Runbook
title: Database failover
description: Promote the standby when the primary PostgreSQL node is lost.
tags:
  - postgres
  - oncall
  - incident-response
status: stable
generated:
  by: sre-agent/2.1
  at: 2026-07-01T12:00:00Z
verified:
  - by: process:failover-drill
  - by: human:oncall-lead
verification_method: quarterly-drill
sources:
  - id: postgres-ha-guide
    url: https://www.postgresql.org/docs/current/high-availability.html
---

# Database failover

Run this when the primary [PostgreSQL service](/services/postgresql.md) is
unreachable and streaming replication was healthy before the outage.

1. Confirm the primary is truly down (see the [service health dashboard](/dashboards/health.md)).
2. Promote the standby.
3. Repoint the application connection string.
4. Capture a post-incident record in the [failover appendix](appendix.md).

Escalation contact: [page the on-call lead](mailto:oncall@example.test).

> Note: the [capacity-planning notes](capacity-planning.md) referenced below do
> not exist yet, so `pgokf` records this as an unresolved internal link.
