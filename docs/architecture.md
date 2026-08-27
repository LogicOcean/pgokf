# pgokf architecture

## Purpose and scope

`pgokf` is a PostgreSQL extension that imports Open Knowledge Format (OKF)
bundles — UTF-8 Markdown concept documents with YAML frontmatter — into a
queryable catalog. The extension name and SQL schema are both `pgokf`; Debian
packages follow the PostgreSQL convention, for example `postgresql-18-pgokf`.

The catalog provides ingestion, transactional synchronization, metadata queries,
native full-text search, an OKF v0.2 link graph with cycle-safe traversal, a
provenance/trust/lifecycle projection, and an admin-only Apache Parquet snapshot
export. It does not replace the bundle on disk:
Markdown remains the portable source of truth, while PostgreSQL is a transactional
projection optimized for discovery.

The target matrix is **PostgreSQL 15, 16, 17, 18, and 19**. The extension is built
in Rust (**edition 2024**, workspace `rust-version` 1.96) with
[**pgrx 0.19**](https://github.com/pgcentralfoundation/pgrx); the workspace forbids
`unsafe_code` and treats Clippy `all` + `pedantic` as warnings that CI escalates to
errors.

## Workspace layout

The project is a three-crate Cargo workspace that cleanly separates
PostgreSQL-independent logic from the database-facing shell:

| Crate | Role |
| ----- | ---- |
| `crates/okf-parser` | PostgreSQL-independent parser: normalizes concept paths, splits/validates YAML frontmatter, renders the body to plain text, and extracts Markdown links. Produces a database-neutral `ParsedConcept`. |
| `crates/okf-sync` | PostgreSQL-independent filesystem layer: bounded, symlink-escape-safe directory discovery, BLAKE3 content hashing, and the incremental sync report. |
| `crates/extension` | The pgrx extension (package `pgokf`): the SQL surface, base tables, the shared register/refresh engine, search, graph, provenance, admin, configuration, roles, GUCs, and error mapping. |

Keeping the parser and sync engine free of any pgrx dependency makes them unit
testable without a running backend and keeps the trust boundary — where
untrusted bundle content meets the database — small and explicit.

## System context

```text
OKF bundle directory
  (*.md + YAML frontmatter)
          |
          v
  path validation / allowed-roots containment   (crates/extension/src/security.rs)
          |
          v
  bounded, symlink-safe discovery + BLAKE3 hash  (okf-sync)
          |
          v
  Markdown + YAML parser ----------------------> per-file diagnostics
          |                                       (okf-parser)
          v
  normalized concept records (ParsedConcept)
          |
          +----> pgokf.bundles
          +----> pgokf.concepts ------> weighted tsvector / GIN
          +----> pgokf.concept_metadata
          +----> pgokf.links ---------> recursive graph queries (concept_neighbors)
          +----> pgokf.concept_provenance
          |
          v
  transactional diff / upsert / delete           (crates/extension/src/catalog/sync.rs)
          |
          v
SQL API under schema pgokf:
  register_bundle / refresh_bundle / unregister_bundle / list_bundles / bundle_info
  concept_search / concept_neighbors
  set_config / reset_config / get_config / version
  export_parquet  (admin-only Parquet snapshot; the one function that writes files)
```

## Components

### Extension boundary

The Rust/pgrx extension exposes SQL objects under the non-relocatable `pgokf`
schema. `CREATE EXTENSION pgokf;` installs the schema, base tables, composite
result types, indexes, functions, the two roles, and the durable-configuration
table. A `bootstrap` SQL block creates the `pgokf` and `pgokf_private` schemas
and the `pgokf_reader` / `pgokf_admin` roles, and hardens schema access, before
the feature SQL blocks run. Public entry points are schema-qualified everywhere
in documentation and examples. See [sql-api.md](sql-api.md) for exact
signatures.

### Bundle reader and security boundary

`pgokf.register_bundle(path, name, options)` treats the server-side filesystem as
privileged input. The reader validates the requested root (absolute, no `..`, no
NUL), canonicalizes it, enforces configured allowed roots when present (with both
sides canonicalized so symlinks cannot escape containment), applies the file /
count / byte GUC limits, rejects symlink escapes during discovery, and reads only
accepted Markdown files. Registration and refresh are restricted to
`pgokf_admin`; read and search are granted separately to `pgokf_reader`.

The database never executes content from a bundle. Markdown, YAML scalar values,
links, and referenced resources are data only, and every value reaches SQL as a
bound parameter. The full authorization and containment model is in
[security.md](security.md).

### Parser and normalization

For each non-reserved `.md` file, the parser (`okf-parser`):

1. derives the OKF concept ID from the normalized bundle-relative path without
   `.md` — a producer-declared frontmatter `id` is preserved for diagnostics but
   never trusted as the catalog key;
2. decodes UTF-8 and splits the `---`-delimited YAML frontmatter from the body;
3. requires `type` and `title`, and preserves `description`, `resource`, `tags`,
   and every unknown frontmatter key as JSON metadata;
4. extracts Markdown links in document order, classifying each and normalizing
   internal destinations relative to the source document or bundle root;
5. renders the body to compact plain text for indexing.

`index.md` and `log.md` are reserved OKF files at every directory level and are
never ingested as concepts. Unknown frontmatter keys survive round-tripping so
future OKF versions and producer extensions do not lose data.

### Catalog projection

The physical model (full column detail in [sql-api.md](sql-api.md)):

- **`pgokf.bundles`** — one registered root: identity, canonical path, sync state,
  timestamps, the aggregate `sync_hash` digest, producer `options`, and an
  `enabled` flag.
- **`pgokf.concepts`** — one row per `(bundle_id, id)`: path, type, title,
  description, resource, tags, plain-text body, BLAKE3 `file_hash`, timestamps,
  and the weighted `body_tsv` search vector.
- **`pgokf.concept_metadata`** — one row per unrecognized frontmatter key,
  retained as `jsonb`.
- **`pgokf.links`** — directed Markdown edges extracted per concept: source, raw
  and normalized target, label, kind, and the `resolved` / `is_external` flags.
- **`pgokf.concept_provenance`** — a sparse projection of OKF v0.2 provenance /
  trust / lifecycle frontmatter: typed columns (`generated_by`, `verified`,
  `verification_method`, `freshness`) plus a lossless `details` `jsonb`.

`(bundle_id, id)` is the concept key, so concepts with the same path in different
bundles stay distinct. The composite result types (`bundle_sync_result`,
`concept_search_result`, `bundle_info`, `concept_neighbor`, `export_result`) are
the stable shapes returned to callers.

### Transactional synchronization

`register_bundle` and `refresh_bundle` share one engine
(`catalog/sync.rs::run_bundle_sync`). It scans and parses before mutating, then
applies a set-based diff in a single transaction:

1. serialize on a bundle-scoped `pg_advisory_xact_lock` keyed on the canonical
   path;
2. load the stored `path -> file_hash` projection for the bundle;
3. discover the current filesystem state (symlink-escape safe, GUC-bounded);
4. classify each file against the stored hashes so unchanged rows are never
   rewritten and their `indexed_at` is preserved;
5. parse only added/changed files — strict policy: the first malformed file
   aborts the sync (`22023`) and the transaction rolls back, so a partial
   projection is never committed;
6. delete rows for removed files, upsert changed rows (recomputing the weighted
   `body_tsv`), and replace `concept_metadata` for touched concepts;
7. run the ordered projection seam — links, then provenance — over the staged
   concepts;
8. update the bundle row (file count, `last_synced_at`, aggregate `sync_hash`)
   last.

Feature tables (`links`, `concept_provenance`, `concept_metadata`) cascade from
`pgokf.concepts` via foreign keys, so removals need no seam call. Concurrent
syncs of one bundle serialize on the advisory lock; distinct bundles proceed in
parallel.

#### Extension seams

The backbone is open for extension and closed for modification. Core modules own
the schema and the sync loop; feature modules attach through fixed seams and
never edit the core. `catalog/sync.rs` calls `links::project` then
`provenance::project` after staging concept rows; `catalog/schema.rs` owns the
base tables under the named `catalog_tables` SQL block, and each feature orders
its SQL after it with `requires = ["catalog_tables"]`. This is how links,
neighbors, provenance, admin, and config were each added without touching the
sync engine.

### Search

The backend is PostgreSQL native FTS, so every supported PostgreSQL 15–19
installation works without another extension. A weighted document favors title
(A), then tags/type/description (B), then body (D), with a GIN index on `body_tsv`
for matching. `pgokf.concept_search(query, bundle_id, limit_count)` matches with
`websearch_to_tsquery`, ranks with `ts_rank_cd`, attaches a `ts_headline`
snippet, and searches only enabled bundles. Results add the concept ID as a
deterministic tiebreaker, so equal-rank hits order stably. Ranks are comparable
only within one query; callers order by them rather than persisting them.

An optional future adapter could use ParadeDB `pg_search`/BM25 while returning
the same logical result shape; ParadeDB must never be a transitive requirement of
`CREATE EXTENSION pgokf`. See [BM25 research](bm25-research.md).

### Link graph (OKF v0.2)

Internal Markdown destinations are normalized relative to the source document or
bundle root; fragment identifiers do not change the target concept ID. External
URLs and email links are retained but never become internal edges. An internal
link is marked `resolved` only when its target concept exists in the same bundle
at sync time; broken internal links are retained as unresolved because OKF
permits them and a later sync may resolve them.

`pgokf.concept_neighbors(concept_id, max_hops, bundle_id)` walks the graph with a
cycle-safe recursive CTE over `pgokf.links`. It follows only resolved,
non-external edges, is bundle-scoped, depth-limited (`max_hops >= 1`, capped at
`pgokf.max_graph_hops`), and authorization-filtered at reader level. It returns
each reachable concept once with its shortest hop count and path.
[`examples/queries/graph.sql`](../examples/queries/graph.sql) shows both direct
edge queries and the built-in traversal alongside an equivalent hand-written CTE.

### Parquet export

`pgokf.export_parquet(bundle_id, dest_dir)` (`catalog/export.rs`) is a
self-contained feature module attached at the same extension seam as the others:
it reads the catalog projection and never touches the sync engine or the base
schema. It writes one Apache Parquet file per table for the requested bundle —
`concepts`, `concept_metadata`, `links`, `concept_provenance` — into a validated
server-side directory and returns an `export_result` with the per-file row counts
and total bytes written. It is the **only** function in the extension that writes
files, so it is admin-only and validates `dest_dir` exactly as strictly as a
bundle input root (absolute, traversal-free, canonicalized, contained within
`allowed_roots` when configured, existing and writable — never created). Each
table is streamed in bounded keyset batches written as Parquet row groups, so
peak memory is independent of catalog size, and every query is scoped to
`bundle_id`. The `tsvector` search column is excluded (no portable
representation); `timestamptz` is written as UTC microseconds and `jsonb` as
canonical JSON text. See [security.md](security.md#server-side-file-writes-export_parquet).

### Configuration and safety limits

Two configuration surfaces, described fully in
[configuration.md](configuration.md):

- **GUCs** — four `SIGHUP` resource ceilings (`max_file_bytes`,
  `max_bundle_files`, `max_frontmatter_bytes`, `max_graph_hops`) that can only be
  set in `postgresql.conf`, plus a `SUSET` `log_level`. They are hard safety
  limits no SQL session can raise.
- **Durable policy** — the singleton `pgokf_private.config` row, managed through
  `set_config` / `reset_config` / `get_config`. `allowed_roots` is enforced by
  the sync engine today; `default_text_search_config`, `default_strict`,
  `sync_log_retention_days`, and `default_exclude` are validated and stored but
  reserved for planned functionality.

## Data and API invariants

- A concept ID is bundle-relative and has no `.md` suffix.
- `(bundle_id, concept_id)` is unique.
- Source bytes must be valid UTF-8; normalization must not silently rewrite user
  text.
- The parser never trusts a producer-defined `id` over the path-derived OKF ID.
- Sync and search obey PostgreSQL transaction visibility and role permissions.
- Every value from bundle content or caller input reaches SQL as a bound
  parameter; structural SQL variation uses fixed identifiers chosen in Rust.
- Relevance scores are comparable only within one query; callers rely on
  ordering, not persisted scores.
- SQL objects and examples use `pgokf`, never any former name.

## Packaging and compatibility

The target matrix is PostgreSQL 15, 16, 17, 18, and 19; the crate selects the
major version through a Cargo feature (`pg15`…`pg19`, default `pg18`), and CI
exercises each target. The core package depends only on PostgreSQL/pgrx and its
bundled Rust libraries. Distribution artifacts use names such as
`postgresql-15-pgokf` … `postgresql-19-pgokf`. Optional search integrations are
separately detected, documented, and tested.

## Error handling and observability

Every operation reports failures as a `CatalogError` carrying a bundle-relative
path and mapping to a fixed SQLSTATE (`22023` / `42501` / `23505` / `XX000`); see
[troubleshooting.md](troubleshooting.md). `register_bundle` / `refresh_bundle`
return per-bucket counts (`added` / `updated` / `removed` / `unchanged` /
`total`) suitable for CI and operators. Server logs should include bundle
identity and high-level failure categories without dumping full concept bodies.
Metrics worth exposing later include scan duration, files/bytes read, per-bucket
counts, parse failures, unresolved links, and search latency.

## Evolution

Schema migrations are extension upgrade scripts. The public SQL functions and
composite result types are the compatibility boundary; callers should not depend
on private storage tables. Planned evolution includes consuming the reserved
configuration keys (custom text-search configuration, non-strict sync,
exclusion globs, sync-log retention), richer graph APIs, populating
`bundles.okf_version` from bundle-level `index.md`, incremental filesystem
watching outside the backend, and an optional BM25 adapter after compatibility
and licensing review.
