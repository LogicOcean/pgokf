# pgokf

`pgokf` is a PostgreSQL extension for materializing Open Knowledge Format (OKF) Markdown bundles as a queryable catalog.

> **Status:** Phase 1 foundation. The parser and synchronization planner are implemented; the PostgreSQL registration and query API is still under development.

## Planned quick start

```sql
CREATE EXTENSION pgokf;
SELECT * FROM pgokf.register_bundle('/data/my-knowledge-bundle');
SELECT id, title, type, tags
FROM pgokf.concepts
WHERE type = 'Runbook' AND tags @> ARRAY['postgres']::text[];
SELECT * FROM pgokf.concept_search('replication failover');
```

The PostgreSQL server process—not the `psql` client—must be able to read a registered bundle directory.

## Workspace

- `crates/okf-parser`: YAML frontmatter and Markdown parsing
- `crates/okf-sync`: file discovery, BLAKE3 hashing, and incremental sync planning
- `crates/extension`: `pgokf` pgrx extension skeleton

Run the pure-Rust test suite with:

```sh
cargo test --locked
```
