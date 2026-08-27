//! Native full-text search over the catalog: `pgokf.concept_search`.
//!
//! The search contract is `PostgreSQL` FTS only, so every supported server
//! works without additional extensions. Matching uses
//! `websearch_to_tsquery` over the weighted `body_tsv` column (title `A`,
//! tags/type/description `B`, body `D`), ranking uses `ts_rank_cd`, and each
//! hit carries a `ts_headline` snippet computed over the stored title,
//! description, and body text. The concept ID is the deterministic
//! tiebreaker, so equal-rank results order stably.
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

// The text-search regconfig is bound as `$4` (text, cast to `regconfig` in SQL
// so no identifier is interpolated) and drives both `websearch_to_tsquery` and
// `ts_headline`, so query parsing uses the same configuration that built each
// row's `body_tsv` at index time. `ts_rank_cd` takes no configuration.
const SEARCH_QUERY: &str = "
    SELECT c.bundle_id,
           c.id,
           c.path,
           c.title,
           c.type,
           pg_catalog.ts_rank_cd(c.body_tsv, q.query),
           pg_catalog.ts_headline(
               $4::pg_catalog.regconfig,
               pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
               q.query)
    FROM pgokf.concepts c
    JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled,
         pg_catalog.websearch_to_tsquery($4::pg_catalog.regconfig, $1) AS q(query)
    WHERE c.body_tsv @@ q.query
      AND ($2 IS NULL OR c.bundle_id = $2)
    ORDER BY pg_catalog.ts_rank_cd(c.body_tsv, q.query) DESC, c.id ASC
    LIMIT $3";

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
fn effective_text_search_config() -> Result<String, CatalogError> {
    Spi::get_one::<String>("SELECT pgokf.get_config() ->> 'default_text_search_config'")
        .map_err(spi_error("failed to read text search configuration"))?
        .ok_or_else(|| {
            CatalogError::internal(
                "default_text_search_config is missing from configuration",
                Path::new(""),
            )
        })
}

fn concept_search_impl(
    query: &str,
    bundle_id: Option<i64>,
    limit_count: i32,
) -> Result<Vec<SearchHit>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    validate_query(query)?;
    let limit = validate_limit_count(limit_count)?;
    let text_search_config = effective_text_search_config()?;

    Spi::connect(|client| {
        let table = client
            .select(
                SEARCH_QUERY,
                None,
                &[
                    query.into(),
                    bundle_id.into(),
                    limit.into(),
                    text_search_config.as_str().into(),
                ],
            )
            .map_err(spi_error("concept search query failed"))?;
        let mut hits = Vec::with_capacity(table.len());
        for row in table {
            let read = spi_error("failed to read search result row");
            let missing = |column: &str| {
                CatalogError::internal(
                    format!("search result column {column} is unexpectedly NULL"),
                    Path::new(""),
                )
            };
            hits.push(SearchHit {
                bundle_id: row
                    .get::<i64>(1)
                    .map_err(&read)?
                    .ok_or_else(|| missing("bundle_id"))?,
                concept_id: row
                    .get::<String>(2)
                    .map_err(&read)?
                    .ok_or_else(|| missing("id"))?,
                path: row
                    .get::<String>(3)
                    .map_err(&read)?
                    .ok_or_else(|| missing("path"))?,
                title: row.get::<String>(4).map_err(&read)?,
                concept_type: row.get::<String>(5).map_err(&read)?,
                rank: row
                    .get::<f32>(6)
                    .map_err(&read)?
                    .ok_or_else(|| missing("rank"))?,
                headline: row.get::<String>(7).map_err(&read)?,
            });
        }
        Ok(hits)
    })
}

/// SQL-facing search entry point, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::concept_search_impl;
    use crate::catalog::types;

    /// Rank catalog concepts against a `websearch_to_tsquery` query.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Searches
    /// only enabled bundles; pass `bundle_id` to scope the search to one
    /// bundle. `limit_count` must lie in `1..=500` (SQLSTATE `22023`
    /// otherwise).
    #[pg_extern(stable, parallel_safe, requires = ["catalog_tables"])]
    fn concept_search(
        query: &str,
        bundle_id: default!(Option<i64>, "NULL"),
        limit_count: default!(i32, 20),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_search_result")> {
        let hits = concept_search_impl(query, bundle_id, limit_count)
            .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = hits
            .into_iter()
            .map(|hit| types::concept_search_result(hit).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.concept_search(text, bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search(text, bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search(text, bigint, integer) IS
    'Rank catalog concepts with native full-text search (websearch_to_tsquery + ts_rank_cd). Reader-level; searches enabled bundles only.';
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
}
