# OKF PostgreSQL Catalog

First PostgreSQL extension for Open Knowledge Format (OKF) metadata catalog.

## Quick Start

```sql
CREATE EXTENSION okf_catalog;
SELECT * FROM okf.register_bundle(/data/my-knowledge-bundle);
SELECT id, title, type, tags FROM okf.concepts WHERE type = Runbook AND tags @> ARRAY[postgres];
SELECT * FROM okf.concept_search(replication failover);
```

## Status

Under development. See [docs/IMPLEMENTATION-PLAN.md](docs/IMPLEMENTATION-PLAN.md) for the full plan.
