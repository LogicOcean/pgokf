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
//! Since 0.3.0 the module also hosts the additive freshness-aware variant
//! `pgokf.concept_search_fresh` (same contract plus a `freshness` filter and a
//! per-hit freshness annotation; see [`concept_search_fresh_impl`]); the
//! original signature and result type are unchanged.
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
/// call. An empty `tags` or `concept_types` slice is normalized to `None` by
/// [`Filters::new`] so it binds as no filter rather than `'{}'::text[]`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Filters<'a> {
    pub concept_type: Option<&'a str>,
    pub tags: Option<&'a [String]>,
    pub status: Option<&'a str>,
    pub trust_tier: Option<&'a str>,
    /// Optional type-membership filter (a hit's `type` must be in the list).
    /// Only the hybrid fusion path sets it: `concept_search` and
    /// `concept_search_fresh` expose the single-value `concept_type` instead.
    pub concept_types: Option<&'a [String]>,
}

impl<'a> Filters<'a> {
    /// Build the filter set, treating an empty `tags` or `concept_types` slice
    /// as no filter (`tags @> '{}'` matches every non-NULL `tags` array but
    /// excludes untagged concepts, and no `type` value is a member of `'{}'`,
    /// so an empty request must be a true no-op).
    pub(crate) fn new(
        concept_type: Option<&'a str>,
        tags: Option<&'a [String]>,
        status: Option<&'a str>,
        trust_tier: Option<&'a str>,
        concept_types: Option<&'a [String]>,
    ) -> Self {
        Self {
            concept_type,
            tags: tags.filter(|slice| !slice.is_empty()),
            status,
            trust_tier,
            concept_types: concept_types.filter(|slice| !slice.is_empty()),
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
        concept_types: filters.concept_types,
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

// ---------------------------------------------------------------------
// Freshness-aware variant (additive, 0.3.0): `pgokf.concept_search_fresh`.
// ---------------------------------------------------------------------

/// The accepted `freshness` filter values of `concept_search_fresh`.
const FRESHNESS_FILTERS: [&str; 3] = ["any", "fresh", "stale"];

/// Validate the `freshness` filter: `any` (the default) is no filter and binds
/// `NULL`; `fresh` and `stale` filter the result set. SQLSTATE `22023`
/// otherwise.
fn validate_freshness_filter(freshness: &str) -> Result<Option<&str>, CatalogError> {
    match freshness {
        "any" => Ok(None),
        "fresh" => Ok(Some("fresh")),
        "stale" => Ok(Some("stale")),
        other => Err(CatalogError::invalid_parameter(
            format!(
                "freshness must be one of {}, got {other}",
                FRESHNESS_FILTERS
                    .map(|filter| format!("'{filter}'"))
                    .join(", ")
            ),
            Path::new(""),
        )),
    }
}

/// One ranked hit with its effective freshness annotation, as
/// `pgokf.concept_search_fresh_result`.
struct FreshSearchHit {
    hit: SearchHit,
    freshness_state: String,
    freshness_reasons: Vec<String>,
    freshness_scope: String,
    stale_since: Option<pgrx::datum::TimestampWithTimeZone>,
    observed_revision: Option<String>,
    indexed_revision: Option<String>,
    published_revision: Option<String>,
    catalog_generation: i64,
    last_reconciled_at: Option<pgrx::datum::TimestampWithTimeZone>,
    embedding_state: String,
    embedding_model: Option<String>,
    embedding_dim: Option<i32>,
    embedding_input_hash: Option<String>,
    embedded_at: Option<pgrx::datum::TimestampWithTimeZone>,
}

/// The freshness-aware search statement: the native FTS hit subquery,
/// annotated with the effective freshness (concept > path > bundle override
/// precedence) inside, filtered and limited there, then annotated with the
/// embedding provenance in the outer projection. `contract_match` is
/// [`crate::catalog::embedding::contract_match_sql`] with the policy bound as
/// `$14` (model), `$15` (dimension), and `$16` (contract). `$12` is the
/// type-membership filter the shared hit subquery declares (always NULL here:
/// this variant exposes the single-value `concept_type` only), `$13` the
/// freshness filter.
fn fresh_search_statement(contract_match: &str) -> String {
    format!(
        "
    SELECT ranked.bundle_id,
           ranked.concept_id,
           ranked.path,
           ranked.title,
           ranked.type,
           ranked.rank,
           ranked.headline,
           ranked.freshness_state,
           ranked.freshness_reasons,
           ranked.freshness_scope,
           ranked.stale_since,
           ranked.observed_revision,
           ranked.indexed_revision,
           ranked.published_revision,
           ranked.catalog_generation,
           ranked.last_reconciled_at,
           CASE WHEN e.concept_id IS NULL THEN 'missing'
                WHEN ranked.freshness_state = 'fresh' AND {contract_match} THEN 'current'
                ELSE 'stale' END AS embedding_state,
           e.model AS embedding_model,
           e.dim AS embedding_dim,
           e.input_hash AS embedding_input_hash,
           e.updated_at AS embedded_at
    FROM (
    SELECT hits.bundle_id,
           hits.concept_id,
           hits.path,
           hits.title,
           hits.type,
           hits.rank,
           hits.headline,
           COALESCE(fc.state, fp.state, fb.state, 'fresh') AS freshness_state,
           COALESCE(fc.reasons, fp.reasons, fb.reasons, '{{}}'::text[]) AS freshness_reasons,
           COALESCE(fc.scope_kind || ':' || fc.scope_key,
                    fp.scope_kind || ':' || fp.scope_key,
                    'bundle') AS freshness_scope,
           COALESCE(fc.stale_since, fp.stale_since, fb.stale_since) AS stale_since,
           COALESCE(fc.observed_revision, fp.observed_revision, fb.observed_revision)
               AS observed_revision,
           COALESCE(fc.indexed_revision, fp.indexed_revision, fb.indexed_revision)
               AS indexed_revision,
           COALESCE(fc.published_revision, fp.published_revision, fb.published_revision)
               AS published_revision,
           b.catalog_generation,
           COALESCE(fc.last_reconciled_at, fp.last_reconciled_at, fb.last_reconciled_at)
               AS last_reconciled_at
    FROM ({hits}) AS hits
    JOIN pgokf.bundles b ON b.id = hits.bundle_id
    LEFT JOIN pgokf.effective_freshness fc
           ON fc.bundle_id = hits.bundle_id
          AND fc.scope_kind = 'concept' AND fc.scope_key = hits.concept_id
    LEFT JOIN pgokf.effective_freshness fp
           ON fp.bundle_id = hits.bundle_id
          AND fp.scope_kind = 'path' AND fp.scope_key = hits.path
    LEFT JOIN pgokf.effective_freshness fb
           ON fb.bundle_id = hits.bundle_id AND fb.scope_kind = 'bundle'
    WHERE ($13::text IS NULL
           OR COALESCE(fc.state, fp.state, fb.state, 'fresh') = $13)
      AND {keyset}
    ORDER BY hits.rank DESC, hits.bundle_id ASC, hits.concept_id ASC
    LIMIT $3
    ) AS ranked
    JOIN pgokf.concepts c ON c.bundle_id = ranked.bundle_id AND c.id = ranked.concept_id
    LEFT JOIN pgokf.concept_embedding e
           ON e.bundle_id = ranked.bundle_id AND e.concept_id = ranked.concept_id
    ORDER BY ranked.rank DESC, ranked.bundle_id ASC, ranked.concept_id ASC",
        hits = search_backend::NATIVE_HITS_QUERY,
        keyset = search_backend::KEYSET_PREDICATE,
    )
}

/// Read one `pgokf.concept_search_fresh_result`-shaped row.
fn read_fresh_hit(row: &pgrx::spi::SpiHeapTupleData<'_>) -> Result<FreshSearchHit, CatalogError> {
    let reader = crate::catalog::spi_read::RowReader::new(
        row,
        "failed to read freshness-aware search row",
        "concept_search_fresh_result",
    );
    Ok(FreshSearchHit {
        hit: SearchHit {
            bundle_id: reader.required(1, "bundle_id")?,
            concept_id: reader.required(2, "concept_id")?,
            path: reader.required(3, "path")?,
            title: reader.optional(4)?,
            concept_type: reader.optional(5)?,
            rank: reader.required(6, "rank")?,
            headline: reader.optional(7)?,
        },
        freshness_state: reader.required(8, "freshness_state")?,
        freshness_reasons: reader.required(9, "freshness_reasons")?,
        freshness_scope: reader.required(10, "freshness_scope")?,
        stale_since: reader.optional(11)?,
        observed_revision: reader.optional(12)?,
        indexed_revision: reader.optional(13)?,
        published_revision: reader.optional(14)?,
        catalog_generation: reader.required(15, "catalog_generation")?,
        last_reconciled_at: reader.optional(16)?,
        embedding_state: reader.required(17, "embedding_state")?,
        embedding_model: reader.optional(18)?,
        embedding_dim: reader.optional(19)?,
        embedding_input_hash: reader.optional(20)?,
        embedded_at: reader.optional(21)?,
    })
}

/// Authorize, validate, and run the freshness-aware search.
///
/// The ranked candidate set is the **native FTS pipeline's** hit subquery
/// ([`search_backend::NATIVE_HITS_QUERY`], shared verbatim so the match/rank/
/// filter semantics never fork); each hit is then annotated from
/// `pgokf.effective_freshness` with the precedence concept override, then path
/// override, then bundle state (a concept with no recorded row is `fresh`).
/// The `freshness` filter applies inside the query - before the keyset
/// predicate and `LIMIT` - so a filtered page is a true page of the filtered
/// set, never a truncated unfiltered one.
///
/// Every hit also carries its embedding provenance, annotated in an outer
/// projection over the (already limited) ranked rows: `embedding_state` is
/// `missing` (no embedding row), `current` (the row satisfies the semantic
/// eligibility predicate of [`crate::catalog::embedding`] - physical contract
/// match plus an effectively fresh concept), or `stale` (a physical row that
/// is not eligible); model, dimension, input hash, and `embedded_at` (the
/// row's `updated_at`) are the stored provenance values, NULL when no row
/// exists.
///
/// The function runs with invoker rights like [`concept_search`]: the
/// `effective_freshness` view is the one reader-granted freshness surface and
/// applies the opt-in tenant predicate inline, so a reader sees exactly its
/// own tenant's states. Composition with the optional BM25 backend is
/// deferred; this variant always ranks with the native FTS pipeline.
fn concept_search_fresh_impl(
    query: &str,
    bundle_id: Option<i64>,
    limit_count: i32,
    freshness: &str,
    filters: Filters,
    after: Option<&Cursor>,
) -> Result<Vec<FreshSearchHit>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    validate_query(query)?;
    let limit = validate_limit_count(limit_count)?;
    let freshness = validate_freshness_filter(freshness)?;
    let text_search_config = effective_text_search_config()?;
    let embedding_policy = crate::catalog::embedding::effective_embedding_policy()?;
    let statement =
        fresh_search_statement(&crate::catalog::embedding::contract_match_sql(14, 15, 16));

    Spi::connect(|client| {
        let table = client
            .select(
                statement.as_str(),
                None,
                &[
                    query.into(),
                    bundle_id.into(),
                    limit.into(),
                    text_search_config.into(),
                    filters.concept_type.into(),
                    filters.tags.map(<[String]>::to_vec).into(),
                    filters.status.into(),
                    filters.trust_tier.into(),
                    after.map(|cursor| cursor.rank).into(),
                    after.map(|cursor| cursor.bundle_id).into(),
                    after.map(|cursor| cursor.concept_id.as_str()).into(),
                    filters.concept_types.map(<[String]>::to_vec).into(),
                    freshness.into(),
                    embedding_policy.model.clone().into(),
                    embedding_policy.dim.into(),
                    embedding_policy.contract.clone().into(),
                ],
            )
            .map_err(spi_error("freshness-aware search query failed"))?;
        let mut hits = Vec::with_capacity(table.len());
        for row in table {
            hits.push(read_fresh_hit(&row)?);
        }
        Ok(hits)
    })
}

/// Pack a [`FreshSearchHit`] into a `pgokf.concept_search_fresh_result` heap
/// tuple.
fn fresh_composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build pgokf.concept_search_fresh_result composite: {error}"),
        Path::new(""),
    )
}

fn fresh_search_result(
    hit: FreshSearchHit,
) -> Result<pgrx::heap_tuple::PgHeapTuple<'static, pgrx::AllocatedByRust>, CatalogError> {
    let mut tuple =
        pgrx::heap_tuple::PgHeapTuple::new_composite_type("pgokf.concept_search_fresh_result")
            .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("bundle_id", hit.hit.bundle_id)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("concept_id", hit.hit.concept_id)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("path", hit.hit.path)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("title", hit.hit.title)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("type", hit.hit.concept_type)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("rank", hit.hit.rank)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("headline", hit.hit.headline)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("freshness_state", hit.freshness_state)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("freshness_reasons", hit.freshness_reasons)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("freshness_scope", hit.freshness_scope)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("stale_since", hit.stale_since)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("observed_revision", hit.observed_revision)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("indexed_revision", hit.indexed_revision)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("published_revision", hit.published_revision)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("catalog_generation", hit.catalog_generation)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("last_reconciled_at", hit.last_reconciled_at)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("embedding_state", hit.embedding_state.clone())
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("embedding_model", hit.embedding_model.clone())
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("embedding_dim", hit.embedding_dim)
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("embedding_input_hash", hit.embedding_input_hash.clone())
        .map_err(fresh_composite_error)?;
    tuple
        .set_by_name("embedded_at", hit.embedded_at)
        .map_err(fresh_composite_error)?;
    Ok(tuple)
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
    // PARALLEL RESTRICTED (leader only): the bm25 backend's provider scoring
    // is declared PARALLEL UNSAFE upstream (pg_textsearch attaches per-backend
    // shared state that a freshly started worker lacks), and the backend runs
    // it through SPI inside this function. Restricting the function keeps a
    // parallel outer plan from executing that scoring in a worker.
    #[pg_extern(stable, parallel_restricted, requires = ["catalog_tables", "provenance_table"])]
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
        let filters = Filters::new(concept_type, tags.as_deref(), status, trust_tier, None);
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
    'Rank catalog concepts. Reader-level; searches active bundles only (enabled AND not retired). Optional structured filters (each a no-op when NULL): concept_type (exact type), tags (ALL-of containment), status and trust_tier (from concept_provenance). Stable total order rank DESC, bundle_id ASC, concept_id ASC; pass after_cursor (a {rank,bundle_id,concept_id} JSON object copied from the previous page''s last row) for OFFSET-free keyset pagination (a malformed cursor raises 22023). Uses the search_backend configuration: native full-text search (websearch_to_tsquery + ts_rank_cd) by default, or BM25 top-k through the provider the bm25_provider policy resolves to (Tiger Data pg_textsearch or ParadeDB pg_search) when set to bm25, falling back to native if the provider or its index is absent.';
",
        name = "search_function_hardening",
        requires = [concept_search]
    );

    extension_sql!(
        r"
CREATE TYPE pgokf.concept_search_fresh_result AS (
    bundle_id          bigint,
    concept_id         text,
    path               text,
    title              text,
    type               text,
    rank               real,
    headline           text,
    freshness_state    text,
    freshness_reasons  text[],
    freshness_scope    text,
    stale_since        timestamptz,
    observed_revision  text,
    indexed_revision   text,
    published_revision text,
    catalog_generation bigint,
    last_reconciled_at timestamptz,
    -- The embedding provenance columns are appended last so a fresh install
    -- matches, attribute-for-attribute, an existing install upgraded via
    -- ALTER TYPE ... ADD ATTRIBUTE (see sql/pgokf--0.2.0--0.3.0-dev.sql).
    embedding_state    text,
    embedding_model    text,
    embedding_dim      integer,
    embedding_input_hash text,
    embedded_at        timestamptz
);

COMMENT ON TYPE pgokf.concept_search_fresh_result IS
    'One ranked hit from pgokf.concept_search_fresh: the concept_search_result columns plus the concept''s effective freshness annotation - state, reason codes, the scope the state was recorded at, stale_since, the producer''s opaque observed/indexed revisions, the catalog generation the materialization covers (published_revision), the bundle''s live catalog_generation, and last_reconciled_at - plus the embedding provenance: embedding_state (missing / current / stale, where current means the stored vector satisfies the semantic eligibility predicate: source file hash equal to the concept''s current file_hash, model/dimension/contract matching the embedding policy, and the concept effectively fresh), embedding_model, embedding_dim, embedding_input_hash, and embedded_at (NULL when no embedding row exists).';
",
        name = "search_fresh_type",
        requires = ["catalog_tables", "effective_freshness_view"]
    );

    /// Rank catalog concepts with their effective freshness annotation and an
    /// optional freshness filter.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Identical
    /// match/rank/filter/pagination semantics to `concept_search` (the native
    /// FTS pipeline), plus a `freshness` filter - `any` (the default),
    /// `fresh`, or `stale` - and a freshness annotation on every hit:
    /// lexical results may include stale concepts, always labeled. A concept
    /// with no recorded freshness row reports `fresh`. Every hit also carries
    /// its embedding provenance: `embedding_state` (`missing` / `current` /
    /// `stale`, where `current` is the semantic eligibility predicate of
    /// `concept_search_semantic`), `embedding_model`, `embedding_dim`,
    /// `embedding_input_hash`, and `embedded_at`.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    #[pg_extern(stable, parallel_restricted, requires = ["search_fresh_type"])]
    fn concept_search_fresh(
        query: &str,
        bundle_id: default!(Option<i64>, "NULL"),
        limit_count: default!(i32, 20),
        freshness: default!(&str, "'any'"),
        concept_type: default!(Option<&str>, "NULL"),
        tags: default!(Option<Vec<String>>, "NULL"),
        status: default!(Option<&str>, "NULL"),
        trust_tier: default!(Option<&str>, "NULL"),
        after_cursor: default!(Option<pgrx::JsonB>, "NULL"),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_search_fresh_result")>
    {
        let filters = Filters::new(concept_type, tags.as_deref(), status, trust_tier, None);
        let after = super::parse_cursor(after_cursor).unwrap_or_else(|error| error.raise());
        let hits = super::concept_search_fresh_impl(
            query,
            bundle_id,
            limit_count,
            freshness,
            filters,
            after.as_ref(),
        )
        .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = hits
            .into_iter()
            .map(|hit| super::fresh_search_result(hit).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.concept_search_fresh(text, bigint, integer, text, text, text[], text, text, jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_fresh(text, bigint, integer, text, text, text[], text, text, jsonb) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search_fresh(text, bigint, integer, text, text, text[], text, text, jsonb) IS
    'Rank catalog concepts with effective freshness: the concept_search contract plus a freshness filter (any - the default - fresh, or stale; 22023 otherwise) and a per-hit freshness annotation (state, reasons, scope, stale_since, opaque observed/indexed revisions, published_revision, catalog_generation, last_reconciled_at) with concept > path > bundle override precedence; a concept with no recorded row is fresh. Every hit also carries its embedding provenance (embedding_state missing/current/stale under the semantic eligibility predicate, plus model, dimension, input hash, and embedded_at). Lexical results may include stale concepts, always labeled; the filter applies before pagination. Reader-level and tenant-scoped like concept_search. This variant always ranks with the native FTS pipeline; composition with the optional BM25 backend is deferred. Semantic ranking itself (concept_search_semantic / concept_search_hybrid) excludes ineligible embeddings rather than labeling them.';
",
        name = "search_fresh_function_hardening",
        requires = [concept_search_fresh]
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
        let filters = Filters::new(None, Some(&empty), None, None, None);

        // Assert
        assert!(filters.tags.is_none(), "an empty tag filter is dropped");
    }

    #[test]
    fn filters_new_normalizes_an_empty_type_list_to_no_filter() {
        // Arrange: an empty concept_types slice must not become
        // `type = ANY('{}')` (which matches nothing); it is a true no-op.
        let empty: Vec<String> = Vec::new();

        // Act
        let filters = Filters::new(None, None, None, None, Some(&empty));

        // Assert
        assert!(
            filters.concept_types.is_none(),
            "an empty type-membership filter is dropped"
        );
    }

    #[test]
    fn filters_new_keeps_a_non_empty_tag_slice() {
        // Arrange
        let tags = vec!["widgets".to_owned()];

        // Act
        let filters = Filters::new(Some("Reference"), Some(&tags), Some("stable"), None, None);

        // Assert
        assert_eq!(filters.concept_type, Some("Reference"));
        assert_eq!(filters.tags.map(<[String]>::len), Some(1));
        assert_eq!(filters.status, Some("stable"));
        assert_eq!(filters.trust_tier, None);
        assert_eq!(filters.concept_types, None);
    }

    #[test]
    fn validate_freshness_filter_maps_any_to_no_filter() {
        // Arrange / Act / Assert
        assert_eq!(validate_freshness_filter("any").expect("any"), None);
        assert_eq!(
            validate_freshness_filter("fresh").expect("fresh"),
            Some("fresh")
        );
        assert_eq!(
            validate_freshness_filter("stale").expect("stale"),
            Some("stale")
        );
    }

    #[test]
    fn validate_freshness_filter_rejects_unknown_values() {
        // Arrange / Act
        let error =
            validate_freshness_filter("unknown").expect_err("unknown filters must be rejected");

        // Assert
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        assert_eq!(error.sqlstate(), "22023");
    }
}
