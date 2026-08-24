# pgokf

PostgreSQL extension for Open Knowledge Format (OKF) metadata catalogs.

## Quick Start

```sql
CREATE EXTENSION pgokf;
SELECT * FROM pgokf.register_bundle('/data/my-knowledge-bundle');
SELECT id, title, type, tags FROM pgokf.concepts WHERE type = 'Runbook' AND tags @> ARRAY['postgres'];
SELECT * FROM pgokf.concept_search('replication failover');
```

## Status

Under development. See [docs/IMPLEMENTATION-PLAN.md](docs/IMPLEMENTATION-PLAN.md) for the full plan.
