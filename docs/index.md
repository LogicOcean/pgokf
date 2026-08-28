# pgokf

**pgokf** is a PostgreSQL extension that materializes [Open Knowledge Format](https://openknowledge.dev)
(OKF v0.2) bundles — directories of UTF-8 Markdown *concept* documents with YAML
frontmatter — into a transactional, queryable catalog inside PostgreSQL.

The bundle on disk stays the portable source of truth. PostgreSQL becomes a
projection of it, optimized for four things a directory of Markdown files cannot
do on its own:

- **Metadata queries** — filter and join across concept `type`, `tags`,
  `resource`, and arbitrary producer-defined frontmatter kept as `jsonb`.
- **Native full-text search** — every concept body is indexed into a weighted
  `tsvector`; ranked search runs entirely on stock PostgreSQL FTS, with no
  third-party extension required.
- **Link-graph traversal** — internal Markdown links become directed edges you
  can walk to a bounded hop count.
- **Provenance, trust, and lifecycle** — the OKF v0.2 generation/trust/lifecycle
  families are projected into dedicated tables you can query and audit.

Optionally, pgokf can also store the **raw source bytes** of every concept file
inside PostgreSQL, turning the database into a small self-contained install that
needs no external file store.

- **Extension name and SQL schema:** `pgokf`
- **Supported PostgreSQL:** 15, 16, 17, 18, 19
- **Built with:** Rust (edition 2024) and [pgrx](https://github.com/pgcentralfoundation/pgrx) 0.19
- **Search backend:** native PostgreSQL FTS only (a BM25 adapter is
  [researched but not shipped](bm25-research.md))

---

## The OKF connection

[OKF](https://github.com/GoogleCloudPlatform/knowledge-catalog) is a filesystem
convention for a **knowledge catalog**: a *bundle* is a directory tree of
Markdown *concept* documents, each carrying YAML frontmatter. In OKF v0.2 the
only required frontmatter field is `type`; `title`, `description`, `resource`,
and `tags` are recommended, and a set of provenance/trust/lifecycle families
(`sources`, `generated`, `verified`, `status`, `stale_after`, `usage_window`)
describe where a concept came from and how much to trust it.

pgokf reads a bundle root, parses each concept, and writes a faithful projection
into the `pgokf` schema. The bundle's reserved root `index.md` supplies the
`okf_version` recorded on the bundle row. Because the files remain canonical,
you can re-`refresh_bundle` at any time and the catalog converges to whatever is
on disk — only files whose BLAKE3 content hash changed are re-parsed.

See [OKF authoring](okf-authoring.md) for how to structure a bundle pgokf will
ingest cleanly, and the [glossary](glossary.md) for precise definitions of every
OKF term.

---

## Key features

| Feature | What you get |
| ------- | ------------ |
| **Incremental sync** | `register_bundle` / `refresh_bundle` re-parse only changed files (BLAKE3 content hashing); deleted files are removed; unchanged rows keep their `indexed_at`. |
| **Weighted full-text search** | `concept_search` ranks with `websearch_to_tsquery` + `ts_rank_cd` over a `tsvector` weighted title (A), tags/type/description (B), body (D), returning a `ts_headline` snippet. |
| **Link graph** | `concept_neighbors` walks resolved, non-external internal links to a bounded hop count and reports the shortest path taken. |
| **Provenance projection** | OKF `generated`, `verified[]`, `sources[]`, `status`, `stale_after`, and `usage_window` land in `concept_provenance`, `concept_verification`, and `concept_provenance_source`. |
| **Two storage tiers** | Default: metadata + search in PostgreSQL, originals stay in a data lake / mounted bucket. Opt-in `store_source`: raw bytes live in PostgreSQL (`concept_source`) for a self-contained install. |
| **Get files back out** | `get_concept_source` returns one stored source file; `export_sources` writes a bundle's stored originals back to disk. |
| **Parquet export** | `export_parquet` snapshots a bundle's projection to Parquet, verified interoperable with DuckDB. |
| **Least-privilege roles** | `pgokf_reader` (search/read) and `pgokf_admin` (register/refresh/config), both `NOLOGIN`; a fresh login role gets nothing until GRANTed one. |
| **Multi-version** | One codebase builds for PostgreSQL 15 through 19. |

---

## Quickstart

The bundle path is read by the **PostgreSQL server process**, so it must be an
absolute path the server can reach. Registration requires membership in
`pgokf_admin`; a fresh login role sees nothing in the `pgokf` schema (SQLSTATE
`42501`) until you GRANT it a role. A ready-to-use sample bundle ships in
[`examples/sample-bundle/`](https://github.com/LogicOcean/pgokf/blob/main/examples/sample-bundle).

```sql
CREATE EXTENSION pgokf;                                                         -- schema, tables, roles, functions
GRANT pgokf_admin TO app_user;                                                  -- give a login role the admin API
SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle');     -- ingest (admin-only)
SELECT concept_id, title, type FROM pgokf.concept_search('postgres failover');  -- ranked full-text search
SELECT * FROM pgokf.concept_neighbors('runbooks/database-failover', 2);         -- walk the resolved link graph
```

For a step-by-step walkthrough — installing the extension, wiring roles, and
your first real bundle — start with [Getting started](getting-started.md).

---

## Documentation

### Guides

| Page | What it covers |
| ---- | -------------- |
| [Getting started](getting-started.md) | Install, grant a role, register your first bundle, run your first query. |
| [OKF authoring](okf-authoring.md) | How to structure a bundle: concepts, frontmatter, the reserved `index.md`, provenance families, the actor convention. |
| [Search guide](search-guide.md) | Writing effective `concept_search` queries, ranking and headlines, tag/type/metadata filters, when native FTS is enough. |
| [Deployment topologies](deployment-topologies.md) | The two storage tiers, data-lake vs self-contained installs, bucket mounts, and where the files live. |
| [Operations](operations.md) | Running pgokf in production: sync scheduling, refresh, exports, monitoring, backup, and upgrades. |

### Reference

| Page | What it covers |
| ---- | -------------- |
| [SQL API](sql-api.md) | Every `pgokf.*` function, table, composite type, and GUC. |
| [Configuration](configuration.md) | GUCs and the durable `pgokf_private.config` policy keys. |
| [Security](security.md) | Roles, the `SECURITY DEFINER` model, path containment, least privilege. |
| [Troubleshooting](troubleshooting.md) | Common errors mapped to SQLSTATEs, with causes and fixes. |
| [FAQ](faq.md) | Grounded answers to the questions this project actually raises. |
| [Glossary](glossary.md) | Precise definitions of every OKF and pgokf term. |

### Deep dives

| Page | What it covers |
| ---- | -------------- |
| [Architecture](architecture.md) | Parser, sync engine, projection seams, and the search path. |
| [Benchmarks](benchmarks.md) | Measured recall and full-text scaling to tens of millions of concepts. |
| [BM25 research](bm25-research.md) | Notes on an optional *future* BM25 top-k adapter — not shipped. |
| [Packaging](packaging.md) | Building and distributing the extension across PostgreSQL 15–19. |
| [API stability](api-stability.md) | What the SQL surface guarantees and how it will evolve. |

---

## Where to go next

- New here? Read [Getting started](getting-started.md).
- Authoring a bundle? Read [OKF authoring](okf-authoring.md).
- Deciding how to deploy? Read [Deployment topologies](deployment-topologies.md).
- Need an exact signature? Read the [SQL API](sql-api.md).
- Hit an error? Check [Troubleshooting](troubleshooting.md) and the [FAQ](faq.md).
