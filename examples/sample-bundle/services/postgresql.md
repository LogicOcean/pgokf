---
type: Reference
title: PostgreSQL service
description: The primary transactional PostgreSQL database service.
tags:
  - postgres
  - database
  - service
status: stable
generated_by: platform-team
---

# PostgreSQL service

The primary transactional database. Streaming replication keeps a hot standby
ready for failover. Point-in-time recovery is available through archived WAL.

Operational entry points:

- Failover procedure: [database failover runbook](/runbooks/database-failover.md)
- Health dashboard: [service health](/dashboards/health.md)
- Upstream documentation: [PostgreSQL high availability](https://www.postgresql.org/docs/current/high-availability.html)
