---
# type: the OKF concept type. Free-form text; kept verbatim on pgokf.concepts.type.
type: runbook
# title: required. Shown in search results and used for ranking.
title: Restart the payments API
# description: optional one-liner. Indexed for full-text search.
description: Safely restart the payments API service after a failed deploy.
# tags: optional list. Each tag becomes a row you can filter/aggregate on.
tags:
  - oncall
  - payments
  - deploy
# --- OKF v0.2 provenance / trust / lifecycle frontmatter -------------------
# Projected into pgokf.concept_provenance (+ one pgokf.concept_verification
# row per verified[] event). Actors follow the OKF convention:
#   agent  <producer>/<version>    human  human:<id>    process  process:<id>
# status: lifecycle state -> draft | stable | deprecated (spec default: stable).
status: stable
# generated: how the CURRENT content was produced (by = actor, at = ISO 8601).
generated:
  by: sre-agent/1.4
  at: 2026-08-01T09:00:00Z
# verified: an ORDERED LIST of verification events. A human actor makes the
# derived trust_tier `human-reviewed`; a non-human-only list is
# `machine-confirmed`. Omit the whole block for an unverified draft.
verified:
  - by: human:sre-lead
    at: 2026-08-02T14:30:00Z
# stale_after: absolute ISO 8601 instant after which the content is stale.
stale_after: 2027-02-01T00:00:00Z
---

<!-- Body is rendered to plain text and indexed for concept_search.        -->
<!-- Internal links [label](sibling.md) become graph edges to other concepts. -->

# Restart the payments API

Operational runbook for the [payments API service](service.md). Read the
[payments architecture overview](wiki-article.md) first if you are new to the
system.

## Preconditions

<!-- State what must be true BEFORE running the steps. -->
- You hold the `oncall` role and can reach the deploy host.
- The most recent deploy is confirmed failed (not merely slow).
- A maintenance window is open or the incident is already declared.

## Steps

1. Announce the restart in the incident channel.
2. Drain traffic from the unhealthy instances.
3. Roll the service back to the last known-good revision.
4. Restart the [payments API service](service.md) instances one at a time.
5. Confirm health checks pass, then restore traffic.

## Rollback / escalation

If health checks still fail after a restart, escalate to the service owner and
open an incident using the [incident template](incident.md).

## References

<!-- Mix internal (graph edges) and external (flagged is_external) links.   -->
- Related runbook: [payments architecture overview](wiki-article.md)
- Upstream docs: [PostgreSQL high availability](https://www.postgresql.org/docs/current/high-availability.html)
- Escalation: [page the on-call lead](mailto:oncall@example.test)
