---
# type: an incident / postmortem record.
type: incident
# title: required. Convention: date + short summary.
title: 2026-08-14 payments API outage
# description: optional; indexed for search.
description: Failed deploy took the payments API offline for 27 minutes.
# tags: severity and affected area help you slice the incident history.
tags:
  - payments
  - postmortem
  - sev2
# --- OKF v0.2 provenance / trust / lifecycle (-> pgokf.concept_provenance) ---
# A postmortem is authored by a human and reviewed by a human, so the derived
# trust_tier is `human-reviewed`. A historical record needs no stale_after.
status: stable
generated:
  by: human:incident-commander
  at: 2026-08-14T15:00:00Z
verified:
  - by: human:reliability-reviewer
    at: 2026-08-15T10:00:00Z
---

<!-- A postmortem record. Link to the runbook(s) used and the service(s)     -->
<!-- affected so the incident is reachable from them in the link graph.       -->

# 2026-08-14 payments API outage

## Impact

For 27 minutes the [payments API service](service.md) returned 5xx errors and
no charges were accepted. Estimated 1,200 failed requests.

## Timeline

<!-- Keep timestamps unambiguous (UTC). -->
- **14:02 UTC** - Deploy of revision `a1b2c3` rolled out.
- **14:05 UTC** - Error rate crossed alert threshold; on-call paged.
- **14:11 UTC** - Incident declared (sev2).
- **14:18 UTC** - Ran the [restart the payments API runbook](runbook.md).
- **14:29 UTC** - Rolled back to last known-good; health checks recovered.
- **14:32 UTC** - Traffic fully restored; incident resolved.

## Root cause

The deploy shipped an incompatible config schema that crashed the API on start.

## Resolution

Followed the [restart the payments API runbook](runbook.md) to roll back the
[payments API service](service.md) to the previous revision.

## Follow-ups

- Add config-schema validation to the deploy pipeline.
- Review the [payments architecture overview](wiki-article.md) startup section.

## References

- Status page: [public status update](https://status.example.test/incidents/2026-08-14)
