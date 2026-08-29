// SPDX-License-Identifier: AGPL-3.0-only
//! Full-text search over the catalog: `pgokf.concept_search`.
//!
//! This module owns the SQL-facing search entry point, its input validation,
//! and the per-call dispatch to a ranked-search **backend**. The two backends
//! live behind the [`crate::catalog::search_backend`] Strategy seam:
//!
//! - **native** (the default) - `PostgreSQL` FTS only, so every supported
//!   server works without additional extensions. Matching uses
//!   `websearch_to_tsquery` over the weighted `body_tsv` column (title `A`,
//!   tags/type/description `B`, body `D`), ranking uses `ts_rank_cd`, and each
//!   hit carries a `ts_headline` snippet.
//! - **bm25** (optional) - Block-Max WAND top-k over a `ParadeDB` `pg_search`
//!   index, selected by the durable `search_backend` configuration key and
//!   reached only through runtime SPI.
//!
//! Whichever backend runs, the concept ID is the deterministic tiebreaker so
//! equal-rank results order stably, and the returned
//! `pgokf.concept_search_result` shape is identical.
//!
//! # Security model
//!
//! `concept_search` deliberately runs with **invoker rights** (no `SECURITY
//! DEFINER`): it only reads tables that `pgokf_reader` already holds
//! `SELECT` on, so escalating to the extension owner would grant nothing and
//! would only widen the attack surface. Row access therefore obeys ordinary
//! `PostgreSQL` permissions, and [`crate::security::authorize_current_user`]
//! adds the role-policy check (`pgokf_reader` or `pgokf_admin`) as defense
//! in depth on top of the `EXECUTE`/`SELECT` grants.

use std::path::Path;

use pgrx::Spi;

use crate::catalog::search_backend::{self, Cursor, SearchRequest};
use crate::catalog::types::SearchHit;
use crate::errors::CatalogError;
use crate::security;

/// Inclusive bounds accepted for `limit_count`.
pub const LIMIT_RANGE: std::ops::RangeInclusive<i32> = 1..=500;

/// Validate `limit_count`, mapping it into the SQL `LIMIT` argument.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error (SQLSTATE
/// `22023`) when the value is outside [`LIMIT_RANGE`].
pub fn validate_limit_count(limit_count: i32) -> Result<i64, CatalogError> {
    if LIMIT_RANGE.contains(&limit_count) {
        Ok(i64::from(limit_count))
    } else {
        Err(CatalogError::invalid_parameter(
            format!(
                "limit_count must be between {} and {}, got {limit_count}",
                LIMIT_RANGE.start(),
                LIMIT_RANGE.end()
            ),
            Path::new(""),
        ))
    }
}

/// Validate the query text: it must contain at least one non-whitespace
/// character.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error (SQLSTATE
/// `22023`) when the query is empty or whitespace-only.
pub fn validate_query(query: &str) -> Result<(), CatalogError> {
    if query.trim().is_empty() {
        Err(CatalogError::invalid_parameter(
            "query must not be empty",
            Path::new(""),
        ))
    } else {
        Ok(())
    }
}

fn spi_error(context: &'static str) -> impl Fn(pgrx::spi::Error) -> CatalogError {
    move |error| CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Resolve the effective `default_text_search_config` for query parsing.
///
/// `concept_search` runs with invoker rights, so it cannot read the
/// administrator-only `pgokf_private.config` table directly. It instead reads
/// the effective value through the reader-granted `SECURITY DEFINER`
/// `pgokf.get_config` function, so query parsing uses the very configuration
/// that indexed the rows.
pub(crate) fn effective_text_search_config() -> Result<String, CatalogError> {
    Spi::get_one::<String>("SELECT pgokf.get_config() ->> 'default_text_search_config'")
        .map_err(spi_error("failed to read text search configuration"))?
        .ok_or_else(|| {
            CatalogError::internal(
                "default_text_search_config is missing from configuration",
                Path::new(""),
            )
        })
}

/// Resolve the effective `search_backend` policy name for this call.
///
/// Read, like [`effective_text_search_config`], through the reader-granted
/// `SECURITY DEFINER` `pgokf.get_config` projection, because `concept_search`
/// runs with invoker rights and cannot read the administrator-only
/// `pgokf_private.config` table directly.
pub(crate) fn effective_search_backend() -> Result<String, CatalogError> {
    Spi::get_one::<String>("SELECT pgokf.get_config() ->> 'search_backend'")
        .map_err(spi_error("failed to read search backend configuration"))?
        .ok_or_else(|| {
            CatalogError::internal(
                "search_backend is missing from configuration",
                Path::new(""),
            )
        })
}

/// The validated, borrow-ready structured filters for one `concept_search`
/// call. An empty `tags` slice is normalized to `None` by [`Filters::new`] so it
/// binds as no filter rather than `'{}'::text[]`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Filters<'a> {
    pub concept_type: Option<&'a str>,
    pub tags: Option<&'a [String]>,
    pub status: Option<&'a str>,
    pub trust_tier: Option<&'a str>,
}

impl<'a> Filters<'a> {
    /// Build the filter set, treating an empty `tags` slice as no tag filter
    /// (`tags @> '{}'` matches every non-NULL `tags` array but excludes untagged
    /// concepts, so an empty request must be a true no-op).
    pub(crate) fn new(
        concept_type: Option<&'a str>,
        tags: Option<&'a [String]>,
        status: Option<&'a str>,
        trust_tier: Option<&'a str>,
    ) -> Self {
        Self {
            concept_type,
            tags: tags.filter(|slice| !slice.is_empty()),
            status,
            trust_tier,
        }
    }
}

/// Authorize, validate, and dispatch one ranked search through the configured
/// backend. Shared by `concept_search` and the hybrid fusion path.
///
/// `after` is the optional keyset cursor: when `Some`, the backend returns the
/// page that continues strictly *after* it in the stable total order. The
/// content more-like-this and hybrid fusion paths pass `None` (they consume a
/// whole ranked list, not a page).
pub(crate) fn run_ranked_search(
    query: &str,
    bundle_id: Option<i64>,
    limit: i64,
    filters: Filters,
    after: Option<&Cursor>,
) -> Result<Vec<SearchHit>, CatalogError> {
    let text_search_config = effective_text_search_config()?;
    let backend = search_backend::select(&effective_search_backend()?);
    backend.search(&SearchRequest {
        query,
        bundle_id,
        limit,
        text_search_config: &text_search_config,
        concept_type: filters.concept_type,
        tags: filters.tags,
        status: filters.status,
        trust_tier: filters.trust_tier,
        after,
    })
}

/// Parse the opaque `after_cursor` JSON into a typed [`Cursor`], or `None` for a
/// first-page request.
///
/// The caller copies the `rank`, `bundle_id`, and `concept_id` of the previous
/// page's last row into a JSON object; this reads them back and binds them as
/// typed parameters (never interpolated). A present-but-malformed cursor - not an
/// object, or missing/ill-typed a field - is rejected with SQLSTATE `22023`
/// rather than silently ignored, so a corrupt cursor never quietly restarts
/// pagination from the first page.
// The rank round-trips real -> JSON number -> f64 here; narrowing back to the
// f32 the `rank` column stores is exact for a value that originated as a real.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn parse_cursor(
    after_cursor: Option<pgrx::JsonB>,
) -> Result<Option<Cursor>, CatalogError> {
    let Some(json) = after_cursor else {
        return Ok(None);
    };
    let cursor_error = || {
        CatalogError::invalid_parameter(
            "after_cursor must be a JSON object with numeric 'rank', integer 'bundle_id', \
             and string 'concept_id' (copy them from the last row of the previous page)",
            Path::new(""),
        )
    };
    let value = json.0;
    let object = value.as_object().ok_or_else(cursor_error)?;
    let rank = object
        .get("rank")
        .ok_or_else(cursor_error)?
        .as_f64()
        .ok_or_else(cursor_error)?;
    let bundle_id = object
        .get("bundle_id")
        .ok_or_else(cursor_error)?
        .as_i64()
        .ok_or_else(cursor_error)?;
    let concept_id = object
        .get("concept_id")
        .ok_or_else(cursor_error)?
        .as_str()
        .ok_or_else(cursor_error)?
        .to_owned();
    Ok(Some(Cursor {
        rank: rank as f32,
        bundle_id,
        concept_id,
    }))
}

fn concept_search_impl(
    query: &str,
    bundle_id: Option<i64>,
    limit_count: i32,
    filters: Filters,
    after: Option<&Cursor>,
) -> Result<Vec<SearchHit>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    validate_query(query)?;
    let limit = validate_limit_count(limit_count)?;
    run_ranked_search(query, bundle_id, limit, filters, after)
}

/// SQL-facing search entry point, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{Filters, concept_search_impl};
    use crate::catalog::types;

    /// Rank catalog concepts against a search query, with optional structured
    /// filters.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Searches
    /// only active bundles (enabled and not retired); pass `bundle_id` to scope
    /// the search to one bundle. `limit_count` must lie in `1..=500` (SQLSTATE
    /// `22023` otherwise).
    ///
    /// The four structured filters are each a no-op when `NULL` (the default),
    /// so the historical three-argument call is unchanged: `concept_type` matches
    /// the concept type exactly, `tags` matches with **ALL-of** containment (a
    /// hit must carry every listed tag), and `status` / `trust_tier` match the
    /// OKF lifecycle status and derived trust tier from `concept_provenance`.
    ///
    /// `after_cursor` is the optional **keyset pagination** cursor (default
    /// `NULL` = first page). Results have a stable total order - `rank DESC`,
    /// then `bundle_id ASC`, then `concept_id ASC` - so a caller copies the
    /// `rank`, `bundle_id`, and `concept_id` of a page's last row into a JSON
    /// object `{"rank":..,"bundle_id":..,"concept_id":..}` and passes it back to
    /// fetch the next page, which continues strictly after that position with no
    /// `OFFSET` drift. A present-but-malformed cursor raises SQLSTATE `22023`.
    ///
    /// The ranking backend follows the durable `search_backend` configuration
    /// key: `native` `PostgreSQL` FTS by default, or `ParadeDB` `pg_search`
    /// BM25 when set to `bm25` (which falls back to native, with a warning, if
    /// `pg_search` or its index is absent). The result shape is identical
    /// either way.
    // `tags` is a `Vec<String>` because that is the SQL `text[]` boundary type;
    // it is only borrowed (`as_deref`) into the filter set, so pass-by-value is
    // inherent to the pgrx signature rather than a smell.
    // Eight SQL arguments is inherent to the backward-compatible signature (the
    // three original inputs, the four structured filters, and the pagination
    // cursor), not a decomposable Rust smell.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    #[pg_extern(stable, parallel_safe, requires = ["catalog_tables", "provenance_table"])]
    fn concept_search(
        query: &str,
        bundle_id: default!(Option<i64>, "NULL"),
        limit_count: default!(i32, 20),
        concept_type: default!(Option<&str>, "NULL"),
        tags: default!(Option<Vec<String>>, "NULL"),
        status: default!(Option<&str>, "NULL"),
        trust_tier: default!(Option<&str>, "NULL"),
        after_cursor: default!(Option<pgrx::JsonB>, "NULL"),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_search_result")> {
        let filters = Filters::new(concept_type, tags.as_deref(), status, trust_tier);
        let after = super::parse_cursor(after_cursor).unwrap_or_else(|error| error.raise());
        let hits = concept_search_impl(query, bundle_id, limit_count, filters, after.as_ref())
            .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = hits
            .into_iter()
            .map(|hit| types::concept_search_result(hit).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text, jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text, jsonb) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search(text, bigint, integer, text, text[], text, text, jsonb) IS
    'Rank catalog concepts. Reader-level; searches active bundles only (enabled AND not retired). Optional structured filters (each a no-op when NULL): concept_type (exact type), tags (ALL-of containment), status and trust_tier (from concept_provenance). Stable total order rank DESC, bundle_id ASC, concept_id ASC; pass after_cursor (a {rank,bundle_id,concept_id} JSON object copied from the previous page''s last row) for OFFSET-free keyset pagination (a malformed cursor raises 22023). Uses the search_backend configuration: native full-text search (websearch_to_tsquery + ts_rank_cd) by default, or ParadeDB pg_search BM25 when set to bm25 (falling back to native if pg_search or its index is absent).';
",
        name = "search_function_hardening",
        requires = [concept_search]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;

    #[test]
    fn validate_limit_count_accepts_the_inclusive_bounds() {
        // Arrange & Act & Assert
        assert_eq!(validate_limit_count(1).expect("lower bound is valid"), 1);
        assert_eq!(
            validate_limit_count(500).expect("upper bound is valid"),
            500
        );
        assert_eq!(validate_limit_count(20).expect("default is valid"), 20);
    }

    #[test]
    fn validate_limit_count_rejects_zero_negative_and_oversized_values() {
        for invalid in [0, -1, 501, i32::MIN, i32::MAX] {
            // Arrange & Act
            let error =
                validate_limit_count(invalid).expect_err("out-of-range limits must be rejected");

            // Assert
            assert_eq!(error.kind(), ErrorKind::InvalidParameter);
            assert_eq!(error.sqlstate(), "22023");
        }
    }

    #[test]
    fn validate_query_rejects_empty_and_whitespace_queries() {
        for invalid in ["", "   ", "\t\n"] {
            // Arrange & Act
            let error = validate_query(invalid).expect_err("blank queries must be rejected");

            // Assert
            assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        }
    }

    #[test]
    fn validate_query_accepts_normal_text() {
        // Arrange & Act & Assert
        assert!(validate_query("postgres indexing").is_ok());
    }

    #[test]
    fn filters_new_normalizes_an_empty_tag_slice_to_no_filter() {
        // Arrange: an empty tags slice must not become `tags @> '{}'` (which
        // would exclude untagged concepts); it is a true no-op instead.
        let empty: Vec<String> = Vec::new();

        // Act
        let filters = Filters::new(None, Some(&empty), None, None);

        // Assert
        assert!(filters.tags.is_none(), "an empty tag filter is dropped");
    }

    #[test]
    fn filters_new_keeps_a_non_empty_tag_slice() {
        // Arrange
        let tags = vec!["widgets".to_owned()];

        // Act
        let filters = Filters::new(Some("Reference"), Some(&tags), Some("stable"), None);

        // Assert
        assert_eq!(filters.concept_type, Some("Reference"));
        assert_eq!(filters.tags.map(<[String]>::len), Some(1));
        assert_eq!(filters.status, Some("stable"));
        assert_eq!(filters.trust_tier, None);
    }
}
