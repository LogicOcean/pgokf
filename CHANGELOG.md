# Changelog

All notable changes to the `pgokf` PostgreSQL extension are documented in this
file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The public API contract and what "a breaking change" means for this extension
are defined in [docs/api-stability.md](docs/api-stability.md).

## [Unreleased]

Nothing yet.

## [0.1.3] - 2026-08-28

OKF v0.2 conformance re-model of the provenance / trust / lifecycle projection,
and population of `pgokf.bundles.okf_version`. This is a **breaking change** to
the `pgokf.concept_provenance` shape; because the extension is pre-release (no
tagged release, no external installs), the schema is changed in place with no
compatibility shim.

### Changed

- **`pgokf.concept_provenance` re-modeled to OKF v0.2 (breaking).** The invented,
  non-OKF columns `verified` (a flattened bool), `verification_method`, and
  `freshness` are removed. The table now carries the real OKF v0.2 fields:
  `generated_by` (`generated.by`), `generated_at` (`generated.at`), `status`
  (LIFECYCLE `status`), `stale_after`, `usage_window_from` / `usage_window_to`
  (top-level `usage_window`), and a `trust_tier` **derived** from the
  verification actors (`unverified` → `machine-confirmed` → `human-reviewed`).
  Timestamps are ISO 8601, parsed defensively (a malformed instant projects
  `NULL`, never aborting the sync); the recognized key subset is kept losslessly
  in `details`. The index on `verified` is replaced by an index on `trust_tier`.
- **`pgokf.bundles.okf_version` is now populated.** The sync engine reads the
  optional `okf_version` from the reserved bundle-root `index.md` frontmatter
  (string or number, e.g. `0.2`) and stores it; an absent or malformed value
  leaves the column `NULL`. It surfaces unchanged through `bundle_info` /
  `list_bundles`.

### Added

- **`pgokf.concept_verification` table** — the ordered OKF `verified[]` event
  list, one `(bundle_id, concept_id, ordinal)` row per `{by, at}` event (a single
  `verified` mapping is stored as one `ordinal = 0` row; actorless events are
  skipped). Cascades from `pgokf.concepts`; reader-`SELECT`able.
- **`pgokf.concept_provenance_source` table** — the OKF `sources[]` provenance
  materials, one row per entry (`source_id`, `resource`, `title`, `author`,
  `usage_count`, `last_modified`, per-source `usage_window_from` / `_to`).
  Distinct from the raw-bytes `pgokf.concept_source`. Cascades from
  `pgokf.concepts`; reader-`SELECT`able.

### Upgrade

- No supported in-place upgrade from `0.1.2`: this pre-release drops and
  re-creates the provenance projection. Re-`CREATE EXTENSION` and re-register
  bundles; because the on-disk bundle is the source of truth, the projection is
  fully rebuilt from a sync.

## [0.1.2] - 2026-08-27

Additive, opt-in raw source storage. Default behavior is unchanged: the new
`store_source` policy is **off by default**, so an install that never enables it
is byte-for-byte identical to 0.1.1.

### Added

- **`store_source` configuration key** (boolean, default `false`) on
  `pgokf_private.config`. It selects between two deployment tiers: `true` stores
  each concept's verbatim source bytes in PostgreSQL (small, self-contained
  install — no external storage needed); `false` keeps the source in a mounted
  object store / data lake and PostgreSQL holds only metadata and search. Like
  `default_text_search_config`, it is read at sync time and is **not
  retroactive** — set it before the first `register_bundle`, or re-register.
- **`pgokf.concept_source` table** — opt-in verbatim source bytes
  (`raw_content bytea`, `byte_size integer`), keyed `(bundle_id, concept_id)` and
  cascading from `pgokf.concepts`, so removals and unregistration drop the stored
  source automatically. TOAST-compressed with `lz4` where the build supports it,
  otherwise `pglz`. Reader-`SELECT`able.
- **`pgokf.get_concept_source(bundle_id, concept_id) → bytea`** — reader-level
  retrieval of a concept's exact stored bytes to the client (no filesystem
  write). Raises `22023` when the concept exists but no source was stored, and,
  distinctly, when no such concept exists.
- **`pgokf.export_sources(bundle_id, dest_dir) → pgokf.export_result`** —
  admin-only reconstruction of a bundle's stored source files on disk,
  byte-for-byte. Reuses `export_parquet`'s destination validation and
  `O_NOFOLLOW` file creation, recreates the bundle-relative directory tree, and
  verifies each written file against the concept's BLAKE3 `file_hash`.

### Changed

- The sync engine now persists source bytes into `pgokf.concept_source` when
  `store_source` is enabled, projected inside the same atomic, advisory-locked
  transaction as links and provenance (no change when the key is off).

### Upgrade

- `ALTER EXTENSION pgokf UPDATE TO '0.1.2'` brings a 0.1.1 install fully to 0.1.2
  with no data loss: it adds the `store_source` column (default `false`), the
  `concept_source` table, and the two new functions, and touches no existing
  object.

## [0.1.0] - 2026-08-27

The first tagged release: a complete, transactional PostgreSQL catalog for
Open Knowledge Format (OKF) bundles. The bundle on disk stays the portable
source of truth; PostgreSQL becomes a projection optimized for metadata
queries, native full-text search, and link-graph traversal.

### Added

- **Bundle registration and sync.** `pgokf.register_bundle(path, name, options)`
  ingests an OKF bundle root and `pgokf.refresh_bundle(bundle_id)` incrementally
  re-synchronizes it, re-parsing only files whose BLAKE3 content hash changed
  and removing rows for deleted files. `pgokf.unregister_bundle(bundle_id)`
  removes a bundle; concepts, metadata, links, and provenance cascade.
- **Catalog projection.** Base tables `pgokf.bundles`, `pgokf.concepts`, and
  `pgokf.concept_metadata`, plus the feature projections `pgokf.links`
  (concept-to-concept link graph) and `pgokf.concept_provenance` (generation
  and verification lineage).
- **Full-text search.** `pgokf.concept_search(query, bundle_id, limit)` returns
  ranked hits with `ts_headline` snippets over a weighted `tsvector` (title A,
  tags/type/description B, body D). Native PostgreSQL FTS only — no third-party
  search extension is required.
- **Link-graph traversal.** `pgokf.concept_neighbors(concept_id, max_hops,
  bundle_id)` walks the resolved link graph outward from a concept.
- **Administration.** `pgokf.list_bundles()` and `pgokf.bundle_info(bundle_id)`
  expose the registered-bundle inventory as the `pgokf.bundle_info` type.
- **Durable configuration.** `pgokf.set_config`, `pgokf.reset_config`, and
  `pgokf.get_config` manage a single, typed, cluster-persistent policy row
  (`allowed_roots`, `default_text_search_config`, `default_strict`,
  `sync_log_retention_days`, `default_exclude`) stored in the
  administrator-only `pgokf_private.config` table.
- **Parquet export.** `pgokf.export_parquet(bundle_id, dest_dir)` writes a
  bundle's catalog projection to four Parquet files — `concepts.parquet`,
  `concept_metadata.parquet`, `links.parquet`, and `concept_provenance.parquet`
  — inside `dest_dir` for downstream analytics.
- **Version introspection.** `pgokf.version()` reports the loaded shared
  library's version for post-upgrade agreement checks.
- **Composite result types.** `pgokf.bundle_sync_result`,
  `pgokf.concept_search_result`, `pgokf.concept_neighbor`, `pgokf.bundle_info`,
  and `pgokf.export_result`.
- **Security model.** Two cluster roles, `pgokf_reader` (search and read
  configuration) and `pgokf_admin` (register/refresh/unregister and manage
  configuration, inherits `pgokf_reader`). Every mutating function is
  `SECURITY DEFINER` with a pinned `search_path`, `EXECUTE` is revoked from
  `PUBLIC` and granted only to the appropriate role, and bundle paths are
  validated (absolute, traversal-free, canonicalized, optionally confined to
  configured `allowed_roots`) before the server reads any file. The private
  `pgokf_private` schema is internal state, not API.
- **Configurable safety limits (GUCs).** `pgokf.max_file_bytes`,
  `pgokf.max_bundle_files`, `pgokf.max_frontmatter_bytes`,
  `pgokf.max_graph_hops`, and `pgokf.log_level`.
- **Documentation coverage.** Every public object — all 12 functions, all 5
  composite types, all 6 catalog tables, and both API roles — carries a
  `COMMENT ON`, enforced by the `api_stability` test suite and by a runtime
  `obj_description` coverage gate in the release checklist.
- **PostgreSQL 15–19 support**, built with Rust (edition 2024) and pgrx 0.19.
- **Extension upgrade mechanism.** A documented, forward-compatible example
  upgrade path (`pgokf--0.1.0--0.1.1.sql`) exercises
  `ALTER EXTENSION pgokf UPDATE` with a proven no-data-loss guarantee.

### Security

- Path traversal, symlink escape, NUL-byte, and relative-path inputs to
  `register_bundle` are rejected before any filesystem access.
- The `pgokf_private` schema and its `config` table are readable and writable
  only by the extension owner and `pgokf_admin`; readers cannot see policy.

[Unreleased]: https://github.com/LogicOcean/okf-pg-catalog/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/LogicOcean/okf-pg-catalog/releases/tag/v0.1.0
