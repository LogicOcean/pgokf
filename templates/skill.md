---
# type: an OKF skill / competency concept - a documented capability.
type: skill
# title: required.
title: Operate the payments platform
# description: optional; indexed for search.
description: The capability to run, troubleshoot, and recover the payments platform.
# tags: the domains this competency spans.
tags:
  - payments
  - operations
  - oncall
# --- Producer EXTENSION frontmatter ----------------------------------------
# IMPORTANT: OKF does NOT define a "skill" type or any skill-specific fields.
# `type: skill` is just a producer-chosen type string (OKF consumers tolerate
# unknown types), and every key below (owner, proficiency_levels,
# required_for_role, review_cadence_days) is a PRODUCER EXTENSION - not an
# OKF-mandated field. Any key that is NOT a modeled field
# (type/title/description/tags/resource) and NOT an OKF v0.2 provenance/trust/
# lifecycle key becomes a row in pgokf.concept_metadata, stored as jsonb.
# This is how you attach domain-specific data.
owner: team-payments
proficiency_levels:            # <!-- a structured value survives as JSON metadata -->
  - novice
  - practitioner
  - expert
required_for_role: payments-oncall
review_cadence_days: 180
# --- OKF v0.2 provenance / trust / lifecycle (-> pgokf.concept_provenance) ---
# These ARE OKF-defined families (unlike the producer keys above).
status: stable
generated:
  by: enablement-agent/1.0
  at: 2026-06-01T00:00:00Z
verified:
  - by: human:enablement-lead
    at: 2026-06-02T00:00:00Z
stale_after: 2026-12-01T00:00:00Z
---

<!-- A competency documented as a knowledge artifact. Link it to the         -->
<!-- runbooks and services it applies to so it is reachable in the graph.     -->

# Operate the payments platform

Holders of this competency can keep the payments platform healthy and recover it
during incidents without escalation.

## What this covers

- Understanding the [payments architecture overview](wiki-article.md).
- Running the [restart the payments API runbook](runbook.md).
- Owning day-to-day operation of the [payments API service](service.md).

## How it is assessed

Candidates demonstrate a supervised recovery drill, then handle a live page.
See past [incident records](incident.md) for realistic scenarios.

## External learning resources

- [PostgreSQL administration docs](https://www.postgresql.org/docs/current/admin.html)
- Questions: [email the enablement team](mailto:enablement@example.test)
