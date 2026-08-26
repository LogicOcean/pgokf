---
id: rich-concept-producer-id
type: Attested Computation
title: Monthly active accounts
description: Canonical monthly active-account computation with complete OKF v0.2 metadata.
resource: https://catalog.example.test/metrics/monthly-active-accounts
tags: [analytics, accounts, monthly, attested]
status: stable
stale_after: 2027-01-01T00:00:00Z
generated:
  by: catalog-agent/1.0
  at: 2026-07-01T12:00:00Z
verified:
  - by: process:metric-validation
    at: 2026-07-02T02:00:00Z
  - by: human:fixture-reviewer
    at: 2026-07-03T09:30:00Z
usage_window:
  from: 2026-06-01T00:00:00Z
  to: 2026-06-30T23:59:59Z
sources:
  - id: account-policy
    resource: https://docs.example.test/policies/active-account
    title: Active account policy
    author: human:data-governance
    usage_count: 4200
    last_modified: 2026-06-15T08:00:00Z
  - id: events-table
    resource: /source-events.md
    title: Account events source
    author: process:warehouse-catalog
    usage_count: 18000
    last_modified: 2026-06-30T23:00:00Z
    usage_window:
      from: 2026-06-24T00:00:00Z
      to: 2026-06-30T23:59:59Z
runtime: postgres
parameters:
  - name: month_start
    type: date
    required: true
computation: /computation.md
executor:
  resource: /executor.md
  receipt: [query_id, executed_sql, result]
attester:
  resource: /attester.md
producer_extension:
  preserve_me: true
  quality_band: gold
---

# Definition

An account is active when it has at least one qualifying event during the calendar month.[^account-policy]

The sanctioned SQL is in [the computation](/computation.md), operates on [source events](/source-events.md), and is checked by [the attester](/attester.md).

[^account-policy]: Active account policy
[^events-table]: Account events source
