# pgokf

A PostgreSQL extension that materializes [Open Knowledge Format](https://openknowledge.dev)
(OKF) bundles — directories of UTF-8 Markdown concept documents with YAML
frontmatter — into a transactional, queryable catalog. The bundle on disk stays
the portable source of truth; PostgreSQL becomes a projection optimized for
metadata queries, native full-text search, and link-graph traversal.

- **Extension name and SQL schema:** `pgokf`
- **Supported PostgreSQL:** 15, 16, 17, 18, 19
- **Built with:** Rust (edition 2024) and [pgrx](https://github.com/pgcentralfoundation/pgrx) 0.19
- **Search backend:** native PostgreSQL FTS only — no third-party extension is required

## Quickstart

The bundle path is read by the **PostgreSQL server process**, so it must be an
absolute path the server can reach, and registration requires membership in
`pgokf_admin`. A ready-to-use sample bundle ships in
[`examples/sample-bundle/`](examples/sample-bundle).

```sql
CREATE EXTENSION pgokf;                                                    -- install schema, tables, roles, functions
SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle'); -- ingest the bundle (admin-only)
SELECT concept_id, title, type FROM pgokf.concept_search('postgres failover'); -- ranked full-text search
SELECT * FROM pgokf.concept_neighbors('runbooks/database-failover', 2);    -- walk the resolved link graph
SELECT id, title, tags FROM pgokf.concepts ORDER BY id;                     -- browse the catalog projection
```

Running the quickstart against the sample bundle produces, in order:
`register_bundle` reports `added=4` (the reserved `index.md` is skipped);
`concept_search` ranks `runbooks/database-failover` first; `concept_neighbors`
returns the three concepts it links to; and `pgokf.concepts` lists all four
ingested concepts.

## Documentation

| Document | What it covers |
| -------- | -------------- |
| [docs/sql-api.md](docs/sql-api.md) | Complete reference for every `pgokf.*` function, table, type, and GUC |
| [docs/architecture.md](docs/architecture.md) | System design: parser, sync engine, projection seams, search |
| [docs/security.md](docs/security.md) | Roles, `SECURITY DEFINER` model, path containment, least privilege |
| [docs/configuration.md](docs/configuration.md) | GUCs and the durable `pgokf_private.config` policy keys |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Common errors mapped to SQLSTATEs, with causes and fixes |
| [docs/bm25-research.md](docs/bm25-research.md) | Research notes on an optional future BM25 adapter |

Runnable SQL lives in [`examples/queries/`](examples/queries)
(`quickstart.sql`, `search.sql`, `graph.sql`).

## What gets created

`CREATE EXTENSION pgokf;` installs, under the non-relocatable `pgokf` schema:

- **Tables** — `pgokf.bundles`, `pgokf.concepts`, `pgokf.concept_metadata`,
  `pgokf.links`, `pgokf.concept_provenance`, and the administrator-only
  `pgokf_private.config`.
- **Functions** — bundle lifecycle (`register_bundle`, `refresh_bundle`,
  `unregister_bundle`, `list_bundles`, `bundle_info`), search
  (`concept_search`), graph (`concept_neighbors`), configuration
  (`set_config`, `reset_config`, `get_config`), Parquet snapshot export
  (`export_parquet`, admin-only — the one function that writes files), and
  `version`.
- **Types** — `bundle_sync_result`, `bundle_info`, `concept_search_result`,
  `concept_neighbor`, and `export_result`.
- **Roles** — `pgokf_reader` (read/search) and `pgokf_admin` (register/refresh/
  configure; inherits `pgokf_reader`). Both are cluster-wide `NOLOGIN` roles you
  `GRANT` to real login users.

See [docs/sql-api.md](docs/sql-api.md) for exact signatures and
[docs/security.md](docs/security.md) for the authorization model.

## Building from source

The extension is developed and tested with `cargo-pgrx`:

```bash
cargo install cargo-pgrx --version 0.19.2 --locked
cargo pgrx init --pg18 $(which pg_config)        # or your target major version
cargo pgrx install --pg-config $(which pg_config) --features pg18
```

Select the target major version through the crate feature (`pg15`…`pg19`); the
default feature is `pg18`. Run the workspace test suites with
`cargo test` (parser and sync crates) and `cargo pgrx test pg18` (the
in-database tests).

## License

MIT. See the workspace manifest for details.
