---
type: Dashboard
title: Service health
description: Live health signals for the core database service.
tags:
  - postgres
  - observability
status: stable
resource:
  url: https://grafana.example.test/d/db-health
  kind: grafana-dashboard
---

# Service health

Live latency, replication lag, and connection-pool saturation for the
[PostgreSQL service](../services/postgresql.md).

External view: [open in Grafana](https://grafana.example.test/d/db-health).
