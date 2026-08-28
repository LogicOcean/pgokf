//! Catalog backbone: base tables, the shared register/refresh sync engine,
//! and native full-text search.
//!
//! # Module layout and extension seams
//!
//! The backbone is deliberately open for extension and closed for
//! modification. Core modules own the schema and the sync loop; feature
//! modules attach through fixed seams and never edit the core:
//!
//! - [`schema`] — the `catalog_tables` SQL block: base tables, indexes, and
//!   the public composite result types. Feature SQL orders itself after the
//!   base schema with `requires = ["catalog_tables"]`.
//! - [`types`] — Rust-side composite-result builders and the
//!   [`types::StagedConcept`] seam payload handed to projection steps.
//! - [`sync`] — the shared register/refresh engine. After concept rows are
//!   staged it invokes the ordered projection seam ([`links::project`], then
//!   [`provenance::project`]) before returning.
//! - [`search`] — `pgokf.concept_search`, which dispatches through the
//!   ranked-search backend seam.
//! - [`search_backend`] — the `SearchBackend` Strategy seam: the native FTS
//!   backend (default) and the optional `ParadeDB` `pg_search` BM25 adapter
//!   reached only through runtime SPI, plus `pgokf.rebuild_search_index`.
//! - [`similar`] — `pgokf.find_similar`, content more-like-this over the seed's
//!   `body_tsv` dispatched through the same `SearchBackend` seam.
//! - [`embedding`] — the optional pgvector semantic/hybrid surface
//!   (`pgokf.concept_embedding`, `set_concept_embedding`,
//!   `concept_search_semantic`, `concept_search_hybrid`,
//!   `rebuild_embedding_index`), reached only through runtime SQL and storing
//!   the vector as `real[]` so `CREATE EXTENSION` needs no pgvector.
//! - [`facets`] — `pgokf.search_facets`, faceted result counts over the same
//!   matching set `concept_search` produces, grouped by a validated facet.
//! - [`search_status`] — `pgokf.search_index_status`, the reader-level jsonb
//!   report of optional-index availability and coverage.
//! - [`schedule`] — the optional `pg_cron` scheduled re-sync adapter
//!   (`pgokf.schedule_refresh`, `unschedule_refresh`), reached only through
//!   runtime SPI and mirroring the `pgvector` / `pg_search` optional-dependency
//!   seam.
//!
//! Feature-extension stubs, each to be filled by a later wave without
//! touching [`sync`]:
//!
//! - [`links`] — link-graph projection (`pgokf.links`), including the typed
//!   attestation edges of Attested Computation concepts.
//! - [`neighbors`] — recursive graph traversal APIs.
//! - [`provenance`] — provenance/trust/lifecycle projection.
//! - [`bundle_log`] — the reserved-`log.md` per-directory activity-log
//!   projection (`pgokf.bundle_log`, `pgokf.list_bundle_log`); the sync engine
//!   reads each `log.md` through the `ByteSource` and projects it without ever
//!   staging it as a concept.
//! - [`iso8601`] — shared defensive ISO 8601 timestamp parsing used by the
//!   provenance and bundle-log projections.
//! - [`source`] — opt-in verbatim source-byte storage (`pgokf.concept_source`)
//!   and retrieval, gated by the `store_source` configuration key.
//! - [`config`] — the `pgokf.allowed_roots` style configuration surface.
//! - [`admin`] — `bundle_info`, `unregister_bundle`, `list_bundles`,
//!   `set_bundle_enabled`.
//! - [`audit`] — the `pgokf_private.sync_log` audit trail and
//!   `pgokf.list_sync_log`; the sync engine appends one row at its successful
//!   tail and prunes to the `sync_log_retention_days` policy. The per-concept
//!   change manifest (`pgokf_private.sync_log_change`, `pgokf.list_sync_changes`)
//!   hangs off each audit row.
//! - [`access`] — the exfiltration/access audit
//!   (`pgokf_private.access_log`, `pgokf.list_access_log`): one row per
//!   content-exporting operation (`export_parquet`, `export_sources`,
//!   `get_concept_source`).
//! - [`dedup`] — `pgokf.duplicate_concepts`, cross-bundle content-duplicate
//!   detection over the stored BLAKE3 `file_hash`.
//! - [`stats`] — reader-level observability: `catalog_stats`, `health`, and
//!   `stale_concepts`.
//! - [`content`] — `pgokf.register_bundle_content`, the mountless
//!   content-ingestion path: it wraps caller-supplied bytes in the sync
//!   engine's `ContentSource` and runs the identical shared pipeline, so a
//!   companion process can stream an object store into the catalog without the
//!   extension performing any network or filesystem I/O.

pub mod access;
pub mod admin;
pub mod audit;
mod batch;
pub mod bundle_log;
pub mod config;
pub mod content;
pub mod dedup;
pub mod embedding;
pub mod export;
pub mod facets;
mod iso8601;
pub mod links;
pub mod neighbors;
pub mod provenance;
pub mod schedule;
pub mod schema;
pub mod search;
pub mod search_backend;
pub mod search_status;
pub mod similar;
pub mod source;
pub(crate) mod spi_read;
pub mod stats;
pub mod sync;
pub mod types;

pub use types::StagedConcept;
