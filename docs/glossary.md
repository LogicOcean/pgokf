# Glossary

Precise definitions of the OKF and pgokf terms used throughout this
documentation. Every `pgokf` object named here exists in the shipped extension;
see the [SQL API](sql-api.md) for exact signatures and column lists.

- [OKF concepts](#okf-concepts)
- [Provenance, trust, and lifecycle](#provenance-trust-and-lifecycle)
- [Multi-tenancy, lifecycle, and history](#multi-tenancy-lifecycle-and-history)
- [pgokf schema objects](#pgokf-schema-objects)
- [Roles](#roles)
- [GUCs](#gucs-run-time-parameters)
- [Configuration keys](#configuration-keys)
- [Search and indexing](#search-and-indexing)
- [Storage and deployment](#storage-and-deployment)

---

## OKF concepts

### OKF (Open Knowledge Format)

A filesystem convention for a knowledge catalog: a directory tree of Markdown
documents with YAML frontmatter that together describe a body of knowledge. This
extension targets **OKF v0.2**. Spec:
[GoogleCloudPlatform/knowledge-catalog](https://github.com/GoogleCloudPlatform/knowledge-catalog).

### Bundle

One OKF catalog: a directory root containing concept documents. pgokf registers
a bundle by its absolute, canonical path and stores one row per bundle in
[`pgokf.bundles`](#pgokfbundles). The path is the bundle's unique identity
(per tenant); for a filesystem bundle the files on disk remain the
authoritative source of truth. A **content-sourced** bundle is instead streamed
in through `register_bundle_content` (keyed on the synthetic path
`content:<name>`) and has no filesystem root.

### Concept

One knowledge document in a bundle: a UTF-8 Markdown file with YAML frontmatter.
Its **concept ID** is derived from its path - the normalized, bundle-relative
path without the `.md` suffix (e.g. `runbooks/database-failover`). pgokf
projects each concept into a row of [`pgokf.concepts`](#pgokfconcepts), keyed
`(bundle_id, id)`.

### Type

The one **required** OKF v0.2 frontmatter field on every concept. pgokf treats
`type` as data - a projected column (`pgokf.concepts.type`, indexed) and a
search weight (B) - not a fixed enum, so any producer-defined type is accepted.

### Frontmatter

The YAML block at the head of a concept file. Recommended fields (`title`,
`description`, `resource`, `tags`) become first-class columns; the
provenance/trust/lifecycle families become dedicated tables; any remaining
producer-defined keys are retained in
[`pgokf.concept_metadata`](#pgokfconcept_metadata) as `jsonb`, one row per key.

### Reserved files (`index.md`, `log.md`)

Two filenames reserved at **every directory level** of a bundle; neither is ever
projected as a searchable concept or counted in `file_count`. The bundle-root
`index.md` carries the bundle's `okf_version`, which pgokf stores on
`pgokf.bundles.okf_version`. Each per-directory `log.md` activity log is parsed
line by line into [`pgokf.bundle_log`](#pgokfbundle_log) (see
[bundle log](#bundle-log-logmd-projection)).

### Bundle log (`log.md` projection)

The projection of a bundle's reserved `log.md` activity logs: one
`pgokf.bundle_log` row per non-blank line, keyed by the containing directory
and a zero-based ordinal, with a leading ISO 8601 timestamp lifted into
`logged_at` and the line text stored losslessly. Replaced wholesale on every
sync; read with `pgokf.list_bundle_log(bundle_id[, directory])`. See
[the authoring guide](okf-authoring.md#logmd-is-projected-as-a-per-directory-activity-log).

### Attested Computation

The one OKF v0.2 concept `type` that defines type-specific fields (it attests
how a computed artifact was produced). pgokf resolves its three
reference-bearing fields (`computation`, `executor`, and `attester`) into
[`pgokf.links`](#pgokflinks) as typed edges (`link_relation =
attestation:computation` / `attestation:executor` / `attestation:attester`)
that `concept_neighbors` traverses like any resolved internal link; its
non-reference fields (`runtime`, `parameters`) are preserved in
`concept_metadata`. See
[the authoring guide](okf-authoring.md#the-one-special-type-attested-computation).

---

## Provenance, trust, and lifecycle

The OKF v0.2 families describing where a concept came from and how much to trust
it. pgokf projects them across three tables (see the [FAQ](faq.md#how-is-provenance-modeled-in-the-tables)).

### `sources` family

The materials a concept was derived from. Projected into
[`pgokf.concept_provenance_source`](#pgokfconcept_provenance_source), one row per
source, capturing `source_id`, `resource`, `title`, `author`, `usage_count`,
`last_modified`, and a usage window.

### `generated` family (`by`, `at`)

Who produced the current content and when. Projected onto
[`pgokf.concept_provenance`](#pgokfconcept_provenance) as `generated_by` (an
[actor](#actor-convention)) and `generated_at` (an ISO 8601 timestamp).

### `verified` events

The `verified[]` event list - attestations that a concept was checked.
Projected into [`pgokf.concept_verification`](#pgokfconcept_verification), one
row per event (`verified_by`, `verified_at`, `ordinal`).

### `status`, `stale_after`, `usage_window` (lifecycle)

Lifecycle signals: a concept's `status`, the time after which it is considered
stale (`stale_after`), and the window during which it is valid to use
(`usage_window`). All three are projected onto
[`pgokf.concept_provenance`](#pgokfconcept_provenance).

### Trust tier

A trust classification for a concept, projected as
`pgokf.concept_provenance.trust_tier` (indexed) for filtering by trustworthiness.
Derived from the `verified[]` actors: `unverified` (no events),
`machine-confirmed` (events but no `human:` actor), or `human-reviewed` (at
least one `human:` actor).

### Actor convention

The OKF format for naming an actor in `generated.by` / `verified[].by`:
`<producer>/<version>` for a tool or agent, `human:<id>` for a person, or
`process:<id>` for an automated process.

---

## Multi-tenancy, lifecycle, and history

### Tenant

The optional isolation scope for a shared catalog. Every projection row carries
a denormalized `tenant_id` (default `'default'`), and opt-in row-level security
keyed on the [`pgokf.tenant`](#gucs-run-time-parameters) session GUC filters
reads to the session's tenant; a session that never sets the GUC sees every row
(backward compatible). Writes are stamped from the session tenant and confined
to it. The GUC is a **scoping selector, not a hard security boundary** against
arbitrary SQL; see [Multi-tenancy](multi-tenancy.md).

### Retirement

The reversible soft-delete window for a bundle. `pgokf.retire_bundle` stamps
`bundles.retired_at` and hides the bundle from search, graph traversal, and the
default `list_bundles` without deleting any rows; `pgokf.unretire_bundle`
restores it; the admin-tier `pgokf.purge_retired(older_than)` hard-deletes
bundles retired longer than the interval. A bundle is **active** only when
`enabled AND retired_at IS NULL`. Distinct from `set_bundle_enabled` (a plain
visibility toggle) and from the immediate, cascading `unregister_bundle`.

### Version history

The opt-in, append-only SCD Type-2 version trail of each concept, recorded in
[`pgokf.concept_history`](#pgokfconcept_history) when the `track_history`
config key is on: one row per version with a validity interval
`[valid_from, valid_to)` and a `change_kind` (`added` / `updated` / `removed`).
Off by default with zero storage cost; bounded by `history_retention_days`. See
[Version history](version-history.md).

### Point-in-time query

Reading the catalog as it stood at an instant. `pgokf.concept_as_of(bundle_id,
concept_id, as_of)` returns the single history version valid at `as_of` (or no
rows if the concept did not exist then); `pgokf.concept_history` returns the
whole timeline, newest first. Both require [version history](#version-history)
to have been recording.

---

## pgokf schema objects

Everything below lives in the non-relocatable **`pgokf`** schema, except the
four administrator-only tables (`config`, `sync_log`, `sync_log_change`,
`access_log`) in the **`pgokf_private`** schema.

### Tables

#### `pgokf.bundles`

One row per registered bundle: `id` (`bigint` identity), `path` (canonical;
unique per tenant), `tenant_id`, `name`, `source_type` (`filesystem` /
`content`), `okf_version`, `file_count`, `last_synced_at`, `sync_hash`
(aggregate BLAKE3 digest of the last sync), `options` (`jsonb`), `enabled`, and
`retired_at`.

#### `pgokf.concepts`

The core projection, one row per `(bundle_id, id)`: `tenant_id`, `path`, `type`,
`title`, `description`, `tags`, `resource`, `body_text`, `file_hash` (BLAKE3),
`modified_at`, `body_tsv` (the search vector), and `indexed_at`.

#### `pgokf.concept_metadata`

Producer-defined frontmatter keys not covered by the standard projection, one
row per `(bundle_id, concept_id, key)` with the value as `jsonb`. GIN-indexed
with `jsonb_path_ops` for containment queries.

#### `pgokf.links`

Directed Markdown links extracted per concept, one row per outgoing link in
source order: `source_id`, `target_id`, `link_text`, `target_path`, `link_kind`
(inline / reference / autolink / email / image), `link_relation` (`reference`,
or `attestation:*` for [Attested Computation](#attested-computation) edges),
`resolved`, `is_external`, and `ordinal`. Only `resolved`, non-external links
are graph edges.

#### `pgokf.concept_provenance`

Sparse scalar provenance/trust/lifecycle projection, one row per concept that
carries any such frontmatter (see the families above), plus a lossless `details`
`jsonb`.

#### `pgokf.concept_verification`

The `verified[]` event list, one row per event.

#### `pgokf.concept_provenance_source`

The `sources[]` materials list, one row per source.

#### `pgokf.concept_source`

Opt-in verbatim source bytes of each concept file (`raw_content` `bytea`,
`byte_size`), populated only when [`store_source`](#store_source) is enabled.
Compressed with lz4 where the build supports it, else pglz.

#### `pgokf.concept_embedding`

Per-concept embedding vectors for semantic search, stored as the builtin
`real[]` (cast to pgvector's `vector` only at query and index time, so the
extension takes no static pgvector dependency). Written by the writer-tier
`set_concept_embedding`; the vector length must equal the `embedding_dim`
config key.

#### `pgokf.bundle_log`

The reserved-`log.md` activity-log projection: one row per parsed entry, keyed
`(bundle_id, directory, ordinal)` with `logged_at` and the lossless `entry`
text. Replaced wholesale on every sync; read via `list_bundle_log`.

#### `pgokf.concept_history`

The opt-in SCD Type-2 version trail: one row per concept version with a
per-concept monotonic `version`, a validity interval `[valid_from, valid_to)`,
a `change_kind`, and a snapshot of the concept core. Cascades from
`pgokf.bundles` (not `concepts`), so a removed concept keeps its history until
the bundle is unregistered. Populated only when `track_history` is on.

#### `pgokf_private.config`

The single-row, cluster-persistent policy table in the private schema. Managed
only through `set_config` / `reset_config`; readable via `get_config`. Holds the
[configuration keys](#configuration-keys).

#### `pgokf_private.sync_log`, `sync_log_change`, `access_log`

The administrator-only audit trails: one `sync_log` row per successful sync or
unregister (read via `list_sync_log`), one `sync_log_change` row per concept a
sync added/updated/removed (read via `list_sync_changes`; cascades from
`sync_log`), and one `access_log` row per content read/export through
`get_concept_source` / `export_parquet` / `export_sources` (read via the
admin-tier `list_access_log`). All three share the `sync_log_retention_days`
retention window.

### Functions

The 39 SQL functions in the `pgokf` schema, grouped by tier (`reader` <
`writer` < `admin`, each inheriting the tier below):

| Function | Role | Purpose |
| -------- | ---- | ------- |
| `version()` | reader | Report the loaded shared-library version. |
| `list_bundles()` | reader | List registered bundles (retired ones excluded). |
| `bundle_info(bundle_id)` | reader | One bundle's administrative view. |
| `concept_search(query, bundle_id, limit_count, concept_type, tags, status, trust_tier, after_cursor)` | reader | Ranked full-text search with structured filters and keyset pagination. `limit_count` ∈ `1..=500` else `22023`. |
| `search_facets(query, bundle_id, facet, concept_type, tags, status, trust_tier)` | reader | Count the matching set grouped by one facet (`type` / `bundle` / `status` / `trust_tier` / `tag`). |
| `search_index_status()` | reader | Backend, optional-index presence, and coverage report. |
| `find_similar(concept_id, bundle_id, limit_count)` | reader | Content more-like-this from a seed concept's salient lexemes. |
| `concept_search_semantic(query_embedding, bundle_id, limit_count)` | reader | Cosine-distance semantic search; requires pgvector (`22023` when absent). |
| `concept_search_hybrid(query, query_embedding, bundle_id, limit_count)` | reader | RRF fusion of lexical + semantic; degrades to lexical without pgvector. |
| `concept_neighbors(concept_id, max_hops, bundle_id)` | reader | Walk the resolved link graph. `max_hops >= 1`; ambiguous ID → `22023`. |
| `concept_history(bundle_id, concept_id, max_rows)` | reader | A concept's version timeline, newest first. |
| `concept_as_of(bundle_id, concept_id, as_of)` | reader | The version valid at an instant (point-in-time). |
| `list_bundle_log(bundle_id, directory, max_rows)` | reader | A bundle's reserved-`log.md` activity-log entries. |
| `get_config()` | reader | Read the current config as `jsonb`. |
| `list_sync_log(bundle_id, max_rows)` | reader | The sync audit trail. |
| `list_sync_changes(sync_id, max_rows)` | reader | Per-concept change manifest of one sync. |
| `catalog_stats()` | reader | Per-bundle counts, sync recency, staleness flag. |
| `health()` | reader | Liveness/readiness `jsonb` document. |
| `stale_concepts(bundle_id, as_of)` | reader | Concepts past their OKF `stale_after`. |
| `duplicate_concepts(bundle_id, min_group)` | reader | Byte-identical concepts grouped by `file_hash`. |
| `get_concept_source(bundle_id, concept_id)` | reader | Return one stored concept's raw bytes (audited). |
| `register_bundle(path, name, options)` | writer | Register and first-sync a filesystem bundle. Duplicate path → SQLSTATE `23505`. |
| `register_bundle_content(name, paths, contents, options)` | writer | Register or re-sync a mountless, content-sourced bundle from streamed bytes. |
| `refresh_bundle(bundle_id)` | writer | Re-sync a filesystem bundle; only changed files re-parsed. Content bundles → `22023`. |
| `unregister_bundle(bundle_id)` | writer | Remove a bundle; projections cascade. Unknown ID → SQLSTATE `22023`. |
| `set_bundle_enabled(bundle_id, enabled)` | writer | Hide/show a bundle in search and traversal, reversibly. |
| `retire_bundle(bundle_id)` | writer | Start the reversible retirement (soft-delete) window. |
| `unretire_bundle(bundle_id)` | writer | Restore a retired bundle. |
| `set_concept_embedding(bundle_id, concept_id, embedding)` | writer | Store one caller-computed embedding (`real[]`); validates length and finiteness. |
| `set_config(key, value)` | admin | Set a durable config key. |
| `reset_config(key)` | admin | Reset one key (or all when `NULL`). |
| `purge_retired(older_than)` | admin | Hard-delete bundles retired longer than the interval. |
| `list_access_log(bundle_id, max_rows)` | admin | The access/exfiltration audit trail. |
| `rebuild_search_index()` | admin | (Re)build the optional BM25 index for the provider `bm25_provider` resolves to (`pg_textsearch` or `pg_search`); no-op without one. |
| `rebuild_embedding_index()` | admin | Build the pgvector HNSW cosine index; logged no-op without pgvector. |
| `schedule_refresh(bundle_id, schedule)` | admin | Register a `pg_cron` re-sync job; `22023` without `pg_cron`. |
| `unschedule_refresh(bundle_id)` | admin | Remove the `pg_cron` job; clean no-op when absent. |
| `export_parquet(bundle_id, dest_dir)` | admin | Snapshot a bundle's projection to Parquet (audited). |
| `export_sources(bundle_id, dest_dir)` | admin | Write a bundle's stored originals back to disk (audited). |

### Composite types

The 14 return types for the functions above (full column lists in the
[SQL API](sql-api.md)):

- **`pgokf.bundle_sync_result`**: per-bucket file counts from `register_bundle`
  / `register_bundle_content` / `refresh_bundle` (`added`, `updated`, `removed`,
  `unchanged`, `total`).
- **`pgokf.concept_search_result`**: one ranked hit (`bundle_id`, `concept_id`,
  `path`, `title`, `type`, `rank`, `headline`).
- **`pgokf.concept_neighbor`**: one reachable concept (`source_id`,
  `neighbor_id`, `hops`, `path` for the route taken, `title`).
- **`pgokf.bundle_info`**: administrative view of a bundle (`id`, `path`,
  `name`, `okf_version`, `file_count`, `last_synced_at`, `enabled`).
- **`pgokf.export_result`**: outcome of `export_parquet` / `export_sources`
  (`dest_dir`, per-file row counts, and `bytes_written`).
- **`pgokf.sync_log_entry`**: one sync audit row from `list_sync_log`.
- **`pgokf.sync_change`**: one per-concept change from `list_sync_changes`.
- **`pgokf.access_log_entry`**: one access-audit row from `list_access_log`.
- **`pgokf.catalog_stat`**: one bundle's statistics from `catalog_stats`
  (including `retired_at`).
- **`pgokf.stale_concept`**: one concept past its `stale_after`.
- **`pgokf.duplicate_group`**: one group of byte-identical concepts.
- **`pgokf.search_facet`**: one `(facet_value, count)` pair from
  `search_facets`.
- **`pgokf.bundle_log_entry`**: one `log.md` entry from `list_bundle_log`.
- **`pgokf.concept_version`**: one history version from `concept_history` /
  `concept_as_of` (`version`, `valid_from`, `valid_to`, `change_kind`, `type`,
  `title`, `description`, `file_hash`).

---

## Roles

All three are cluster-wide `NOLOGIN` roles created by the extension bootstrap,
forming the hierarchy `pgokf_reader` < `pgokf_writer` < `pgokf_admin` (each
tier inheriting the one below). A fresh login role is a member of none and must
be GRANTed one.

### `pgokf_reader`

The read-only API role. May search (lexical, semantic, hybrid, facets,
similarity), walk the graph, read history, logs, stats, and configuration, and
`SELECT` the catalog tables.

### `pgokf_writer`

The ingestion tier, intended for automated pipelines. Adds the bundle
lifecycle (`register_bundle`, `register_bundle_content`, `refresh_bundle`,
`unregister_bundle`, `set_bundle_enabled`, `retire_bundle`,
`unretire_bundle`) and `set_concept_embedding`. Cannot change configuration,
write exports, or read `pgokf_private`. Inherits `pgokf_reader`.

### `pgokf_admin`

The administrative API role. Manages configuration (`set_config` /
`reset_config`), the file-writing exports (`export_parquet`, `export_sources`),
`purge_retired`, the index rebuilds, `pg_cron` scheduling, and the access
audit (`list_access_log`). Inherits `pgokf_writer` (and so `pgokf_reader`).

---

## GUCs (run-time parameters)

Session/server parameters in the `pgokf.*` namespace (see
[Configuration](configuration.md)).

| GUC | Meaning |
| --- | ------- |
| `pgokf.max_file_bytes` | Ceiling for a single bundle file, in bytes. |
| `pgokf.max_bundle_files` | Ceiling on the number of files in one bundle. |
| `pgokf.max_frontmatter_bytes` | Ceiling for a concept's YAML frontmatter, in bytes. |
| `pgokf.max_graph_hops` | Upper bound for `concept_neighbors` traversal depth. |
| `pgokf.log_level` | Logging threshold (default `warning`). |
| `pgokf.tenant` | Per-session tenant selector (`USERSET`, empty default = see all rows). A scoping selector, not a hard security boundary; see [Multi-tenancy](multi-tenancy.md). |

> **GUC vs. configuration key.** GUCs are PostgreSQL run-time parameters
> (per-session or per-server). Configuration keys are durable, cluster-persistent
> policy stored in `pgokf_private.config` and changed only through `set_config`.

---

## Configuration keys

Durable policy in `pgokf_private.config`, managed via `set_config` /
`reset_config` and read via `get_config`.

| Key | Meaning |
| --- | ------- |
| `allowed_roots` | Absolute directory roots a registered/exported path must resolve inside; empty means the interim any-absolute-path policy. |
| `default_text_search_config` | Default text-search configuration for building tsvectors and parsing queries (default `pg_catalog.english`). Not retroactive. |
| `default_strict` | Whether sync rejects malformed files (`true`, the default) instead of skipping them with a warning. |
| `default_exclude` | Default bundle-relative glob patterns excluded from discovery. |
| `store_source` | Whether sync stores verbatim source bytes in `concept_source`. See [`store_source`](#store_source). |
| `search_backend` | `native` (built-in FTS, the default) or `bm25` (route `concept_search` through a BM25 provider - Tiger Data `pg_textsearch` or ParadeDB `pg_search` - when installed). |
| `bm25_provider` | Which BM25 provider the `bm25` backend uses: `auto` (default: `pg_textsearch` when installed, else `pg_search`), `pg_textsearch`, or `pg_search`. |
| `embedding_dim` | Expected embedding length for `set_concept_embedding` and the HNSW index typmod (default 1536). |
| `notify_channel` | When set, a successful sync emits `pg_notify(<channel>, ...)` with the change summary; empty (default) disables it. |
| `okf_version_policy` | `warn` (default) or `reject` for bundles declaring an unsupported OKF `okf_version`. |
| `sync_log_retention_days` | Retention window for the sync, change-manifest, and access audit trails (default 30; `0` keeps forever). Active since 0.1.5. |
| `track_history` | Opt-in switch for the concept [version history](#version-history) (default `false`). |
| `history_retention_days` | Prune closed history versions older than this many days (default `0` = keep indefinitely). |

---

## Search and indexing

### `tsvector`

PostgreSQL's full-text search document type. pgokf builds `pgokf.concepts.body_tsv`
as a **weighted** vector: title (weight A), tags/type/description (B), body text
(D), so a title match outranks a body match.

### GIN index

Generalized Inverted Index. pgokf uses GIN for the search vector
(`concepts_body_tsv_gin`), the tags array (`concepts_tags_gin`), and the metadata
`jsonb` (`concept_metadata_value_gin`, `jsonb_path_ops`).

### `websearch_to_tsquery` / `ts_rank_cd` / `ts_headline`

The stock PostgreSQL FTS primitives `concept_search` uses: `websearch_to_tsquery`
parses a web-style query string, `ts_rank_cd` ranks matches (cover-density,
respecting weights), and `ts_headline` produces the snippet returned as
`headline`.

### BM25

Best Match 25, a ranking function used by inverted-index search engines. In
pgokf, BM25 is an **optional, config-selected search backend** - setting the
durable `search_backend` key to `bm25` routes `pgokf.concept_search` through a
provider's `bm25` index (Tiger Data `pg_textsearch` or ParadeDB `pg_search`,
chosen by `bm25_provider`) when the operator has installed it. It is a backend
mode, **not a standalone function** (there is no `bm25()` function), and it
falls back to native FTS when the provider is absent. See
[Enabling the BM25 backend](search-guide.md#enabling-the-bm25-backend).

### WAND top-k

Weak-AND, a dynamic-pruning algorithm for retrieving the top *k* documents
without scoring the entire match set. It is the kind of top-k pruning the
optional `bm25` search backend relies on (ParadeDB `pg_search` implements
Block-Max WAND; Tiger Data `pg_textsearch` its own block-max scoring) to keep
broad queries roughly flat where native `ts_rank_cd` scales linearly. See
[Enabling the BM25 backend](search-guide.md#enabling-the-bm25-backend).

### Embedding / semantic search

An embedding is a fixed-length numeric vector representing a concept's meaning,
computed by a model **outside** PostgreSQL (e.g. by the `pgokf-embed`
companion) and stored in [`pgokf.concept_embedding`](#pgokfconcept_embedding)
via `set_concept_embedding`. `concept_search_semantic` ranks concepts by
pgvector **cosine distance** to a query embedding; its `rank` is the normalized
cosine similarity. Requires the optional pgvector extension. See
[Semantic and hybrid search](search-guide.md#semantic-and-hybrid-search-optional-pgvector).

### Hybrid search / RRF

`concept_search_hybrid` fuses the lexical result list (through the configured
`search_backend`) with the semantic result list using **Reciprocal Rank Fusion**
(RRF, k = 60): each result contributes `1 / (60 + rank_position)` from each
list, so hits ranked well by either signal surface. Computed entirely in SQL;
degrades to lexical-only with a `WARNING` when pgvector is absent.

### HNSW index

Hierarchical Navigable Small World, pgvector's approximate-nearest-neighbor
index type. `rebuild_embedding_index` builds an HNSW **cosine** index over the
stored embeddings for the configured `embedding_dim`; a logged no-op when
pgvector is absent or the dimension exceeds pgvector's 2000-dim HNSW limit.

### Keyset pagination

`OFFSET`-free paging on `concept_search`. Results have a stable total order
(`rank DESC, bundle_id ASC, concept_id ASC`); copy the last row's `rank`,
`bundle_id`, and `concept_id` into the `after_cursor jsonb` argument and the
next page continues strictly after it, with no drift, duplicates, or skips even
when ranks tie. A malformed cursor raises `22023`. See
[the search guide](search-guide.md#keyset-pagination).

---

## Storage and deployment

### `store_source`

The configuration key selecting the storage tier. `false` (default) = **data-lake
tier**: PostgreSQL holds metadata and search, originals stay in an external
object store / mounted bucket. `true` = **self-contained tier**: verbatim source
bytes also live in PostgreSQL (`concept_source`). Not retroactive - takes effect
on the next sync/refresh.

### Data-lake tier

The default deployment: the bundle files live in a data lake or mounted bucket
(e.g. a MinIO + s3fs bucket mount, which was verified on this project), and
PostgreSQL is the query/search layer over them. See
[Deployment topologies](deployment-topologies.md).

### Mountless ingestion

The enterprise variant with no server-side mount: a companion process (the
shipped `pgokf-ingest`, optionally in `--watch` daemon mode) reads the object
store and streams the collected `(path, bytes)` pairs to
`register_bundle_content` as `pgokf_writer`. Object-store credentials live in
the companion and never reach PostgreSQL; the extension performs no network
I/O. See
[Deployment topologies](deployment-topologies.md#enterprise-tier-mountless-the-ingestion-companion).

### BLAKE3

The cryptographic hash pgokf uses for content identity. Each concept's
`file_hash` and the bundle's aggregate `sync_hash` are BLAKE3 digests; they drive
**incremental sync** (only files whose hash changed are re-parsed) and let stored
source bytes be verified against `concepts.file_hash`.

### Parquet export

`export_parquet` writes a bundle's projection to four Apache Parquet files
(concepts, metadata, links, provenance), verified interoperable with **DuckDB**.
See [Operations](operations.md).

---

## See also

- [SQL API](sql-api.md): exact signatures and column lists
- [Configuration](configuration.md): GUCs and config keys in depth
- [Security](security.md): roles and the SECURITY DEFINER model
- [Multi-tenancy](multi-tenancy.md): the tenant model and its trust caveat
- [Version history](version-history.md): the SCD Type-2 trail in depth
- [FAQ](faq.md): grounded answers to common questions
