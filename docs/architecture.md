# pgokf architecture

## Purpose and scope

`pgokf` is a PostgreSQL extension that imports Open Knowledge Format (OKF)
bundles - UTF-8 Markdown concept documents with YAML frontmatter - into a
queryable catalog. The extension name and SQL schema are both `pgokf`; Debian
packages follow the PostgreSQL convention, for example `postgresql-18-pgokf`.

The catalog provides ingestion (filesystem `register_bundle` and mountless
`register_bundle_content`), transactional synchronization, metadata queries,
ranked search (native full-text by default, with optional BM25, semantic, and
hybrid backends), an OKF v0.2 link graph with cycle-safe traversal, a
provenance/trust/lifecycle projection, opt-in multi-tenant row-level security,
opt-in concept version history, bundle lifecycle with audit logging, and
admin-only file exports (Apache Parquet snapshots and stored-source
reconstruction). It does not replace the bundle on disk:
Markdown remains the portable source of truth, while PostgreSQL is a transactional
projection optimized for discovery.

The target matrix is **PostgreSQL 15, 16, 17, 18, and 19**. The extension is built
in Rust (**edition 2024**, workspace `rust-version` 1.96) with
[**pgrx 0.19**](https://github.com/pgcentralfoundation/pgrx); the workspace forbids
`unsafe_code` and treats Clippy `all` + `pedantic` as warnings that CI escalates to
errors.

## Workspace layout

The project is a seven-crate Cargo workspace that cleanly separates
PostgreSQL-independent logic from the database-facing shell, with the
standalone companions built alongside the extension:

| Crate | Role |
| ----- | ---- |
| `crates/okf-parser` | PostgreSQL-independent parser: normalizes concept paths, splits/validates YAML frontmatter, renders the body to plain text, and extracts Markdown links. Produces a database-neutral `ParsedConcept`. |
| `crates/okf-sync` | PostgreSQL-independent filesystem layer: bounded, symlink-escape-safe directory discovery, BLAKE3 content hashing, and the incremental sync report. |
| `crates/extension` | The pgrx extension (package `pgokf`): the SQL surface, base tables, the shared register/refresh engine, search, graph, provenance, history, audit, admin, configuration, roles, GUCs, and error mapping. |
| `crates/pgokf-ingest` | Standalone mountless-ingestion companion: lists an S3-compatible bucket, diffs against the catalog, and streams changed files to `register_bundle_content`; one-shot or continuous with `--watch`. |
| `crates/pgokf-embed` | Standalone embedding companion: computes concept embeddings against an OpenAI-compatible endpoint and streams them in via `set_concept_embedding`. |
| `crates/pgokf-mcp` | Standalone MCP server exposing the catalog read surface (search, graph, provenance) to AI agents over the Model Context Protocol. |
| `crates/pgokf-pgconn` | Shared TLS-capable PostgreSQL connection helper used by the companions. |

Keeping the parser and sync engine free of any pgrx dependency makes them unit
testable without a running backend and keeps the trust boundary, where
untrusted bundle content meets the database, small and explicit. The
companions are ordinary client processes: they speak plain SQL to the
extension's public functions and hold no privileged path into the backend.

## System context

```text
OKF bundle directory                     object store (S3-compatible)
  (*.md + YAML frontmatter)                |
          |                                v
          |                          pgokf-ingest companion
          |                            (lists, diffs, streams bytes)
          v                                |
  path validation / allowed-roots          |   register_bundle_content
  containment (src/security.rs)            |   (bytes over the wire; no
          |                                |    backend filesystem I/O)
          v                                |
  bounded, symlink-safe discovery          |
  + BLAKE3 hash (okf-sync)                 |
          |                                |
          v                                v
  Markdown + YAML parser ----------------------> per-file diagnostics
          |                                       (okf-parser)
          v
  normalized concept records (ParsedConcept)
          |
          +----> pgokf.bundles / pgokf.bundle_log
          +----> pgokf.concepts ------> weighted tsvector / GIN
          +----> pgokf.concept_metadata
          +----> pgokf.links ---------> recursive graph queries (concept_neighbors)
          +----> pgokf.concept_provenance / concept_verification
          |          / concept_provenance_source
          +----> pgokf.concept_source     (opt-in store_source)
          +----> pgokf.concept_history    (opt-in track_history)
          +----> pgokf.concept_embedding  (caller-streamed vectors)
          |
          v
  transactional diff / upsert / delete           (crates/extension/src/catalog/sync.rs)
    + sync audit log, change manifest, NOTIFY    (pgokf_private.sync_log[_change])
          |
          v
SQL API under schema pgokf (39 functions; exact signatures in sql-api.md):
  ingestion   register_bundle / register_bundle_content / refresh_bundle /
              unregister_bundle / set_bundle_enabled / retire_bundle /
              unretire_bundle / set_concept_embedding      (writer tier)
  search      concept_search / search_facets / find_similar /
              concept_search_semantic / concept_search_hybrid /
              concept_neighbors / search_index_status      (reader tier)
  history     concept_history / concept_as_of              (reader tier)
  read/ops    list_bundles / bundle_info / get_config / list_sync_log /
              list_sync_changes / list_bundle_log / catalog_stats / health /
              stale_concepts / duplicate_concepts / get_concept_source /
              version                                      (reader tier)
  admin       set_config / reset_config / purge_retired /
              schedule_refresh / unschedule_refresh /
              rebuild_search_index / rebuild_embedding_index / list_access_log /
              export_parquet / export_sources   (the two file-writing functions)
```

## Components

### Extension boundary

The Rust/pgrx extension exposes SQL objects under the non-relocatable `pgokf`
schema. `CREATE EXTENSION pgokf;` installs the schema, base tables, composite
result types, indexes, functions, the three roles, and the durable-configuration
table. A `bootstrap` SQL block creates the `pgokf` and `pgokf_private` schemas
and the `pgokf_reader` < `pgokf_writer` < `pgokf_admin` role tier, and hardens
schema access, before the feature SQL blocks run. Public entry points are
schema-qualified everywhere in documentation and examples. See
[sql-api.md](sql-api.md) for exact signatures: 39 public functions, 11 public
and 4 private tables, and 14 composite types, locked by the stable-API
guardrail tests in `crates/extension/tests/api_stability.rs` (see
[api-stability.md](api-stability.md)).

### Bundle reader and security boundary

`pgokf.register_bundle(path, name, options)` treats the server-side filesystem as
privileged input. The reader validates the requested root (absolute, no `..`, no
NUL), canonicalizes it, enforces configured allowed roots when present (with both
sides canonicalized so symlinks cannot escape containment), applies the file /
count / byte GUC limits, rejects symlink escapes during discovery, and reads only
accepted Markdown files. Registration and refresh are restricted to the
`pgokf_writer` ingestion tier; read and search are granted separately to
`pgokf_reader`, and configuration, exports, and index rebuilds require
`pgokf_admin`. The mountless path, `register_bundle_content`, accepts concept
bytes as function arguments instead of reading the filesystem, so no path
validation applies there: the backend performs no file I/O at all and the same
parse/diff/project engine runs on the caller-supplied bytes.

The database never executes content from a bundle. Markdown, YAML scalar values,
links, and referenced resources are data only, and every value reaches SQL as a
bound parameter. The full authorization and containment model is in
[security.md](security.md).

### Parser and normalization

For each non-reserved `.md` file, the parser (`okf-parser`):

1. derives the OKF concept ID from the normalized bundle-relative path without
   `.md` - a producer-declared frontmatter `id` is preserved for diagnostics but
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

- **`pgokf.bundles`** - one registered root: identity, canonical path, sync state,
  timestamps, the aggregate `sync_hash` digest, producer `options`, the declared
  `okf_version` (read from the reserved bundle-root `index.md`), and an
  `enabled` flag.
- **`pgokf.concepts`** - one row per `(bundle_id, id)`: path, type, title,
  description, resource, tags, plain-text body, BLAKE3 `file_hash`, timestamps,
  and the weighted `body_tsv` search vector.
- **`pgokf.concept_metadata`** - one row per unrecognized frontmatter key,
  retained as `jsonb`.
- **`pgokf.links`** - directed Markdown edges extracted per concept: source, raw
  and normalized target, label, kind, and the `resolved` / `is_external` flags.
- **`pgokf.concept_provenance`** - a sparse scalar projection of OKF v0.2
  provenance / trust / lifecycle frontmatter: typed columns (`generated_by`,
  `generated_at`, `status`, `stale_after`, `usage_window_from`/`_to`) plus a
  `trust_tier` **derived** from the verification actors and a lossless `details`
  `jsonb`.
- **`pgokf.concept_verification`** - one row per OKF `verified[]` event (the
  ordered `{by, at}` verification list); **`pgokf.concept_provenance_source`** -
  one row per OKF `sources[]` provenance material. Both cascade from
  `pgokf.concepts`.
- **`pgokf.concept_source`** holds the opt-in (`store_source`) verbatim source
  bytes of each concept, for the self-contained storage tier.
- **`pgokf.concept_embedding`** holds one caller-streamed embedding vector per
  concept (`set_concept_embedding`), backing semantic and hybrid search.
- **`pgokf.bundle_log`** is the projection of every reserved `log.md` activity
  log in a bundle, one row per parsed entry, read via `list_bundle_log`.
- **`pgokf.concept_history`** is the opt-in (`track_history`) append-only
  version history behind `concept_history` and `concept_as_of`.

Four private tables live in `pgokf_private`: the singleton `config` policy row,
the append-only `sync_log` audit trail with its per-file `sync_log_change`
manifest, and the `access_log` exfiltration audit (read via the admin-only
`list_access_log`).

`(bundle_id, id)` is the concept key, so concepts with the same path in different
bundles stay distinct. The 14 composite result types (`bundle_sync_result`,
`concept_search_result`, `concept_neighbor`, `bundle_info`, `export_result`,
`sync_log_entry`, `catalog_stat`, `stale_concept`, `sync_change`,
`access_log_entry`, `duplicate_group`, `search_facet`, `bundle_log_entry`, and
`concept_version`) are the stable shapes returned to callers.

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
5. parse only added/changed files. Under the default `default_strict` policy
   the first malformed file aborts the sync (`22023`) and the transaction rolls
   back, so a partial projection is never committed; with `default_strict` off,
   a malformed file is logged and skipped instead;
6. delete rows for removed files, upsert changed rows (recomputing the weighted
   `body_tsv`), and replace `concept_metadata` for touched concepts;
7. run the ordered projection seam over the staged concepts: links, then
   provenance, then the opt-in verbatim source bytes (`store_source`); project
   the reserved `log.md` files into `pgokf.bundle_log`, and, when
   `track_history` is on, append the changed versions to
   `pgokf.concept_history` and prune to `history_retention_days`;
8. update the bundle row (file count, `last_synced_at`, aggregate `sync_hash`),
   append the `pgokf_private.sync_log` audit row with its per-file
   `sync_log_change` manifest (pruned to `sync_log_retention_days`), and fire a
   `pg_notify` on the configured `notify_channel`, all in the same transaction.

Feature tables (`links`, `concept_provenance`, `concept_metadata`) cascade from
`pgokf.concepts` via foreign keys, so removals need no seam call. Concurrent
syncs of one bundle serialize on the advisory lock; distinct bundles proceed in
parallel.

#### Extension seams

The backbone is open for extension and closed for modification. Core modules own
the schema and the sync loop; feature modules attach through fixed seams and
never edit the core. `catalog/sync.rs` calls `links::project`, then
`provenance::project`, then `source::project` after staging concept rows, with
`bundle_log::project` and the opt-in `history::project` running in the same
transaction; `catalog/schema.rs` owns the base tables under the named
`catalog_tables` SQL block, and each feature orders its SQL after it with
`requires = ["catalog_tables"]`. This is how links, neighbors, provenance,
source retention, bundle logs, history, embeddings, audit, admin, and config
were each added without touching the sync engine.

### Search

The default backend is PostgreSQL native FTS, so every supported PostgreSQL
15–19 installation works without another extension. A weighted document favors
title (A), then tags/type/description (B), then body (D), with a GIN index on
`body_tsv` for matching. `pgokf.concept_search` matches with
`websearch_to_tsquery`, ranks with `ts_rank_cd`, attaches a `ts_headline`
snippet, and searches only active bundles (enabled and not retired). Beyond the
query, bundle scope, and limit, it takes optional structured filters (concept
type, ALL-of tags, provenance `status` and `trust_tier`) and an `after_cursor`
for OFFSET-free keyset pagination; results order by rank with the bundle and
concept IDs as deterministic tiebreakers, so equal-rank hits order stably and
pages never skip or repeat rows. `search_facets` counts the same matching set
grouped by a chosen facet (type, bundle, status, trust tier, or tag),
`find_similar` ranks concepts by content similarity to a seed concept's salient
lexemes, and `search_index_status` reports index health and coverage.
`rebuild_search_index` (admin) rebuilds the FTS projection in place. Ranks are
comparable only within
one query; callers order by them rather than persisting them.

An optional backend selected by the durable `search_backend` key routes the same
`pgokf.concept_search` through a ParadeDB `pg_search`/BM25 index at runtime,
returning the identical logical result shape. It is opt-in and requires the
operator to install `pg_search` separately: `CREATE EXTENSION pgokf` takes no
hard dependency on ParadeDB, and search falls back to native FTS with a warning
when `pg_search` is absent. See
[Enabling the BM25 backend](search-guide.md#enabling-the-bm25-backend) and the
[`search_backend` key](configuration.md#search-backend-search_backend).

Semantic and hybrid search are likewise opt-in, built on `pgvector`.
The extension never computes embeddings: a caller (typically the `pgokf-embed`
companion) streams vectors of the configured `embedding_dim` in through
`set_concept_embedding` (writer tier), and `rebuild_embedding_index` (admin)
builds the HNSW index. `concept_search_semantic` ranks by vector distance and
`concept_search_hybrid` fuses the lexical and semantic rankings with reciprocal
rank fusion (RRF). The absence of `pgvector` degrades cleanly: semantic search
raises `22023` naming the missing dependency, and hybrid search falls back to
lexical-only with a warning. See
[semantic and hybrid search](search-guide.md#semantic-and-hybrid-search-optional-pgvector).

### Link graph (OKF v0.2)

Internal Markdown destinations are normalized relative to the source document or
bundle root; fragment identifiers do not change the target concept ID. External
URLs and email links are retained but never become internal edges. An internal
link is marked `resolved` only when its target concept exists in the same bundle
at sync time; broken internal links are retained as unresolved because OKF
permits them and a later sync may resolve them. An OKF v0.2 Attested
Computation concept additionally contributes typed frontmatter-derived edges
(`attestation:computation`, `attestation:executor`, `attestation:attester`)
that live in `pgokf.links` beside the Markdown edges and traverse like any
resolved internal edge.

`pgokf.concept_neighbors(concept_id, max_hops, bundle_id)` walks the graph with a
cycle-safe recursive CTE over `pgokf.links`. It follows only resolved,
non-external edges, is bundle-scoped, depth-limited (`max_hops >= 1`, capped at
`pgokf.max_graph_hops`), and authorization-filtered at reader level. It returns
each reachable concept once with its shortest hop count and path.
[`examples/queries/graph.sql`](https://github.com/LogicOcean/pgokf/blob/main/examples/queries/graph.sql) shows both direct
edge queries and the built-in traversal alongside an equivalent hand-written CTE.

### Parquet export

`pgokf.export_parquet(bundle_id, dest_dir)` (`catalog/export.rs`) is a
self-contained feature module attached at the same extension seam as the others:
it reads the catalog projection and never touches the sync engine or the base
schema. It writes one Apache Parquet file per table for the requested bundle -
`concepts`, `concept_metadata`, `links`, `concept_provenance` - into a validated
server-side directory and returns an `export_result` with the per-file row counts
and total bytes written. It is one of only **two** functions in the extension
that write files (the other is `export_sources`, below), so it is admin-only and
validates `dest_dir` exactly as strictly as a
bundle input root (absolute, traversal-free, canonicalized, contained within
`allowed_roots` when configured, existing and writable - never created). Each
table is streamed in bounded keyset batches written as Parquet row groups, so
peak memory is independent of catalog size, and every query is scoped to
`bundle_id`. The `tsvector` search column is excluded (no portable
representation); `timestamptz` is written as UTC microseconds and `jsonb` as
canonical JSON text. See [security.md](security.md#server-side-file-writes-export_parquet).

### Storage tiers (source retention)

A single durable toggle, `store_source`, lets one build serve two deployment
shapes without any other change (`catalog/source.rs`, attached at the same
projection seam as links and provenance - the sync engine calls
`source::project` inside the atomic, advisory-locked transaction and is never
edited):

- **Small, self-contained (`store_source = true`).** The sync engine already
  reads each source file into memory to parse it; under this tier it retains that
  exact buffer (no extra I/O) and the seam upserts it into `pgokf.concept_source`
  (`bytea`, TOAST-compressed - `lz4` where the build supports it, else `pglz`).
  The *original* files then live inside PostgreSQL: a reader fetches a concept's
  exact bytes with `get_concept_source`, and an admin rebuilds the bundle on disk
  byte-for-byte with `export_sources`, which reuses `export.rs`'s destination
  validation and `O_NOFOLLOW` file creation and verifies every written file
  against the concept's BLAKE3 `file_hash`. No external object store is needed.
- **Enterprise data-lake (`store_source = false`, the default).** Nothing is
  written to `concept_source`; the verbatim files stay in a mounted object store /
  data lake and PostgreSQL holds only the metadata-and-search projection. The
  default is therefore byte-for-byte identical to a build without the feature.

`concept_source` cascades from `pgokf.concepts` (`ON DELETE CASCADE`), so removed
concepts and unregistered bundles drop their stored source with no extra seam.
Like `default_text_search_config`, `store_source` is read at sync time and is not
retroactive - see [configuration.md](configuration.md)
and [security.md](security.md#source-retrieval-and-reconstruction).

### Configuration and safety limits

Two configuration surfaces, described fully in
[configuration.md](configuration.md):

- **GUCs** - four `SIGHUP` resource ceilings (`max_file_bytes`,
  `max_bundle_files`, `max_frontmatter_bytes`, `max_graph_hops`) that can only be
  set in `postgresql.conf`, plus a `SUSET` `log_level`. They are hard safety
  limits no SQL session can raise. A sixth GUC, the `USERSET` `pgokf.tenant`,
  is not a limit at all: it is the per-session tenant selector for the opt-in
  row-level security described below.
- **Durable policy** - the singleton `pgokf_private.config` row, managed through
  `set_config` / `reset_config` / `get_config`. All twelve keys are consumed by
  the current engine: `allowed_roots` is enforced on every server-side path,
  and `default_text_search_config` is applied by the sync engine: it is the
  `regconfig` for `to_tsvector` when building each concept's `body_tsv` at
  index time and for `websearch_to_tsquery`/`ts_headline` at query time, so query
  parsing matches the configuration that indexed the rows. Because
  `refresh_bundle` re-parses only files whose content hash changed, changing
  `default_text_search_config` is **not retroactive**: already-synced rows keep
  the tsvector built under the previous configuration, so search can mismatch
  them until the bundle is re-registered (see
  [configuration.md](configuration.md)). `store_source` selects the storage
  tier above and is likewise non-retroactive. `default_strict` chooses between
  abort-on-first-malformed-file and log-and-skip, `default_exclude` supplies
  glob patterns the discovery walk skips, `sync_log_retention_days` prunes the
  sync audit log, `search_backend` selects native FTS or BM25,
  `notify_channel` names the `pg_notify` change channel (empty disables),
  `okf_version_policy` chooses `warn` or `reject` for unsupported bundle
  `okf_version` declarations, `embedding_dim` fixes the accepted embedding
  dimension, and `track_history` / `history_retention_days` govern the opt-in
  version history.

### Multi-tenancy (opt-in row-level security)

Every projection table carries a denormalized `tenant_id` (default `'default'`)
and enables, but does not force, PostgreSQL row-level security with one shared
predicate keyed on the `pgokf.tenant` session GUC: a session that has not set
`pgokf.tenant` (NULL or empty, which is every pre-multi-tenancy install) sees
all rows unchanged, while a session that has set it sees only that tenant's
rows, and the matching `WITH CHECK` confines invoker-side writes to the active
tenant. Because RLS is not forced, the `SECURITY DEFINER` write and admin
functions (which run as the table owner) bypass it, which is correct because
each operates strictly within one single-tenant bundle. The honest caveat:
`pgokf.tenant` is a `USERSET` scoping selector, not a hard security boundary
against a principal who can run arbitrary SQL and re-`SET` it; see
[multi-tenancy.md](multi-tenancy.md) and
[security.md](security.md#multi-tenant-row-level-security).

### Version history (opt-in)

When the durable `track_history` key is on, each sync appends the superseded
version of every changed or removed concept to the append-only
`pgokf.concept_history` table in the same transaction. `concept_history` lists
a concept's versions and `concept_as_of` answers point-in-time questions,
both returning the `concept_version` composite type. Closed versions are
pruned to `history_retention_days` after each sync (0, the default, keeps them
indefinitely); the feature is off by default and costs zero storage until
enabled. See [version-history.md](version-history.md).

### Lifecycle, audit, and observability

Bundles have a lifecycle beyond the `enabled` flag: `retire_bundle` /
`unretire_bundle` open and close a soft-delete window that hides a bundle from
search while keeping its rows, and the admin-only `purge_retired` permanently
removes bundles retired longer than a given interval. Every successful
register/refresh/content sync or unregister appends a
`pgokf_private.sync_log` row with a per-file `sync_log_change` manifest
(`list_sync_log` / `list_sync_changes`), and every content-exporting operation
(`export_parquet`, `export_sources`, `get_concept_source`) appends a
`pgokf_private.access_log` row read via the admin-only `list_access_log`; both
logs are pruned to `sync_log_retention_days`. For monitoring, `catalog_stats`
reports per-bundle counts and sync recency, `health` returns a jsonb
liveness/readiness document, `stale_concepts` lists concepts past their OKF
`stale_after` instant, and `duplicate_concepts` reports identical content
hashes across bundles. `schedule_refresh` / `unschedule_refresh` wire periodic
`refresh_bundle` runs through `pg_cron` when that extension is installed. See
[operations.md](operations.md).

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
The observability surface is queryable in-database: `catalog_stats`, `health`,
`stale_concepts`, and `search_index_status` for monitoring, the sync and access
logs for auditing, and the optional `notify_channel` `LISTEN`/`NOTIFY` stream
for push-style change notification (see
[operations.md](operations.md)).

## Evolution

Schema migrations are extension upgrade scripts. The public SQL functions and
composite result types are the compatibility boundary; callers should not depend
on private storage tables. Much of the evolution once planned here has since
shipped: every configuration key is consumed, `bundles.okf_version` is
populated from the bundle-root `index.md` and policed by `okf_version_policy`,
incremental filesystem watching runs outside the backend (`pgokf-ingest
--watch`), and the optional BM25, semantic, and hybrid search backends are in
place. The surface is versioned and guarded as described in
[api-stability.md](api-stability.md); the
[CHANGELOG](https://github.com/LogicOcean/pgokf/blob/main/CHANGELOG.md)
records how it got here.
