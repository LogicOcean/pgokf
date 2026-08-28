# Frequently asked questions

Grounded answers to the questions this project actually raises. Every function,
table, role, GUC, and SQLSTATE named below exists in the shipped extension.
Cross-links point to the deep documentation for each topic.

- [Installation and access](#installation-and-access)
- [OKF and the catalog model](#okf-and-the-catalog-model)
- [Identifiers and schema design](#identifiers-and-schema-design)
- [Storage tiers and getting files back](#storage-tiers-and-getting-files-back)
- [Search](#search)
- [Interoperability and portability](#interoperability-and-portability)
- [PostgreSQL support and multi-tenancy](#postgresql-support-and-multi-tenancy)

---

## Installation and access

### Why does a fresh login role get a permission error (SQLSTATE 42501)?

Because the `pgokf` schema is **not** granted to `PUBLIC`. During
`CREATE EXTENSION pgokf`, the bootstrap step runs
`REVOKE ALL ON SCHEMA pgokf FROM PUBLIC` and grants `USAGE` only to the two
extension roles. A brand-new login role is a member of neither, so its first
call into the schema fails with SQLSTATE `42501` (`insufficient_privilege`).

The fix is a single GRANT that makes the login role a member of one of the two
roles:

```sql
GRANT pgokf_reader TO app_reader;   -- search + read the catalog
GRANT pgokf_admin  TO app_writer;   -- register/refresh/config (inherits reader)
```

Both `pgokf_reader` and `pgokf_admin` are created `NOLOGIN` — they are privilege
buckets, not accounts. `pgokf_admin` is itself granted `pgokf_reader`, so an
admin can search without a second grant. See [Security](security.md) for the
full model.

### Do I need superuser to install or use the extension?

Installing (`CREATE EXTENSION pgokf`) requires the usual privilege to create an
extension. Day-to-day use does not require superuser: bundle registration,
refresh, search, and configuration all run through the two `pgokf` roles. The
functions that touch the server filesystem (`register_bundle`, `refresh_bundle`,
`export_parquet`, `export_sources`) are `SECURITY DEFINER` with a hardened
`search_path`, and are constrained by the `allowed_roots` policy — see
[Security](security.md) and [Configuration](configuration.md).

### The bundle path is on my laptop but registration fails — why?

The path argument to `register_bundle` is opened by the **PostgreSQL server
process**, not your client. It must be an absolute path the server can reach and
must canonicalize (no `..` traversal, no symlink escape). If `allowed_roots` is
configured, the canonical path must also resolve inside one of those roots. A
path that is not registerable raises a validation error; a path already
registered raises SQLSTATE `23505` (use `refresh_bundle` to re-sync it instead).
See [Deployment topologies](deployment-topologies.md) for how to make bundle
files reachable by the server.

---

## OKF and the catalog model

### What does "OKF v0.2 conformance" actually mean here?

It means the catalog model matches the OKF v0.2 spec's data model:

- Every concept must carry a `type`; `title`, `description`, `resource`, and
  `tags` are recommended and projected as first-class columns on
  `pgokf.concepts`.
- The provenance/trust/lifecycle families — `sources`, `generated` (`by`/`at`),
  `verified[]` events, `status`, `stale_after`, and `usage_window` — are parsed
  and projected into dedicated tables rather than being flattened away.
- Actors follow the OKF convention `<producer>/<version>`, `human:<id>`, or
  `process:<id>` in fields like `generated.by` and `verified[].by`.
- The bundle root's reserved `index.md` supplies the bundle's `okf_version`
  (stored on `pgokf.bundles.okf_version`); `log.md` is likewise reserved.

Producer-defined frontmatter keys that are not part of the standard projection
are not discarded — they are retained per concept in `pgokf.concept_metadata`
as `jsonb`, one row per key, and are indexed for containment queries. See
[OKF authoring](okf-authoring.md).

### How is provenance modeled in the tables?

Provenance is split by cardinality across three tables so each shape stays
queryable:

- **`pgokf.concept_provenance`** — one sparse row per concept carrying the
  scalar generation/trust/lifecycle fields: `generated_by`, `generated_at`,
  `status`, `stale_after`, `usage_window_from/to`, `trust_tier`, plus a lossless
  `details` `jsonb`.
- **`pgokf.concept_verification`** — the `verified[]` event list, one row per
  event (`verified_by`, `verified_at`, `ordinal`).
- **`pgokf.concept_provenance_source`** — the `sources[]` materials a concept
  was derived from, one row per source (`source_id`, `resource`, `title`,
  `author`, `usage_count`, `last_modified`, `usage_window_from/to`).

All three cascade from `pgokf.concepts`, so unregistering a bundle or dropping a
concept removes its provenance automatically. See the [SQL API](sql-api.md) for
column-by-column detail.

### What is an "Attested Computation" concept?

In OKF v0.2, `Attested Computation` is the one concept `type` with
type-specific fields (attesting *how* a computed artifact was produced). pgokf
treats `type` as data — it is a projected column and a search weight, not a
hard-coded enum — so an `Attested Computation` concept is ingested like any
other, with its type-specific frontmatter preserved in `concept_metadata`. See
the [glossary](glossary.md#attested-computation).

### Are reserved files ingested as concepts?

No. The bundle root's `index.md` (which supplies `okf_version`) and `log.md`
are reserved and are not projected as searchable concepts. In the sample bundle,
`register_bundle` reports four concepts because the reserved `index.md` is
skipped. See [OKF authoring](okf-authoring.md).

---

## Identifiers and schema design

### Why are bundle IDs `bigint` instead of UUIDs?

`pgokf.bundles.id` is `bigint GENERATED ALWAYS AS IDENTITY`. Bundle
registration is a **single-writer** operation serialized on a per-path advisory
lock, so there is no ID-collision problem to solve, and a monotonic `bigint`
gives better B-tree **index locality** and smaller foreign keys — every
`concepts`, `links`, `concept_metadata`, and provenance row carries `bundle_id`,
so the width matters. A UUID (e.g. UUIDv7) earns its keep only when many
independent writers or federated catalogs must mint IDs without coordinating;
that is not the current single-catalog model. If federation is later needed, the
surrogate key is the natural seam to change. See [Architecture](architecture.md).

### Concept IDs are strings — where do they come from?

A concept's `id` is derived from its path: the normalized, bundle-relative path
with the `.md` suffix removed (e.g. `runbooks/database-failover`). The primary
key on `pgokf.concepts` is `(bundle_id, id)`, so the same logical path in two
bundles is two distinct concepts. This is why `concept_neighbors` needs a
`bundle_id` to disambiguate when an ID exists in more than one bundle (it raises
SQLSTATE `22023` if you omit it and the ID is ambiguous).

---

## Storage tiers and getting files back

### What are the two storage tiers, and which should I use?

The `store_source` config key selects the tier (default `false`):

- **`store_source = false` (default) — data-lake / enterprise tier.**
  PostgreSQL holds metadata, the search index, the link graph, and provenance.
  The original files stay in their external object store or mounted bucket. Best
  when the files are large or already live in a data lake, and you want
  PostgreSQL to be the query layer, not the store of record.
- **`store_source = true` — small self-contained tier.** Each concept's verbatim
  source bytes are also stored in `pgokf.concept_source` (compressed with lz4
  where the build supports it, else pglz). The database is then a complete,
  portable install that needs no external file store.

`store_source` is **not** retroactive — a change takes effect for bundles synced
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
(BLAKE3). With `store_source = false` these functions have nothing to return —
the files live only in your external store. See the [SQL API](sql-api.md).

---

## Search

### Is search real BM25, or PostgreSQL FTS?

Shipped search is **native PostgreSQL full-text search** — no third-party
extension. `pgokf.concept_search` builds a query with `websearch_to_tsquery` and
ranks with `ts_rank_cd` against a weighted `tsvector` (`body_tsv`): title
weight A, tags/type/description weight B, body weight D. It returns a
`ts_headline` snippet per hit. The `tsvector` is indexed with GIN
(`concepts_body_tsv_gin`).

BM25 is a **benchmarked future adapter**, not a shipped function. The research
(a ParadeDB `pg_search` top-k adapter) lives in
[BM25 research](bm25-research.md); there is no `pgokf` BM25 function today.

### When does native FTS become a problem, and what does BM25 buy?

Measured on this project:

- **Selective queries** — point lookups, tag filters, type filters — stay
  **sub-millisecond to roughly 10-15 ms** even at ~10M concepts, because they
  ride B-tree / GIN indexes.
- **Broad "rank everything" full-text queries** scale **linearly** with corpus
  size: about **322 ms at 1M**, **2.4 s at 10M**, and **29 s at 50M** concepts,
  because `ts_rank_cd` must score every match.
- The benchmarked **BM25 top-k adapter kept broad queries roughly flat at
  ~10-15 ms** (a 30-194x speedup) by pruning with WAND-style top-k instead of
  scoring the whole match set.

So native FTS is the right default for selective queries and moderate corpora;
the future BM25 adapter targets broad top-k queries over very large corpora. See
[Benchmarks](benchmarks.md) and [Search guide](search-guide.md).

### How do I search within one bundle, or limit results?

`concept_search(query, bundle_id => NULL, limit_count => 20)`. Pass a
`bundle_id` to scope the search to one bundle; `limit_count` must be in the range
`1..=500` (otherwise SQLSTATE `22023`). Search only ever touches **enabled**
bundles. For filtering by tag, type, or producer metadata alongside the ranked
query, see [Search guide](search-guide.md).

### Why doesn't my query language config change affect existing rows?

The `default_text_search_config` key (default `pg_catalog.english`) is applied
when a concept's `tsvector` is built at index time and when a search query is
parsed. Changing it affects bundles synced or refreshed **afterward**; existing
rows keep their `tsvector` until `refresh_bundle` re-indexes them. It must name
an installed configuration (validated against `pg_catalog.pg_ts_config`). See
[Configuration](configuration.md).

---

## Interoperability and portability

### Can I export the catalog for use in other tools?

Yes. `pgokf.export_parquet(bundle_id, dest_dir)` snapshots a bundle's projection
to four Parquet files — `concepts.parquet`, `concept_metadata.parquet`,
`links.parquet`, and `concept_provenance.parquet` — in a server-side directory,
returning per-file row counts and total bytes (`pgokf.export_result`). The
output was **verified interoperable with DuckDB** on this project: you can point
DuckDB straight at the files and query them. `export_parquet` is admin-only and
its destination is validated exactly like a bundle root (absolute,
traversal-free, inside `allowed_roots`). See [Operations](operations.md).

### Is the bundle on disk still authoritative after I import it?

Yes — that is the design. The Markdown bundle is the portable source of truth;
the `pgokf` schema is a projection you can rebuild at any time by re-running
`refresh_bundle`. Incremental sync re-parses only files whose BLAKE3 content
hash changed and removes rows for deleted files, so the catalog converges to
whatever is on disk. See [Architecture](architecture.md).

---

## PostgreSQL support and multi-tenancy

### Which PostgreSQL versions are supported?

PostgreSQL **15, 16, 17, 18, and 19** from one codebase (pgrx feature flags
`pg15`–`pg19`; CI builds the matrix). See [Packaging](packaging.md).

### Does pgokf support multi-tenancy or row-level security?

There is no built-in multi-tenant partitioning or row-level security (RLS)
policy shipped in this version. The isolation primitives available today are:

- **Roles** — `pgokf_reader` / `pgokf_admin` gate *which operations* a login
  role can perform, not which rows it sees.
- **Bundles** — a bundle is the natural tenancy boundary; every catalog row
  carries `bundle_id`, and search/graph calls accept a `bundle_id` scope.
- **Standard PostgreSQL** — you can layer your own RLS policies or per-tenant
  databases/schemas on top, since the catalog tables are ordinary relations.

If per-tenant row visibility inside a shared catalog is a hard requirement,
model it with standard PostgreSQL RLS on the `pgokf` tables or with one
database/schema per tenant. See [Security](security.md).

### Is `sync_log_retention_days` doing anything?

Not yet. `sync_log_retention_days` is a durable config key (default `30`,
validated `>= 0`) reserved for sync-log history retention, but it is currently a
**no-op** — no sync-log pruning is wired to it in this version. It is documented
honestly so you are not surprised: setting it changes stored policy but has no
runtime effect today. See [Configuration](configuration.md).

---

## Still stuck?

- Exact function signatures and column lists: [SQL API](sql-api.md)
- Error-to-SQLSTATE mapping with fixes: [Troubleshooting](troubleshooting.md)
- Term definitions: [Glossary](glossary.md)
