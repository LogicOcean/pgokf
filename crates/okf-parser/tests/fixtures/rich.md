---
id: incident-db
type: Runbook
title: Database failover
description: Recover the primary safely
tags: [postgres, incident]
resource:
  url: https://example.test/runbooks/db
owner: sre
severity: high
---
# Database failover

Follow the **replication** checklist.

See [replica health](replica.md) and <https://status.example.test>.

![topology](images/topology.png)
