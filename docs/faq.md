# Frequently asked questions

Grounded answers to the questions this project actually raises. Every function,
table, role, GUC, and SQLSTATE named below exists in the shipped extension.
Cross-links point to the deep documentation for each topic.

- [Installation and access](#installation-and-access)
- [Licensing](#licensing)
- [OKF and the catalog model](#okf-and-the-catalog-model)
- [Identifiers and schema design](#identifiers-and-schema-design)
- [Storage tiers and getting files back](#storage-tiers-and-getting-files-back)
- [Search](#search)
- [Multi-tenancy and version history](#multi-tenancy-and-version-history)
- [Companion tools](#companion-tools)
- [Interoperability and portability](#interoperability-and-portability)
- [PostgreSQL support and operations](#postgresql-support-and-operations)

---

## Installation and access

### Why does a fresh login role get a permission error (SQLSTATE 42501)?

Because the `pgokf` schema is **not** granted to `PUBLIC`. During
`CREATE EXTENSION pgokf`, the bootstrap step runs
`REVOKE ALL ON SCHEMA pgokf FROM PUBLIC` and grants `USAGE` only to the three
extension roles. A brand-new login role is a member of none of them, so its
first call into the schema fails with SQLSTATE `42501`
(`insufficient_privilege`).

The fix is a single GRANT that makes the login role a member of the right tier:

```sql
GRANT pgokf_reader TO app_reader;    -- search + read the catalog
GRANT pgokf_writer TO ingest_bot;    -- register/refresh bundles (inherits reader)
GRANT pgokf_admin  TO catalog_ops;   -- config + exports (inherits writer)
```

All three roles (`pgokf_reader`, `pgokf_writer`, `pgokf_admin`) are created
`NOLOGIN`: they are privilege buckets, not accounts. The hierarchy is
`pgokf_reader` < `pgokf_writer` < `pgokf_admin`, each tier inheriting the one
below, so a writer can search and an admin can ingest without a second grant.
See [Security](security.md) for the full model.

### Do I need superuser to install or use the extension?

Installing (`CREATE EXTENSION pgokf`) requires the usual privilege to create an
extension. Day-to-day use does not require superuser: bundle registration,
refresh, search, and configuration all run through the three `pgokf` roles. The
functions that touch the server filesystem (`register_bundle`, `refresh_bundle`,
`export_parquet`, `export_sources`) are `SECURITY DEFINER` with a hardened
`search_path`, and are constrained by the `allowed_roots` policy - see
[Security](security.md) and [Configuration](configuration.md).

### The bundle path is on my laptop but registration fails - why?

The path argument to `register_bundle` is opened by the **PostgreSQL server
process**, not your client. It must be an absolute path the server can reach and
must canonicalize (no `..` traversal, no symlink escape). If `allowed_roots` is
configured, the canonical path must also resolve inside one of those roots. A
path that is not registerable raises a validation error; a path already
registered (by the current tenant) raises SQLSTATE `23505` (use `refresh_bundle`
to re-sync it instead). See
[Deployment topologies](deployment-topologies.md) for how to make bundle files
reachable by the server, including the **mountless** path
(`register_bundle_content` plus the `pgokf-ingest` companion) when the server
cannot mount the files at all.

---

## Licensing

### What license is pgokf under?

**AGPL-3.0-only, dual-licensed with a commercial option.** Every crate in the
repository (the extension, its `okf-parser` / `okf-sync` libraries, and the
companion tools `pgokf-ingest`, `pgokf-embed`, `pgokf-mcp`, and `pgokf-pgconn`)
is licensed under the GNU Affero General Public License, version 3.0 only. A
separate **commercial license** is available for use the AGPL does not permit,
such as embedding pgokf in a proprietary product or offering it as a hosted
service without releasing your modifications. No part of the project is
permissively licensed. See
[`LICENSING.md`](https://github.com/LogicOcean/pgokf/blob/main/LICENSING.md)
for the details and the commercial contact.

### Does my application become AGPL by connecting to a pgokf database?

Building applications that merely *connect to* a PostgreSQL server with pgokf
installed does not by itself obligate you to release your application's source,
and neither does running unmodified pgokf inside your organization. The AGPL's
network clause applies when you distribute a **modified** pgokf or make a
modified version available to others over a network. This is a summary, not
legal advice; the license text governs. See
[`LICENSING.md`](https://github.com/LogicOcean/pgokf/blob/main/LICENSING.md).

---

## OKF and the catalog model

### What does "OKF v0.2 conformance" actually mean here?

It means the catalog model matches the OKF v0.2 spec's data model:

- Every concept must carry a `type`; `title`, `description`, `resource`, and
  `tags` are recommended and projected as first-class columns on
  `pgokf.concepts`.
- The provenance/trust/lifecycle families - `sources`, `generated` (`by`/`at`),
  `verified[]` events, `status`, `stale_after`, and `usage_window` - are parsed
  and projected into dedicated tables rather than being flattened away.
- Actors follow the OKF convention `<producer>/<version>`, `human:<id>`, or
  `process:<id>` in fields like `generated.by` and `verified[].by`.
- The bundle root's reserved `index.md` supplies the bundle's `okf_version`
  (stored on `pgokf.bundles.okf_version`); the reserved per-directory `log.md`
  activity logs are projected into `pgokf.bundle_log` without ever becoming
  concepts. The `okf_version_policy` configuration key decides whether a bundle
  declaring an unsupported OKF version is warned about or rejected.
- An `Attested Computation` concept's spec-mandated reference fields become
  typed, traversable graph edges (see the next answers).

Producer-defined frontmatter keys that are not part of the standard projection
are not discarded - they are retained per concept in `pgokf.concept_metadata`
as `jsonb`, one row per key, and are indexed for containment queries. See
[OKF authoring](okf-authoring.md).

### How is provenance modeled in the tables?

Provenance is split by cardinality across three tables so each shape stays
queryable:

- **`pgokf.concept_provenance`** - one sparse row per concept carrying the
  scalar generation/trust/lifecycle fields: `generated_by`, `generated_at`,
  `status`, `stale_after`, `usage_window_from/to`, `trust_tier`, plus a lossless
  `details` `jsonb`.
- **`pgokf.concept_verification`** - the `verified[]` event list, one row per
  event (`verified_by`, `verified_at`, `ordinal`).
- **`pgokf.concept_provenance_source`** - the `sources[]` materials a concept
  was derived from, one row per source (`source_id`, `resource`, `title`,
  `author`, `usage_count`, `last_modified`, `usage_window_from/to`).

All three cascade from `pgokf.concepts`, so unregistering a bundle or dropping a
concept removes its provenance automatically. See the [SQL API](sql-api.md) for
column-by-column detail.

### What is an "Attested Computation" concept?

In OKF v0.2, `Attested Computation` is the one concept `type` with
type-specific fields (attesting *how* a computed artifact was produced). pgokf
treats `type` as data - it is a projected column and a search weight, not a
hard-coded enum - so an `Attested Computation` concept is ingested like any
other, with two conformance additions:

- Its three **reference-bearing** fields (`computation`, `executor`,
  `attester`, each a bare resource path or a `{resource: ...}` mapping) are
  resolved into `pgokf.links` as typed edges carrying a `link_relation` of
  `attestation:computation` / `attestation:executor` / `attestation:attester`,
  and `pgokf.concept_neighbors` traverses a resolved internal one like any
  other edge.
- Its non-reference fields (`runtime`, `parameters`) are retained as producer
  metadata in `concept_metadata`, like any non-modeled key.

See [the authoring guide](okf-authoring.md#the-one-special-type-attested-computation)
and the [glossary](glossary.md#attested-computation).

### Are reserved files ingested as concepts?

No. `index.md` and `log.md` are reserved at every directory level and never
become searchable concepts, and they never count toward a bundle's
`file_count`. Each is still put to use: the bundle root's `index.md` supplies
`okf_version`, and every `log.md` is parsed line by line into the
`pgokf.bundle_log` activity-log table (read it with
`pgokf.list_bundle_log(bundle_id[, directory])`). In the sample bundle,
`register_bundle` reports four concepts because the reserved `index.md` is
skipped. See [OKF authoring](okf-authoring.md#reserved-files-indexmd-and-logmd).

---

## Identifiers and schema design

### Why are bundle IDs `bigint` instead of UUIDs?

`pgokf.bundles.id` is `bigint GENERATED ALWAYS AS IDENTITY`. Bundle
registration is a **single-writer** operation serialized on a per-path advisory
lock, so there is no ID-collision problem to solve, and a monotonic `bigint`
gives better B-tree **index locality** and smaller foreign keys - every
`concepts`, `links`, `concept_metadata`, and provenance row carries `bundle_id`,
so the width matters. A UUID (e.g. UUIDv7) earns its keep only when many
independent writers or federated catalogs must mint IDs without coordinating;
that is not the current single-catalog model. If federation is later needed, the
surrogate key is the natural seam to change. See [Architecture](architecture.md).

### Concept IDs are strings - where do they come from?

A concept's `id` is derived from its path: the normalized, bundle-relative path
with the `.md` suffix removed (e.g. `runbooks/database-failover`). The primary
key on `pgokf.concepts` is `(bundle_id, id)`, so the same logical path in two
bundles is two distinct concepts. This is why `concept_neighbors` needs a
`bundle_id` to disambiguate when an ID exists in more than one **active** bundle
(it raises SQLSTATE `22023` if you omit it and the ID is ambiguous; disabled and
retired bundles do not count toward the ambiguity).

---

## Storage tiers and getting files back

### What are the two storage tiers, and which should I use?

The `store_source` config key selects the tier (default `false`):

- **`store_source = false` (default) - data-lake / enterprise tier.**
  PostgreSQL holds metadata, the search index, the link graph, and provenance.
  The original files stay in their external object store or mounted bucket. Best
  when the files are large or already live in a data lake, and you want
  PostgreSQL to be the query layer, not the store of record.
- **`store_source = true` - small self-contained tier.** Each concept's verbatim
  source bytes are also stored in `pgokf.concept_source` (compressed with lz4
  where the build supports it, else pglz). The database is then a complete,
  portable install that needs no external file store.

`store_source` is **not** retroactive - a change takes effect for bundles synced
or refreshed afterward; existing rows keep their stored source (or its absence)
until `refresh_bundle` re-indexes them. See
[Deployment topologies](deployment-topologies.md) and
[Configuration](configuration.md).

### Can I get the original files back out of PostgreSQL?

Only when they were stored (`store_source = true`). Two functions read them back:

- **`pgokf.get_concept_source(bundle_id, concept_id)`** returns the exact bytes
  of one stored concept file (`bytea`). Reader-level.
- **`pgokf.export_sources(bundle_id, dest_dir)`** writes a bundle's stored
  originals back to a directory on the server. Admin-only, and constrained by
  the same `allowed_roots` path policy as registration.

The stored bytes are verbatim: they hash back to `pgokf.concepts.file_hash`
(BLAKE3). With `store_source = false` these functions have nothing to return -
the files live only in your external store. Every read/export through these
paths (and `export_parquet`) also appends one row to the access audit
(`pgokf_private.access_log`, read with the admin-tier `pgokf.list_access_log`).
See the [SQL API](sql-api.md) and [Operations](operations.md).

---

## Search

### Is search real BM25, or PostgreSQL FTS?

Shipped search is **native PostgreSQL full-text search** - no third-party
extension. `pgokf.concept_search` builds a query with `websearch_to_tsquery` and
ranks with `ts_rank_cd` against a weighted `tsvector` (`body_tsv`): title
weight A, tags/type/description weight B, body weight D. It returns a
`ts_headline` snippet per hit. The `tsvector` is indexed with GIN
(`concepts_body_tsv_gin`).

BM25 is available as an **optional, config-selected backend**, not a separate
function. Setting the durable `search_backend` key to `bm25` routes the *same*
`pgokf.concept_search` through a provider's `bm25` index (Tiger Data
`pg_textsearch`, PostgreSQL license, or ParadeDB `pg_search`) - the operator
must install the provider separately, and search falls back to native FTS
with a warning when it is absent. There is no standalone `pgokf` `bm25()` function; it
is a backend mode selected by configuration. See
[Enabling the BM25 backend](search-guide.md#enabling-the-bm25-backend) and the
[`search_backend` key](configuration.md#search-backend-search_backend).

### When does native FTS become a problem, and what does BM25 buy?

Measured on this project:

- **Selective queries** - point lookups, tag filters, type filters - stay
  **sub-millisecond to roughly 10-15 ms** even at ~10M concepts, because they
  ride B-tree / GIN indexes.
- **Broad "rank everything" full-text queries** scale **linearly** with corpus
  size: about **322 ms at 1M**, **2.4 s at 10M**, and **29 s at 50M** concepts,
  because `ts_rank_cd` must score every match.

For broad top-k queries over very large corpora, the optional `bm25` backend
(`search_backend=bm25`, backed by Tiger Data `pg_textsearch` or ParadeDB
`pg_search`) prunes with BM25 top-k instead of scoring the whole match set, so
broad queries stay roughly flat where native ranking grows linearly. It
requires the operator to install a provider and falls back to native FTS when
none is present. So native FTS is the
right default for selective queries and moderate corpora; enable the `bm25`
backend when you need broad top-k at scale. See
[Enabling the BM25 backend](search-guide.md#enabling-the-bm25-backend),
[Benchmarks](benchmarks.md), and the [Search guide](search-guide.md).

### How do I search within one bundle, filter, or paginate?

`concept_search(query, bundle_id => NULL, limit_count => 20, ...)`. Pass a
`bundle_id` to scope the search to one bundle; `limit_count` must be in the range
`1..=500` (otherwise SQLSTATE `22023`). Search only ever touches **active**
bundles (enabled and not retired). Four optional structured-filter arguments
(`concept_type`, `tags` as ALL-of containment, `status`, `trust_tier`) narrow
the ranked set, each a no-op when `NULL`, and the trailing `after_cursor jsonb`
argument gives exact **keyset pagination**: copy the `rank`, `bundle_id`, and
`concept_id` of a page's last row into it and the next page continues strictly
after that row, with no `OFFSET` drift. `pgokf.search_facets` counts the same
matching set grouped by one facet (`type`, `bundle`, `status`, `trust_tier`, or
`tag`), and `pgokf.find_similar` ranks concepts against a seed concept's most
salient lexemes. See the [Search guide](search-guide.md).

### Does pgokf do semantic (vector) search?

Yes, as an **optional** surface built on pgvector, mirroring the optional BM25
seam (`CREATE EXTENSION pgokf` succeeds without pgvector):

- A companion (the shipped `pgokf-embed`, or your own) computes embeddings
  *outside* PostgreSQL and streams them in through the writer-tier
  `pgokf.set_concept_embedding(bundle_id, concept_id, embedding real[])`; the
  extension never computes embeddings or performs network I/O. The expected
  vector length is the durable `embedding_dim` config key (default 1536).
- **`pgokf.concept_search_semantic(query_embedding, ...)`** ranks by pgvector
  cosine distance (the `rank` column is the normalized cosine similarity). It
  **requires pgvector** and raises SQLSTATE `22023` naming the missing
  dependency when it is absent; there is no lexical fallback.
- **`pgokf.concept_search_hybrid(query, query_embedding, ...)`** fuses the
  lexical and semantic result lists with Reciprocal Rank Fusion (RRF, k = 60)
  entirely in SQL, and degrades to lexical-only with a `WARNING` when pgvector
  is absent.
- **`pgokf.rebuild_embedding_index()`** (admin-tier) builds a pgvector HNSW
  cosine index for the configured dimension.

See [Semantic and hybrid search](search-guide.md#semantic-and-hybrid-search-optional-pgvector)
and the [`embedding_dim` key](configuration.md#embedding-dimension-embedding_dim).

### Why doesn't my query language config change affect existing rows?

The `default_text_search_config` key (default `pg_catalog.english`) is applied
when a concept's `tsvector` is built at index time and when a search query is
parsed. Changing it affects bundles synced or refreshed **afterward**; existing
rows keep their `tsvector` until `refresh_bundle` re-indexes them. It must name
an installed configuration (validated against `pg_catalog.pg_ts_config`). See
[Configuration](configuration.md).

---

## Multi-tenancy and version history

### Does pgokf support multi-tenancy or row-level security?

Yes: **opt-in row-level security keyed on the `pgokf.tenant` session GUC**,
shipped since 0.1.7 and backward compatible by construction:

- A session that never sets `pgokf.tenant` (the default) sees every row and
  behaves exactly as before multi-tenancy existed (unless the `require_tenant`
  policy is on, which denies it); a session that sets it
  (`SET pgokf.tenant = 'acme'`, per role via `ALTER ROLE`, or as a connection
  option) reads only that tenant's rows.
- Writes are stamped and confined: `register_bundle` /
  `register_bundle_content` stamp the bundle from the session tenant, every
  projected child row inherits it, and a tenant-set session that targets a
  bundle owned by another tenant gets the same `22023` "bundle ... is not
  registered" it would get for a nonexistent bundle.
- Bundles are keyed `UNIQUE (tenant_id, path)`, so two tenants may register the
  same path independently.

One honest caveat: `pgokf.tenant` is a `USERSET` GUC, so it is a **scoping
selector, not a hard security boundary** against a tenant who can run arbitrary
SQL: any session can `SET` or `RESET` it, and the unset default sees all rows
(unless `require_tenant` is on).
A hard boundary requires a constrained access layer that pins the GUC, or a
per-tenant-role / per-database model. See [Multi-tenancy](multi-tenancy.md) and
[Security](security.md#multi-tenant-row-level-security).

### Can I see what a concept said last Tuesday?

Yes, with **opt-in version history** (since 0.1.11). Enable the `track_history`
config key and every subsequent sync records an append-only SCD Type-2 version
trail in `pgokf.concept_history`: an added concept starts at version 1, an
update closes the open version and appends the next, and a removal appends a
tombstone. Read it with `pgokf.concept_history(bundle_id, concept_id)` (the
timeline, newest first) and `pgokf.concept_as_of(bundle_id, concept_id, as_of)`
(the single version valid at an instant: the point-in-time answer). The
feature is **off by default** with zero storage cost, is not retroactive
(recording begins at the next sync), and `history_retention_days` bounds growth
by pruning closed versions older than the window. See
[Version history](version-history.md).

---

## Companion tools

### What are the companion binaries, and do they run inside PostgreSQL?

Four standalone crates ship alongside the extension. All of them run
**out of process**, keep network and object-store credentials outside
PostgreSQL, and reach the catalog only through its public SQL functions:

- **`pgokf-ingest`**: mountless ingestion. Lists an S3-compatible object store
  (MinIO / SeaweedFS / AWS S3 / GCS / Azure), downloads the objects, and streams
  them to `pgokf.register_bundle_content` as a `pgokf_writer` role. A `--watch`
  mode re-lists on an interval (default 60 s) and re-ingests when the content
  hash changes.
- **`pgokf-embed`**: the reference embedding generator for semantic search.
  Finds concepts without embeddings, calls a configurable OpenAI-compatible
  `/v1/embeddings` endpoint, and streams each vector back through
  `pgokf.set_concept_embedding`.
- **`pgokf-mcp`**: a Model Context Protocol server exposing the catalog to AI
  agents over stdio JSON-RPC (tools backed by `concept_search`, `find_similar`,
  `concept_neighbors`, and a concept getter).
- **`pgokf-pgconn`**: the shared connect helper the other three use; it adds
  optional TLS to PostgreSQL (`--tls`, env `OKF_PG_TLS`, or
  `sslmode=require`).

See [Deployment topologies](deployment-topologies.md#enterprise-tier-mountless-the-ingestion-companion)
for the ingestion companion in context.

---

## Interoperability and portability

### Can I export the catalog for use in other tools?

Yes. `pgokf.export_parquet(bundle_id, dest_dir)` snapshots a bundle's projection
to four Parquet files - `concepts.parquet`, `concept_metadata.parquet`,
`links.parquet`, and `concept_provenance.parquet` - in a server-side directory,
returning per-file row counts and total bytes (`pgokf.export_result`). The
output was **verified interoperable with DuckDB** on this project: you can point
DuckDB straight at the files and query them. `export_parquet` is admin-only and
its destination is validated exactly like a bundle root (absolute,
traversal-free, inside `allowed_roots`). See [Operations](operations.md).

### Is the bundle on disk still authoritative after I import it?

Yes - that is the design. The Markdown bundle is the portable source of truth;
the `pgokf` schema is a projection you can rebuild at any time by re-running
`refresh_bundle`. Incremental sync re-parses only files whose BLAKE3 content
hash changed and removes rows for deleted files, so the catalog converges to
whatever is on disk. See [Architecture](architecture.md). (A content-sourced
bundle, one streamed in with `register_bundle_content`, has no on-disk root;
re-sync it by calling `register_bundle_content` again.)

---

## PostgreSQL support and operations

### Which PostgreSQL versions are supported?

PostgreSQL **15, 16, 17, 18, and 19** from one codebase (pgrx feature flags
`pg15`–`pg19`; CI builds the matrix). See [Packaging](packaging.md).

### Which optional extensions does pgokf use?

Four, all runtime-only and all degrading cleanly when absent;
`CREATE EXTENSION pgokf` never requires any of them:

- **pgvector** enables `concept_search_semantic` (which raises `22023` without
  it) and the semantic half of `concept_search_hybrid` (which degrades to
  lexical-only with a `WARNING`).
- **`pg_textsearch`** (Tiger Data, PostgreSQL license, PostgreSQL 17-18) or
  **`pg_search`** (ParadeDB, AGPL-3.0) enables the `search_backend=bm25` mode
  (one per database; `bm25_provider` chooses); `concept_search` falls back to
  native FTS with a warning without one.
- **`pg_cron`** enables `schedule_refresh` / `unschedule_refresh`;
  `schedule_refresh` raises `22023` naming the missing dependency without it.

`pgokf.search_index_status()` reports which of them are installed and how much
of the catalog their indexes cover. See [Operations](operations.md).

### Is `sync_log_retention_days` doing anything?

Yes, since 0.1.5. Every successful sync appends one row to the admin-only
`pgokf_private.sync_log` audit trail (read via `pgokf.list_sync_log`), and
immediately afterward history older than `sync_log_retention_days` (default
`30`; `0` keeps it forever) is pruned in the same transaction. Since 0.1.8 the
same window also governs the per-concept change manifest
(`pgokf_private.sync_log_change`, which cascades from `sync_log`) and the
access/exfiltration audit (`pgokf_private.access_log`). See
[Configuration](configuration.md#audit-log-retention-sync_log_retention_days).

---

## Still stuck?

- Exact function signatures and column lists: [SQL API](sql-api.md)
- Error-to-SQLSTATE mapping with fixes: [Troubleshooting](troubleshooting.md)
- Term definitions: [Glossary](glossary.md)
