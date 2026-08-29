# pgokf

**pgokf** is a PostgreSQL extension that materializes [Open Knowledge Format](https://github.com/GoogleCloudPlatform/knowledge-catalog)
(OKF v0.2) bundles - directories of UTF-8 Markdown *concept* documents with YAML
frontmatter - into a transactional, queryable catalog inside PostgreSQL.

The bundle on disk stays the portable source of truth. PostgreSQL becomes a
projection of it, optimized for four things a directory of Markdown files cannot
do on its own:

- **Metadata queries** - filter and join across concept `type`, `tags`,
  `resource`, and arbitrary producer-defined frontmatter kept as `jsonb`.
- **Ranked search** - every concept body is indexed into a weighted `tsvector`;
  ranked full-text search runs entirely on stock PostgreSQL FTS with no
  third-party extension, with structured filters, keyset pagination, and faceted
  counts built in, and optional BM25, semantic (vector), and hybrid backends
  when the operator installs `pg_search` or `pgvector`.
- **Link-graph traversal** - internal Markdown links become directed edges you
  can walk to a bounded hop count.
- **Provenance, trust, and lifecycle** - the OKF v0.2 generation/trust/lifecycle
  families are projected into dedicated tables you can query and audit.

Optionally, pgokf can also store the **raw source bytes** of every concept file
inside PostgreSQL, turning the database into a small self-contained install that
needs no external file store.

- **Extension name and SQL schema:** `pgokf`
- **Supported PostgreSQL:** 15, 16, 17, 18, 19
- **Built with:** Rust (edition 2024) and [pgrx](https://github.com/pgcentralfoundation/pgrx) 0.19
- **Search backends:** native PostgreSQL FTS by default; an optional
  `search_backend=bm25` mode routes search through ParadeDB `pg_search`, and
  optional semantic and hybrid search use `pgvector`, each when the operator
  installs it (see the [search guide](search-guide.md#enabling-the-bm25-backend)
  and [semantic and hybrid search](search-guide.md#semantic-and-hybrid-search-optional-pgvector))
- **Ingestion:** filesystem (`register_bundle`) or mountless
  (`register_bundle_content`) from an object store via the `pgokf-ingest`
  companion, with no filesystem or network I/O inside the backend
- **Companions:** `pgokf-ingest` (mountless S3 ingestion, one-shot or
  `--watch`), `pgokf-embed` (embedding sidecar for semantic search),
  `pgokf-mcp` (an MCP server exposing the catalog to AI agents)
- **License:** AGPL-3.0-only, with a commercial license available (see
  [`LICENSING.md`](https://github.com/LogicOcean/pgokf/blob/main/LICENSING.md))

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
on disk - only files whose BLAKE3 content hash changed are re-parsed.

See [OKF authoring](okf-authoring.md) for how to structure a bundle pgokf will
ingest cleanly, and the [glossary](glossary.md) for precise definitions of every
OKF term.

---

## Key features

| Feature | What you get |
| ------- | ------------ |
| **Incremental sync** | `register_bundle` / `refresh_bundle` re-parse only changed files (BLAKE3 content hashing); deleted files are removed; unchanged rows keep their `indexed_at`. |
| **Mountless ingestion** | `register_bundle_content` projects concept bytes handed over the wire; the `pgokf-ingest` companion streams a bundle from S3-compatible object storage (one-shot or `--watch`), so the backend never mounts a bucket. |
| **Weighted full-text search** | `concept_search` ranks with `websearch_to_tsquery` + `ts_rank_cd` over a `tsvector` weighted title (A), tags/type/description (B), body (D), returning a `ts_headline` snippet, with structured filters (type / tags / status / trust tier), keyset pagination (`after_cursor`), faceted counts (`search_facets`), and `find_similar` content similarity. |
| **Pluggable search backends** | Native FTS by default; `search_backend=bm25` routes the same call through ParadeDB `pg_search`; `concept_search_semantic` and `concept_search_hybrid` add vector and RRF-fused search when `pgvector` is installed. Each optional backend degrades cleanly when its extension is absent. |
| **Link graph** | `concept_neighbors` walks resolved, non-external internal links to a bounded hop count and reports the shortest path taken. |
| **Provenance projection** | OKF `generated`, `verified[]`, `sources[]`, `status`, `stale_after`, and `usage_window` land in `concept_provenance`, `concept_verification`, and `concept_provenance_source`; Attested-Computation references become graph edges. |
| **Multi-tenancy (opt-in)** | Row-level security keyed on the `pgokf.tenant` session GUC isolates tenants' bundles; a session that sets no tenant sees everything, so existing installs are unaffected. See [Multi-tenancy](multi-tenancy.md). |
| **Version history (opt-in)** | `track_history` keeps an append-only version of every concept change; `concept_history` and `concept_as_of` answer point-in-time questions, pruned by `history_retention_days`. See [Version history](version-history.md). |
| **Lifecycle and audit** | A retire / unretire / `purge_retired` soft-delete window, an append-only sync log with per-file change manifests, an access log for exfiltration auditing, `LISTEN`/`NOTIFY` change notification, and `duplicate_concepts` dedup reporting. |
| **Two storage tiers** | Default: metadata + search in PostgreSQL, originals stay in a data lake / mounted bucket. Opt-in `store_source`: raw bytes live in PostgreSQL (`concept_source`) for a self-contained install. |
| **Get files back out** | `get_concept_source` returns one stored source file; `export_sources` writes a bundle's stored originals back to disk. |
| **Parquet export** | `export_parquet` snapshots a bundle's projection to Parquet, verified interoperable with DuckDB. |
| **Operability built in** | `catalog_stats`, `health`, `stale_concepts`, and `search_index_status` for monitoring; `schedule_refresh` / `unschedule_refresh` wire periodic refresh through `pg_cron` when it is installed. |
| **Least-privilege roles** | Three `NOLOGIN` tiers: `pgokf_reader` (search/read) < `pgokf_writer` (ingestion) < `pgokf_admin` (config, exports, rebuilds, audit); a fresh login role gets nothing until GRANTed one. |
| **Multi-version** | One codebase builds for PostgreSQL 15 through 19. |

---

## Quickstart

The bundle path is read by the **PostgreSQL server process**, so it must be an
absolute path the server can reach. Registration requires membership in
`pgokf_writer`; a fresh login role sees nothing in the `pgokf` schema (SQLSTATE
`42501`) until you GRANT it a role. A ready-to-use sample bundle ships in
[`examples/sample-bundle/`](https://github.com/LogicOcean/pgokf/blob/main/examples/sample-bundle).

```sql
CREATE EXTENSION pgokf;                                                         -- schema, tables, roles, functions
GRANT pgokf_writer TO app_user;                                                 -- give a login role the ingestion API
SELECT * FROM pgokf.register_bundle('/abs/path/to/examples/sample-bundle');     -- ingest (writer-level)
SELECT concept_id, title, type FROM pgokf.concept_search('postgres failover');  -- ranked full-text search
SELECT * FROM pgokf.concept_neighbors('runbooks/database-failover', 2);         -- walk the resolved link graph
```

For a step-by-step walkthrough - installing the extension, wiring roles, and
your first real bundle - start with [Getting started](getting-started.md).

---

## Documentation

### Guides

| Page | What it covers |
| ---- | -------------- |
| [Getting started](getting-started.md) | Install, grant a role, register your first bundle, run your first query. |
| [OKF authoring](okf-authoring.md) | How to structure a bundle: concepts, frontmatter, the reserved `index.md`, provenance families, the actor convention. |
| [Search guide](search-guide.md) | Writing effective `concept_search` queries, ranking and headlines, structured filters, keyset pagination, facets, and the optional BM25, semantic, and hybrid backends. |
| [Deployment topologies](deployment-topologies.md) | The two storage tiers, data-lake vs self-contained installs, bucket mounts vs mountless ingestion, and where the files live. |
| [Operations](operations.md) | Running pgokf in production: monitoring, audit logs, retirement, sync scheduling, exports, backup, and upgrades. |
| [Multi-tenancy](multi-tenancy.md) | Opt-in row-level tenant isolation keyed on the `pgokf.tenant` session GUC, and what it does and does not guarantee. |
| [Version history](version-history.md) | Opt-in append-only concept history and point-in-time queries (`concept_history`, `concept_as_of`). |

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
| [Packaging](packaging.md) | Building and distributing the extension across PostgreSQL 15–19. |
| [API stability](api-stability.md) | What the SQL surface guarantees and how it will evolve. |
| [Release checklist](release-checklist.md) | The gate a release must pass, including the in-database API-surface audit. |

---

## Where to go next

- New here? Read [Getting started](getting-started.md).
- Authoring a bundle? Read [OKF authoring](okf-authoring.md).
- Deciding how to deploy? Read [Deployment topologies](deployment-topologies.md).
- Need an exact signature? Read the [SQL API](sql-api.md).
- Hit an error? Check [Troubleshooting](troubleshooting.md) and the [FAQ](faq.md).
