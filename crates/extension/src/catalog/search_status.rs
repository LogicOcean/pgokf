//! Search-index health and coverage: `pgokf.search_index_status`.
//!
//! The two optional ranked-search accelerators — the `ParadeDB` `pg_search`
//! BM25 index and the `pgvector` HNSW embedding index — are invisible from the
//! ordinary catalog: an operator cannot tell from `concept_search` alone whether
//! either is installed, built, and *current* with the concept set. This function
//! reports exactly that in one `jsonb` document: the configured backend, that
//! native FTS is always available, and for each optional index whether its
//! extension is installed, whether the index exists, and how much of the catalog
//! it covers (rows indexed / embedded versus total concepts).
//!
//! # Security model
//!
//! Reader-tier and **invoker rights**. It reads the durable `search_backend` /
//! `embedding_dim` only through the reader-granted `SECURITY DEFINER`
//! `pgokf.get_config` projection (never the admin-only config table directly),
//! probes extension and index presence through the always-readable system
//! catalogs, and counts `pgokf.concepts` / `pgokf.concept_embedding` — both
//! reader-`SELECT`-granted — as the invoker, so row-level security scopes the
//! coverage counts to the session's tenant automatically.

use std::path::Path;

use pgrx::Spi;

use crate::errors::CatalogError;
use crate::security;

/// The status document, assembled in SQL. Kept as a single query so the config
/// read, the presence probes, and the coverage counts are one consistent
/// snapshot. bm25 coverage is all-or-nothing — the index, when present, spans
/// every concept row — so its `indexed_rows` is the total concept count when the
/// index exists and zero otherwise; embedding coverage is the fraction of
/// concepts that carry a stored vector. Coverage percentages are `NULL` when
/// there are no concepts to cover.
const STATUS_DOCUMENT_QUERY: &str = "
    WITH s AS (
        SELECT
            (pgokf.get_config() ->> 'search_backend') AS search_backend,
            (pgokf.get_config() ->> 'embedding_dim')::pg_catalog.int4 AS embedding_dim,
            (SELECT pg_catalog.count(*) FROM pgokf.concepts) AS total_concepts,
            (SELECT pg_catalog.count(*) FROM pgokf.concept_embedding) AS embedded_rows,
            EXISTS (SELECT 1 FROM pg_catalog.pg_extension WHERE extname = 'pg_search')
                AS bm25_available,
            EXISTS (
                SELECT 1
                FROM pg_catalog.pg_index i
                JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
                JOIN pg_catalog.pg_am am ON am.oid = ic.relam
                WHERE i.indrelid = 'pgokf.concepts'::pg_catalog.regclass
                  AND am.amname = 'bm25') AS bm25_index_exists,
            EXISTS (SELECT 1 FROM pg_catalog.pg_extension WHERE extname = 'vector')
                AS pgvector_available,
            EXISTS (
                SELECT 1
                FROM pg_catalog.pg_index i
                JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
                JOIN pg_catalog.pg_am am ON am.oid = ic.relam
                WHERE i.indrelid = 'pgokf.concept_embedding'::pg_catalog.regclass
                  AND am.amname = 'hnsw') AS embedding_index_exists
    )
    SELECT pg_catalog.jsonb_build_object(
        'search_backend', search_backend,
        'native', true,
        'bm25', pg_catalog.jsonb_build_object(
            'available', bm25_available,
            'index_exists', bm25_index_exists,
            'indexed_rows', CASE WHEN bm25_index_exists THEN total_concepts ELSE 0 END,
            'total_rows', total_concepts,
            'coverage_pct', CASE
                WHEN total_concepts = 0 THEN NULL
                WHEN bm25_index_exists THEN 100.0
                ELSE 0.0 END),
        'embedding', pg_catalog.jsonb_build_object(
            'pgvector_available', pgvector_available,
            'index_exists', embedding_index_exists,
            'embedded_rows', embedded_rows,
            'total_concepts', total_concepts,
            'coverage_pct', CASE
                WHEN total_concepts = 0 THEN NULL
                ELSE pg_catalog.round(100.0 * embedded_rows / total_concepts, 2) END,
            'dim', embedding_dim))
    FROM s";

fn search_index_status_impl() -> Result<pgrx::JsonB, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    Spi::get_one::<pgrx::JsonB>(STATUS_DOCUMENT_QUERY)
        .map_err(|error| {
            CatalogError::internal(
                format!("failed to read search index status: {error}"),
                Path::new(""),
            )
        })?
        .ok_or_else(|| {
            CatalogError::internal("search index status query returned no row", Path::new(""))
        })
}

/// SQL-facing search-index status entry point, installed into the `pgokf`
/// schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{extension_sql, pg_extern};

    use super::search_index_status_impl;

    /// Report search-index health and coverage as a `jsonb` document.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). The document
    /// reports `search_backend` (the configured backend), `native` (always
    /// `true`), and two sub-objects: `bm25` (`available` = `pg_search`
    /// installed, `index_exists`, `indexed_rows`, `total_rows`, `coverage_pct`)
    /// and `embedding` (`pgvector_available`, `index_exists`, `embedded_rows`,
    /// `total_concepts`, `coverage_pct`, `dim`). Coverage counts are tenant-
    /// scoped (invoker rights, RLS-filtered); `coverage_pct` is `NULL` when there
    /// are no concepts to cover.
    #[pg_extern(stable, requires = ["embedding_table"])]
    fn search_index_status() -> pgrx::JsonB {
        search_index_status_impl().unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.search_index_status() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.search_index_status() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.search_index_status() IS
    'Search-index health and coverage (jsonb) for operators: search_backend (configured), native (always true), bm25 {available (pg_search installed), index_exists, indexed_rows, total_rows, coverage_pct} and embedding {pgvector_available, index_exists, embedded_rows, total_concepts, coverage_pct, dim}. Reader-level, STABLE, invoker rights; coverage counts are tenant-scoped (RLS-filtered). bm25 coverage is all-or-nothing (the index spans every concept row); embedding coverage is the fraction of concepts with a stored vector.';
",
        name = "search_index_status_hardening",
        requires = [search_index_status]
    );
}
