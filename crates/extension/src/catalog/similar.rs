//! Content "more-like-this" over the catalog: `pgokf.find_similar`.
//!
//! # What it does
//!
//! Given a seed concept, `find_similar` finds the other concepts whose *body
//! content* is most similar — distinct from [`crate::catalog::neighbors`], which
//! walks the authored link graph. It is a lexical more-like-this: it reads the
//! seed's already-built `body_tsv`, takes its most salient lexemes (the highest
//! term frequencies), assembles them into an `OR` query, and runs that query
//! back through the very same ranked-search backend seam
//! ([`crate::catalog::search_backend`]) that powers `pgokf.concept_search`,
//! excluding the seed itself from the results.
//!
//! # Backend reuse (native and BM25)
//!
//! Because the salient terms are dispatched through
//! [`crate::catalog::search::run_ranked_search`], the query honors the durable
//! `search_backend` configuration exactly like `concept_search`:
//!
//! - **native** — the seed's salient terms are matched with
//!   `websearch_to_tsquery` / `ts_rank_cd` over `body_tsv`.
//! - **bm25** — the same salient terms run through the `ParadeDB` `pg_search`
//!   `bm25` index (Block-Max WAND top-k) when it is present; this realizes a
//!   BM25 more-like-this over the seed's salient content, and falls back to the
//!   native path (with a warning) when `pg_search` or its index is absent — the
//!   fallback already lives in [`crate::catalog::search_backend::Bm25Backend`].
//!
//! # Identity
//!
//! The seed is identified by its **text** `concept_id` — the path-derived OKF
//! id used everywhere else in the surface (`concept_search_result.concept_id`,
//! `concept_neighbors`) — not a surrogate integer, because `pgokf.concepts.id`
//! is `text`. Like `concept_neighbors`, an ambiguous id present in more than one
//! bundle requires `bundle_id` to disambiguate (SQLSTATE `22023`).
//!
//! # Security
//!
//! Reader-level and **invoker rights**, mirroring `concept_search`: it reads
//! only tables `pgokf_reader` already holds `SELECT` on, so escalating would
//! grant nothing.

use std::path::Path;

use pgrx::Spi;

use crate::catalog::search::{self, Filters};
use crate::catalog::types::SearchHit;
use crate::errors::CatalogError;
use crate::security;

/// How many of the seed's most frequent lexemes seed the more-like-this query.
///
/// A modest cap keeps the assembled `OR` query bounded regardless of document
/// size while still capturing a concept's salient vocabulary.
const TOP_LEXEMES: i64 = 20;

/// Shortest lexeme length considered salient, so single- and double-character
/// stems (which carry little topical signal) never dominate the query.
const MIN_LEXEME_LEN: i32 = 3;

fn spi_error(context: &'static str) -> impl Fn(pgrx::spi::Error) -> CatalogError {
    move |error| CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Resolve which bundle the seed concept lives in.
///
/// An explicit `bundle_id` is used verbatim. Otherwise the concept id is looked
/// up across bundles: a single match scopes to it, no match yields `None` (an
/// empty result), and multiple matches are rejected with SQLSTATE `22023` so the
/// caller disambiguates — the same contract as `concept_neighbors`.
fn resolve_seed_bundle(
    concept_id: &str,
    bundle_id: Option<i64>,
) -> Result<Option<i64>, CatalogError> {
    if let Some(explicit) = bundle_id {
        return Ok(Some(explicit));
    }

    let bundles = Spi::connect(|client| {
        let table = client
            .select(
                "SELECT DISTINCT bundle_id FROM pgokf.concepts WHERE id = $1 ORDER BY bundle_id",
                None,
                &[concept_id.into()],
            )
            .map_err(spi_error("failed to resolve seed concept bundle"))?;
        let mut ids = Vec::with_capacity(table.len());
        for row in table {
            let id = crate::catalog::spi_read::required_column::<i64>(
                &row,
                1,
                "failed to read seed concept bundle id",
                "seed bundle id is NULL",
            )?;
            ids.push(id);
        }
        Ok::<_, CatalogError>(ids)
    })?;

    match bundles.as_slice() {
        [] => Ok(None),
        [single] => Ok(Some(*single)),
        many => Err(CatalogError::invalid_parameter(
            format!(
                "concept_id '{concept_id}' exists in {} bundles; pass bundle_id to disambiguate",
                many.len()
            ),
            Path::new(""),
        )),
    }
}

/// Read the seed concept's most salient lexemes from its `body_tsv`, ordered by
/// descending term frequency (position count), skipping very short stems.
///
/// `unnest(tsvector)` expands the stored search vector into `(lexeme, positions,
/// weights)` rows; the number of `positions` is the in-document term frequency,
/// so ordering by it surfaces the seed's most representative vocabulary. Reading
/// the pre-built `body_tsv` means no re-tokenization and no dependence on any
/// search extension.
fn seed_lexemes(bundle_id: i64, concept_id: &str) -> Result<Vec<String>, CatalogError> {
    const LEXEME_QUERY: &str = "
        SELECT t.lexeme
        FROM pgokf.concepts c,
             LATERAL pg_catalog.unnest(c.body_tsv) AS t(lexeme, positions, weights)
        WHERE c.bundle_id = $1
          AND c.id = $2
          AND pg_catalog.length(t.lexeme) >= $3
        ORDER BY coalesce(pg_catalog.cardinality(t.positions), 0) DESC, t.lexeme
        LIMIT $4";

    Spi::connect(|client| {
        let table = client
            .select(
                LEXEME_QUERY,
                Some(TOP_LEXEMES),
                &[
                    bundle_id.into(),
                    concept_id.into(),
                    MIN_LEXEME_LEN.into(),
                    TOP_LEXEMES.into(),
                ],
            )
            .map_err(spi_error("failed to read seed lexemes"))?;
        let mut lexemes = Vec::with_capacity(table.len());
        for row in table {
            let lexeme = crate::catalog::spi_read::required_column::<String>(
                &row,
                1,
                "failed to read seed lexeme",
                "seed lexeme is NULL",
            )?;
            lexemes.push(lexeme);
        }
        Ok(lexemes)
    })
}

/// Assemble the seed's salient lexemes into a single `websearch_to_tsquery` `OR`
/// query, so any of them can match (more-like-this semantics rather than the
/// default AND). The lexemes are already normalized word stems; the assembled
/// string is bound as a query **parameter** downstream, never interpolated.
fn build_or_query(lexemes: &[String]) -> String {
    lexemes.join(" or ")
}

/// Core of `find_similar`: authorize, resolve the seed, extract salient lexemes,
/// and run them through the configured backend, dropping the seed itself.
fn find_similar_impl(
    concept_id: &str,
    bundle_id: Option<i64>,
    limit_count: i32,
) -> Result<Vec<SearchHit>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let limit = search::validate_limit_count(limit_count)?;

    let Some(seed_bundle) = resolve_seed_bundle(concept_id, bundle_id)? else {
        return Ok(Vec::new());
    };

    let lexemes = seed_lexemes(seed_bundle, concept_id)?;
    if lexemes.is_empty() {
        return Ok(Vec::new());
    }
    let query = build_or_query(&lexemes);

    // Fetch one extra hit so removing the seed still leaves room for `limit`
    // similar concepts; the seed always matches its own salient terms and ranks
    // highly, so it is reliably within this window.
    let hits = search::run_ranked_search(&query, bundle_id, limit + 1, Filters::default(), None)?;

    let mut similar: Vec<SearchHit> = hits
        .into_iter()
        .filter(|hit| !(hit.bundle_id == seed_bundle && hit.concept_id == concept_id))
        .collect();
    similar.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(similar)
}

/// SQL-facing more-like-this entry point, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::find_similar_impl;
    use crate::catalog::types;

    /// Find the concepts whose content is most similar to a seed concept.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Reads the
    /// seed's `body_tsv`, takes its most salient lexemes, and ranks other
    /// concepts against them through the configured `search_backend` (native
    /// FTS or BM25), excluding the seed itself. This is content similarity, not
    /// the authored link graph (`concept_neighbors`). `limit_count` must lie in
    /// `1..=500` (SQLSTATE `22023`). When `bundle_id` is omitted and the concept
    /// id exists in more than one bundle, the call fails with SQLSTATE `22023`;
    /// pass `bundle_id` to disambiguate. Searches enabled bundles only.
    #[pg_extern(stable, parallel_safe, requires = ["catalog_tables"])]
    fn find_similar(
        concept_id: &str,
        bundle_id: default!(Option<i64>, "NULL"),
        limit_count: default!(i32, 10),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_search_result")> {
        let hits = find_similar_impl(concept_id, bundle_id, limit_count)
            .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = hits
            .into_iter()
            .map(|hit| types::concept_search_result(hit).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.find_similar(text, bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.find_similar(text, bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.find_similar(text, bigint, integer) IS
    'Content more-like-this: rank concepts by similarity to a seed concept''s body_tsv salient lexemes through the configured search_backend (native FTS or BM25), excluding the seed. Distinct from concept_neighbors (the authored link graph). Reader-level, invoker rights; searches enabled bundles only. Raises 22023 on limit_count out of 1..=500 or an ambiguous concept_id (pass bundle_id).';
",
        name = "find_similar_hardening",
        requires = [find_similar]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_or_query_joins_lexemes_with_or() {
        // Arrange
        let lexemes = vec![
            "index".to_owned(),
            "widget".to_owned(),
            "peregrin".to_owned(),
        ];

        // Act
        let query = build_or_query(&lexemes);

        // Assert: an OR query so any salient term can match (more-like-this).
        assert_eq!(query, "index or widget or peregrin");
    }

    #[test]
    fn build_or_query_of_a_single_lexeme_is_the_lexeme() {
        // Arrange & Act
        let query = build_or_query(std::slice::from_ref(&"index".to_owned()));

        // Assert
        assert_eq!(query, "index");
    }
}
