# Glossary

Precise definitions of the OKF and pgokf terms used throughout this
documentation. Every `pgokf` object named here exists in the shipped extension;
see the [SQL API](sql-api.md) for exact signatures and column lists.

- [OKF concepts](#okf-concepts)
- [Provenance, trust, and lifecycle](#provenance-trust-and-lifecycle)
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
[`pgokf.bundles`](#pgokfbundles). The path is the bundle's unique identity; the
files on disk remain the authoritative source of truth.

### Concept

One knowledge document in a bundle: a UTF-8 Markdown file with YAML frontmatter.
Its **concept ID** is derived from its path — the normalized, bundle-relative
path without the `.md` suffix (e.g. `runbooks/database-failover`). pgokf
projects each concept into a row of [`pgokf.concepts`](#pgokfconcepts), keyed
`(bundle_id, id)`.

### Type

The one **required** OKF v0.2 frontmatter field on every concept. pgokf treats
`type` as data — a projected column (`pgokf.concepts.type`, indexed) and a
search weight (B) — not a fixed enum, so any producer-defined type is accepted.

### Frontmatter

The YAML block at the head of a concept file. Recommended fields (`title`,
`description`, `resource`, `tags`) become first-class columns; the
provenance/trust/lifecycle families become dedicated tables; any remaining
producer-defined keys are retained in
[`pgokf.concept_metadata`](#pgokfconcept_metadata) as `jsonb`, one row per key.

### Reserved files (`index.md`, `log.md`)

Two filenames reserved at the **bundle root**. `index.md` carries the bundle's
`okf_version`, which pgokf stores on `pgokf.bundles.okf_version`; `log.md` is
reserved for catalog history. Neither is projected as a searchable concept.

### Attested Computation

The one OKF v0.2 concept `type` that defines type-specific fields (it attests how
a computed artifact was produced). pgokf ingests it like any other concept, with
its type-specific frontmatter preserved in `concept_metadata`.

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

The `verified[]` event list — attestations that a concept was checked.
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

### Actor convention

The OKF format for naming an actor in `generated.by` / `verified[].by`:
`<producer>/<version>` for a tool or agent, `human:<id>` for a person, or
`process:<id>` for an automated process.

---

## pgokf schema objects

Everything below lives in the non-relocatable **`pgokf`** schema, except
`pgokf_private.config`, which lives in the administrator-only **`pgokf_private`**
schema.

### Tables

#### `pgokf.bundles`

One row per registered bundle: `id` (`bigint` identity), `path` (canonical,
unique), `name`, `okf_version`, `file_count`, `last_synced_at`, `sync_hash`
(aggregate BLAKE3 digest of the last sync), `options` (`jsonb`), and `enabled`.

#### `pgokf.concepts`

The core projection, one row per `(bundle_id, id)`: `path`, `type`, `title`,
`description`, `tags`, `resource`, `body_text`, `file_hash` (BLAKE3),
`modified_at`, `body_tsv` (the search vector), and `indexed_at`.

#### `pgokf.concept_metadata`

Producer-defined frontmatter keys not covered by the standard projection, one
row per `(bundle_id, concept_id, key)` with the value as `jsonb`. GIN-indexed
with `jsonb_path_ops` for containment queries.

#### `pgokf.links`

Directed Markdown links extracted per concept, one row per outgoing link in
source order: `source_id`, `target_id`, `link_text`, `target_path`, `link_kind`
(inline / reference / autolink / email / image), `resolved`, `is_external`, and
`ordinal`. Only `resolved`, non-external links are graph edges.

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

#### `pgokf_private.config`

The single-row, cluster-persistent policy table in the private schema. Managed
only through `set_config` / `reset_config`; readable via `get_config`. Holds the
[configuration keys](#configuration-keys).

### Functions

The 14 SQL functions in the `pgokf` schema:

| Function | Role | Purpose |
| -------- | ---- | ------- |
| `version()` | any | Report the loaded shared-library version. |
| `register_bundle(path, name, options)` | admin | Register and first-sync a bundle. Duplicate path → SQLSTATE `23505`. |
| `refresh_bundle(bundle_id)` | admin | Re-sync a registered bundle; only changed files re-parsed. |
| `unregister_bundle(bundle_id)` | admin | Remove a bundle; projections cascade. Unknown ID → SQLSTATE `22023`. |
| `list_bundles()` | reader | List all registered bundles. |
| `bundle_info(bundle_id)` | reader | One bundle's administrative view. |
| `concept_search(query, bundle_id, limit_count)` | reader | Ranked full-text search. `limit_count` ∈ `1..=500` else `22023`. |
| `concept_neighbors(concept_id, max_hops, bundle_id)` | reader | Walk the resolved link graph. `max_hops >= 1`; ambiguous ID → `22023`. |
| `set_config(key, value)` | admin | Set a durable config key. |
| `reset_config(key)` | admin | Reset one key (or all when `NULL`). |
| `get_config()` | reader | Read the current config as `jsonb`. |
| `export_parquet(bundle_id, dest_dir)` | admin | Snapshot a bundle's projection to Parquet. |
| `get_concept_source(bundle_id, concept_id)` | reader | Return one stored concept's raw bytes. |
| `export_sources(bundle_id, dest_dir)` | admin | Write a bundle's stored originals back to disk. |

### Composite types

Return types for the functions above:

- **`pgokf.bundle_sync_result`** — per-bucket file counts from `register_bundle`
  / `refresh_bundle`: `added`, `updated`, `removed`, `unchanged`, `total`.
- **`pgokf.concept_search_result`** — one ranked hit: `concept_id`, `path`,
  `title`, `type`, `rank`, `headline`.
- **`pgokf.concept_neighbor`** — one reachable concept: `neighbor_id`, `hops`,
  `path` (the route taken), `title`.
- **`pgokf.bundle_info`** — administrative view of a bundle: `id`, `path`,
  `name`, `okf_version`, `file_count`, `last_synced_at`, `enabled`.
- **`pgokf.export_result`** — outcome of `export_parquet`: `dest_dir`, per-file
  row counts, and `bytes_written`.

---

## Roles

Both are cluster-wide `NOLOGIN` roles created by the extension bootstrap; a fresh
login role is a member of neither and must be GRANTed one.

### `pgokf_reader`

The read-only API role. May search the catalog and read configuration:
`concept_search`, `concept_neighbors`, `list_bundles`, `bundle_info`,
`get_config`, `get_concept_source`, and `SELECT` on the catalog tables.

### `pgokf_admin`

The administrative API role. May register, refresh, and unregister bundles,
manage configuration, and export. Inherits `pgokf_reader`, so an admin can
search without a separate grant.

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
| `default_strict` | Whether sync rejects malformed files (`true`) instead of skipping them. |
| `default_exclude` | Default bundle-relative glob patterns excluded from discovery. |
| `store_source` | Whether sync stores verbatim source bytes in `concept_source`. See [`store_source`](#store_source). |
| `sync_log_retention_days` | Retention window for sync-log history (default 30, `>= 0`). **Currently a no-op** — reserved, not yet wired. |

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
pgokf, BM25 is an **optional, config-selected search backend** — setting the
durable `search_backend` key to `bm25` routes `pgokf.concept_search` through a
ParadeDB `pg_search` index when the operator has installed it. It is a backend
mode, **not a standalone function** (there is no `bm25()` function), and it
falls back to native FTS when `pg_search` is absent. See
[Enabling the BM25 backend](search-guide.md#enabling-the-bm25-backend).

### WAND top-k

Weak-AND, a dynamic-pruning algorithm for retrieving the top *k* documents
without scoring the entire match set. It is the mechanism the optional `bm25`
search backend uses (via ParadeDB `pg_search`) to keep broad queries roughly
flat where native `ts_rank_cd` scales linearly. See
[Enabling the BM25 backend](search-guide.md#enabling-the-bm25-backend).

---

## Storage and deployment

### `store_source`

The configuration key selecting the storage tier. `false` (default) = **data-lake
tier**: PostgreSQL holds metadata and search, originals stay in an external
object store / mounted bucket. `true` = **self-contained tier**: verbatim source
bytes also live in PostgreSQL (`concept_source`). Not retroactive — takes effect
on the next sync/refresh.

### Data-lake tier

The default deployment: the bundle files live in a data lake or mounted bucket
(e.g. a MinIO + s3fs bucket mount, which was verified on this project), and
PostgreSQL is the query/search layer over them. See
[Deployment topologies](deployment-topologies.md).

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

- [SQL API](sql-api.md) — exact signatures and column lists
- [Configuration](configuration.md) — GUCs and config keys in depth
- [Security](security.md) — roles and the SECURITY DEFINER model
- [FAQ](faq.md) — grounded answers to common questions
