---
type: Runbook
title: Widget failover
description: Promote the standby when the primary widget node is lost.
tags:
  - widget
  - oncall
---

# Widget failover

Run this when the primary [widget service](/services/widget.md) is
unreachable and streaming replication was healthy before the outage.

1. Confirm the primary is truly down.
2. Promote the standby.
3. Repoint the application connection string.
4. Capture a post-incident record in the [failover appendix](failover-appendix.md).
