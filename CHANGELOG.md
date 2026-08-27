# Changelog

All notable changes to the `pgokf` PostgreSQL extension are documented in this
file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project aims to adhere to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The public API contract and what "a breaking change" means for this extension
are defined in [docs/api-stability.md](docs/api-stability.md).

## [Unreleased]

Nothing yet.

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
- **Link-graph traversal.** `pgokf.concept_neighbors(concept_path, max_hops,
  bundle_id)` walks the resolved link graph outward from a concept.
- **Administration.** `pgokf.list_bundles()` and `pgokf.bundle_info(bundle_id)`
  expose the registered-bundle inventory as the `pgokf.bundle_info` type.
- **Durable configuration.** `pgokf.set_config`, `pgokf.reset_config`, and
  `pgokf.get_config` manage a single, typed, cluster-persistent policy row
  (`allowed_roots`, `default_text_search_config`, `default_strict`,
  `sync_log_retention_days`, `default_exclude`) stored in the
  administrator-only `pgokf_private.config` table.
- **Parquet export.** `pgokf.export_parquet(bundle_id, path)` writes a bundle's
  concept projection to a Parquet file for downstream analytics.
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
