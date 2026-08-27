# Example OKF bundle

This is a reserved OKF bundle file (`index.md`). It describes the bundle as a
whole and is intentionally **not** ingested as a concept: `pgokf` skips
`index.md` and `log.md` at every directory level.

The concepts in this bundle model a tiny operations knowledge base:

- `services/postgresql` — the PostgreSQL service reference
- `runbooks/database-failover` — a failover runbook (carries provenance)
- `runbooks/appendix` — supporting reference material
- `dashboards/health` — a service-health dashboard

Register it with `SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle');`
