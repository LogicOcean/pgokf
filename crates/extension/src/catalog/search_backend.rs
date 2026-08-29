// SPDX-License-Identifier: AGPL-3.0-only
//! Ranked-search backend seam: the [`SearchBackend`] Strategy and its two
//! implementations, plus BM25 index management.
//!
//! # Why a seam
//!
//! `pgokf.concept_search` has one fixed contract - signature, return shape,
//! volatility, and reader authorization - but two interchangeable execution
//! strategies behind it:
//!
//! - [`NativeBackend`] - the zero-dependency `PostgreSQL` full-text search
//!   (`websearch_to_tsquery` + `ts_rank_cd` + `ts_headline` over the weighted
//!   `body_tsv` GIN index). It is the default and works on every supported
//!   server without any extra extension.
//! - [`Bm25Backend`] - an optional adapter over the external `ParadeDB`
//!   `pg_search` extension that runs Block-Max WAND top-k over a `bm25` index,
//!   which is dramatically faster for broad, relevance-ranked queries.
//!
//! [`select`] is the factory that turns the durable `search_backend`
//! configuration value into the strategy to run; [`crate::catalog::search`]
//! reads that value once per call and dispatches through the returned trait
//! object. Adding a third backend later means adding a struct and one `match`
//! arm here - the SQL-facing function never changes (open for extension,
//! closed for modification).
//!
//! # Runtime-only coupling to `pg_search`
//!
//! The extension is compiled and installed with **no build-time reference** to
//! `pg_search`: `CREATE EXTENSION pgokf` succeeds on a server where `pg_search`
//! is absent. Every `pg_search` object ([`bm25 index`], the `@@@` operator,
//! `paradedb.score`, `paradedb.match`) is reached only through **dynamic SPI**
//! at query time. When the `bm25` backend is selected but `pg_search` is not
//! installed, or no `bm25` index exists on `pgokf.concepts`, [`Bm25Backend`]
//! logs a warning and transparently falls back to [`NativeBackend`] rather than
//! erroring, so a mis-set configuration degrades gracefully instead of breaking
//! search.
//!
//! [`bm25 index`]: rebuild

use std::path::Path;

use pgrx::Spi;
use pgrx::datum::DatumWithOid;
use pgrx::spi::SpiTupleTable;

use crate::catalog::spi_read::RowReader;
use crate::catalog::types::SearchHit;
use crate::errors::CatalogError;

/// Canonical wire name of the native full-text-search backend (the default).
pub const NATIVE: &str = "native";
/// Canonical wire name of the optional `ParadeDB` `pg_search` BM25 backend.
pub const BM25: &str = "bm25";

/// Fixed name of the `bm25` index [`rebuild`] manages on `pgokf.concepts`.
const BM25_INDEX_NAME: &str = "concepts_bm25_idx";

/// Report whether `name` is a supported `search_backend` value.
///
/// Used by [`crate::catalog::config`] to validate the durable configuration
/// key against exactly the strategies [`select`] can construct, so the accepted
/// set can never drift from the dispatcher.
#[must_use]
pub fn is_supported(name: &str) -> bool {
    matches!(name, NATIVE | BM25)
}

/// The supported backend names, formatted for an error message.
#[must_use]
pub fn supported_display() -> String {
    format!("'{NATIVE}', '{BM25}'")
}

/// An opaque keyset-pagination cursor: the total-order position of the last row
/// of the previous page.
///
/// Ranked search has a **stable total order** - `rank DESC, bundle_id ASC,
/// concept_id ASC` - so a page can continue strictly *after* a known row without
/// `OFFSET` (which drifts and re-scans as the result set grows). A caller copies
/// the three fields from the last `pgokf.concept_search_result` row of a page
/// into a JSON object `{"rank":..,"bundle_id":..,"concept_id":..}` and passes it
/// back as `after_cursor`; [`crate::catalog::search`] parses it into this struct
/// and both backends resume from it (see [`bind_search_args`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Cursor {
    /// The `rank` of the previous page's last row (the descending primary key).
    pub rank: f32,
    /// That row's `bundle_id` (the first ascending tiebreaker).
    pub bundle_id: i64,
    /// That row's `concept_id` (the final ascending tiebreaker).
    pub concept_id: String,
}

/// One ranked-search request, resolved from the SQL-facing call.
///
/// Groups the inputs every backend needs so the [`SearchBackend`] trait stays a
/// single-method Strategy and new backends receive the whole request without a
/// widening argument list.
#[derive(Debug, Clone, Copy)]
pub struct SearchRequest<'a> {
    /// Validated, non-empty user query text.
    pub query: &'a str,
    /// Optional bundle scope; `None` searches every enabled bundle.
    pub bundle_id: Option<i64>,
    /// Validated `LIMIT` (already range-checked into `1..=500`).
    pub limit: i64,
    /// Effective text-search configuration name for query parsing and
    /// `ts_headline` snippet generation.
    pub text_search_config: &'a str,
    /// Optional exact `concept.type` filter; `None` is a no-op.
    pub concept_type: Option<&'a str>,
    /// Optional tag filter with **ALL-of** semantics: a hit's `concepts.tags`
    /// must contain every listed tag (`tags @> $filter`). `None` (or empty) is
    /// a no-op.
    pub tags: Option<&'a [String]>,
    /// Optional OKF lifecycle `status` filter, matched against
    /// `concept_provenance.status`; `None` is a no-op.
    pub status: Option<&'a str>,
    /// Optional derived `trust_tier` filter, matched against
    /// `concept_provenance.trust_tier`; `None` is a no-op.
    pub trust_tier: Option<&'a str>,
    /// Optional keyset cursor: when `Some`, results continue strictly *after*
    /// this position in the total order (see [`Cursor`]). `None` is the first
    /// page.
    pub after: Option<&'a Cursor>,
}

/// A ranked-search execution strategy.
///
/// Implementations own how the request is executed and ranked; they all return
/// the same [`SearchHit`] shape so [`crate::catalog::search::concept_search`]
/// can pack any backend's rows into `pgokf.concept_search_result`
/// identically.
pub trait SearchBackend {
    /// Execute `request` and return the ranked hits in descending relevance,
    /// with `concept_id` as the stable tiebreaker.
    ///
    /// # Errors
    ///
    /// Returns a [`CatalogError`] when the underlying query fails or a result
    /// column is unexpectedly `NULL`.
    fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, CatalogError>;
}

/// Build the strategy for a configured `search_backend` value.
///
/// Falls back to [`NativeBackend`] for any unrecognized value; the durable
/// configuration is already validated by [`is_supported`], so an out-of-set
/// value here means an out-of-band table edit and native is the safe default.
#[must_use]
pub fn select(configured: &str) -> Box<dyn SearchBackend> {
    if configured == BM25 {
        Box::new(Bm25Backend::new())
    } else {
        Box::new(NativeBackend)
    }
}

fn spi_error(context: &'static str) -> impl Fn(pgrx::spi::Error) -> CatalogError {
    move |error| CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Bind one [`SearchRequest`] to the eleven positional parameters both backend
/// queries share (`$1`..`$11`), so the native and BM25 strategies stay in
/// lockstep on argument order and the structured-filter and cursor binding lives
/// in one place.
///
/// A `NULL`-typed parameter still carries its column type OID (pgrx supplies it
/// from the Rust type), so a `NULL` filter binds as a correctly typed `NULL` and
/// its `$n IS NULL OR ...` guard short-circuits. An empty `tags` slice is treated
/// as no filter - `Some([])` would otherwise bind `'{}'::text[]`, which every
/// non-NULL `tags` array contains but a `NULL` `tags` column does not, silently
/// dropping untagged concepts - so the caller normalizes it to `None` upstream.
///
/// The final three parameters (`$9`..`$11`) are the keyset cursor - rank,
/// `bundle_id`, `concept_id`. They are all-or-nothing: an absent cursor binds
/// three typed `NULL`s and the `$9 IS NULL OR ...` guard makes the keyset
/// predicate a no-op (the first page). The cursor `concept_id` binds by borrow
/// (`as_str`), so nothing is cloned.
fn bind_search_args<'a>(request: &'a SearchRequest) -> [DatumWithOid<'a>; 11] {
    [
        request.query.into(),
        request.bundle_id.into(),
        request.limit.into(),
        request.text_search_config.into(),
        request.concept_type.into(),
        request.tags.map(<[String]>::to_vec).into(),
        request.status.into(),
        request.trust_tier.into(),
        request.after.map(|cursor| cursor.rank).into(),
        request.after.map(|cursor| cursor.bundle_id).into(),
        request
            .after
            .map(|cursor| cursor.concept_id.as_str())
            .into(),
    ]
}

/// Read the shared `pgokf.concept_search_result`-shaped rows from a `SPI`
/// result table into [`SearchHit`]s.
///
/// Both backends project the identical seven-column contract - `bundle_id`,
/// `concept_id`, `path`, `title`, `type`, `rank`, `headline` - so this single
/// reader keeps their row-mapping DRY and guarantees byte-identical result
/// construction whichever strategy produced the rows.
fn read_hits(table: SpiTupleTable) -> Result<Vec<SearchHit>, CatalogError> {
    let mut hits = Vec::with_capacity(table.len());
    for row in table {
        let reader = RowReader::new(&row, "failed to read search result row", "search result");
        hits.push(SearchHit {
            bundle_id: reader.required(1, "bundle_id")?,
            concept_id: reader.required(2, "concept_id")?,
            path: reader.required(3, "path")?,
            title: reader.optional(4)?,
            concept_type: reader.optional(5)?,
            rank: reader.required(6, "rank")?,
            headline: reader.optional(7)?,
        });
    }
    Ok(hits)
}

/// Native `PostgreSQL` full-text-search backend - the default, dependency-free
/// strategy.
///
/// Matching uses `websearch_to_tsquery` over the weighted `body_tsv` column
/// (title `A`, tags/type/description `B`, body `D`), ranking uses
/// `ts_rank_cd`, and each hit carries a `ts_headline` snippet. The text-search
/// regconfig binds as `$4` (cast to `regconfig` in SQL, never interpolated) so
/// query parsing uses the configuration that built each row's `body_tsv`.
pub struct NativeBackend;

/// The keyset-pagination predicate and stable total order both backends share.
///
/// Wrapped around a `hits` subquery that projects the seven result columns plus
/// a computed `rank`, this continues strictly *after* the cursor (`$9`,`$10`,
/// `$11`) in the total order `rank DESC, bundle_id ASC, concept_id ASC`. Because
/// the order mixes directions, the keyset is the expanded lexicographic
/// comparison (not a single row-value `<`): a strictly smaller rank, or an equal
/// rank with a strictly greater `bundle_id`, or an equal `(rank, bundle_id)` with
/// a strictly greater `concept_id`. When `$9` is `NULL` (no cursor) the whole
/// predicate is a no-op and the first page is returned. Applying the filter
/// *outside* the ranked subquery - then `ORDER BY ... LIMIT $3` - is what makes
/// the pages tile the full result set with no duplicates and no skips even when
/// ranks tie.
const KEYSET_ORDER_LIMIT: &str = "
    WHERE $9 IS NULL
       OR hits.rank < $9
       OR (hits.rank = $9 AND hits.bundle_id > $10)
       OR (hits.rank = $9 AND hits.bundle_id = $10 AND hits.concept_id > $11)
    ORDER BY hits.rank DESC, hits.bundle_id ASC, hits.concept_id ASC
    LIMIT $3";

// `ts_rank_cd` takes no configuration; the regconfig `$4` drives both
// `websearch_to_tsquery` and `ts_headline`.
//
// The structured filters bind as $5..$8 and each is a no-op when its bound
// value is NULL (`$n IS NULL OR ...`), so the three-argument and filtered calls
// share one plan. `concept_type` matches `c.type` ($5), `tags` matches with
// ALL-of containment against the `tags` GIN index ($6 as `c.tags @> $6`), and
// `status`/`trust_tier` ($7/$8) match the `LEFT JOIN`ed provenance row (a
// concept with no provenance row has NULL status/tier and is excluded by a
// non-NULL filter, as intended). The LEFT JOIN never multiplies rows -
// `concept_provenance`'s primary key is `(bundle_id, concept_id)` - so an
// all-NULL-filter call returns exactly what it did before. The match and the
// filters live in the `hits` subquery; the shared KEYSET_ORDER_LIMIT tail applies
// the cursor, the stable total order, and the limit over it.
const NATIVE_QUERY: &str = "
    SELECT hits.bundle_id,
           hits.concept_id,
           hits.path,
           hits.title,
           hits.type,
           hits.rank,
           hits.headline
    FROM (
        SELECT c.bundle_id AS bundle_id,
               c.id AS concept_id,
               c.path AS path,
               c.title AS title,
               c.type AS type,
               pg_catalog.ts_rank_cd(c.body_tsv, q.query) AS rank,
               pg_catalog.ts_headline(
                   $4::pg_catalog.regconfig,
                   pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
                   q.query) AS headline
        FROM pgokf.concepts c
        JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
        LEFT JOIN pgokf.concept_provenance cp
               ON cp.bundle_id = c.bundle_id AND cp.concept_id = c.id,
             pg_catalog.websearch_to_tsquery($4::pg_catalog.regconfig, $1) AS q(query)
        WHERE c.body_tsv @@ q.query
          AND ($2 IS NULL OR c.bundle_id = $2)
          AND ($5 IS NULL OR c.type = $5)
          AND ($6 IS NULL OR c.tags @> $6)
          AND ($7 IS NULL OR cp.status = $7)
          AND ($8 IS NULL OR cp.trust_tier = $8)
    ) AS hits";

impl SearchBackend for NativeBackend {
    fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, CatalogError> {
        let query = format!("{NATIVE_QUERY}{KEYSET_ORDER_LIMIT}");
        Spi::connect(|client| {
            let table = client
                .select(&query, None, &bind_search_args(request))
                .map_err(spi_error("native search query failed"))?;
            read_hits(table)
        })
    }
}

/// Optional `ParadeDB` `pg_search` BM25 backend.
///
/// Runs Block-Max WAND top-k over a `bm25` index. The user query binds as a
/// parameter into the `paradedb.match` builder (never interpolated into a
/// `pg_search` query string), so a query containing `pg_search` query syntax is
/// treated as literal search terms, not as operators. Matching spans the
/// `title`, `description`, and `body_text` fields (a `should` boolean),
/// mirroring the fields native FTS weights; ranking is `paradedb.score`; and
/// each hit still carries a `ts_headline` snippet computed the same way native
/// does, so snippets are consistent across backends.
///
/// When `pg_search` is not installed, or no `bm25` index exists on
/// `pgokf.concepts`, [`Bm25Backend::search`] logs a warning and delegates to
/// its [`NativeBackend`] fallback.
pub struct Bm25Backend {
    fallback: NativeBackend,
}

// The `@@@` operator lives in `pg_catalog`, so it resolves under any
// search_path; every `paradedb.*` object is schema-qualified. `paradedb.score`
// takes the whole-row relation reference `c`, so it scores per scanned tuple
// (by ctid) rather than by key - cross-bundle duplicate `id` values are ranked
// independently and correctly. The regconfig binds as `$4` and drives only the
// `ts_headline` snippet, keeping snippets identical to the native backend. The
// `@@@` match, `paradedb.score`, and the filters live in the `hits` subquery so
// the shared KEYSET_ORDER_LIMIT tail applies the cursor, the stable total order,
// and the limit over the already-scored rows.
const BM25_QUERY: &str = "
    SELECT hits.bundle_id,
           hits.concept_id,
           hits.path,
           hits.title,
           hits.type,
           hits.rank,
           hits.headline
    FROM (
        SELECT c.bundle_id AS bundle_id,
               c.id AS concept_id,
               c.path AS path,
               c.title AS title,
               c.type AS type,
               paradedb.score(c) AS rank,
               pg_catalog.ts_headline(
                   $4::pg_catalog.regconfig,
                   pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
                   pg_catalog.websearch_to_tsquery($4::pg_catalog.regconfig, $1)) AS headline
        FROM pgokf.concepts c
        JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
        LEFT JOIN pgokf.concept_provenance cp
               ON cp.bundle_id = c.bundle_id AND cp.concept_id = c.id
        WHERE c.id @@@ paradedb.boolean(should => ARRAY[
                  paradedb.match('title', $1),
                  paradedb.match('description', $1),
                  paradedb.match('body_text', $1)])
          AND ($2 IS NULL OR c.bundle_id = $2)
          AND ($5 IS NULL OR c.type = $5)
          AND ($6 IS NULL OR c.tags @> $6)
          AND ($7 IS NULL OR cp.status = $7)
          AND ($8 IS NULL OR cp.trust_tier = $8)
    ) AS hits";

impl Bm25Backend {
    #[must_use]
    fn new() -> Self {
        Self {
            fallback: NativeBackend,
        }
    }

    fn run_bm25(request: &SearchRequest) -> Result<Vec<SearchHit>, CatalogError> {
        let query = format!("{BM25_QUERY}{KEYSET_ORDER_LIMIT}");
        Spi::connect(|client| {
            let table = client
                .select(&query, None, &bind_search_args(request))
                .map_err(spi_error("bm25 search query failed"))?;
            read_hits(table)
        })
    }
}

impl SearchBackend for Bm25Backend {
    fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, CatalogError> {
        if !pg_search_installed()? {
            pgrx::warning!(
                "pgokf: search_backend is 'bm25' but the pg_search extension is not installed; \
                 falling back to native full-text search. Install pg_search or set \
                 search_backend to 'native' to silence this warning."
            );
            return self.fallback.search(request);
        }
        if !bm25_index_present()? {
            pgrx::warning!(
                "pgokf: search_backend is 'bm25' but no bm25 index exists on pgokf.concepts; \
                 falling back to native full-text search. Run pgokf.rebuild_search_index() to \
                 build the index."
            );
            return self.fallback.search(request);
        }
        Self::run_bm25(request)
    }
}

/// Report whether the `pg_search` extension is installed in this database.
fn pg_search_installed() -> Result<bool, CatalogError> {
    Spi::get_one::<bool>(
        "SELECT pg_catalog.count(*) > 0 FROM pg_catalog.pg_extension WHERE extname = 'pg_search'",
    )
    .map_err(spi_error("failed to check for the pg_search extension"))?
    .ok_or_else(|| CatalogError::internal("pg_search probe returned no row", Path::new("")))
}

/// Report whether a `bm25`-access-method index exists on `pgokf.concepts`.
///
/// Detection is by access method rather than by index name, so an index built
/// out of band (any name) still counts, and [`rebuild`]'s named index is not
/// special-cased.
fn bm25_index_present() -> Result<bool, CatalogError> {
    Spi::get_one::<bool>(
        "SELECT pg_catalog.count(*) > 0
         FROM pg_catalog.pg_index i
         JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
         JOIN pg_catalog.pg_am am ON am.oid = ic.relam
         WHERE i.indrelid = 'pgokf.concepts'::pg_catalog.regclass
           AND am.amname = 'bm25'",
    )
    .map_err(spi_error(
        "failed to check for a bm25 index on pgokf.concepts",
    ))?
    .ok_or_else(|| CatalogError::internal("bm25 index probe returned no row", Path::new("")))
}

/// (Re)build the `bm25` index on `pgokf.concepts`, or report the no-op.
///
/// Returns `true` when the index was (re)built and `false` when `pg_search` is
/// absent (a logged no-op). Runs entirely through dynamic SQL over fixed,
/// input-free statements so the extension never statically references
/// `pg_search`.
fn rebuild() -> Result<bool, CatalogError> {
    crate::security::authorize_current_user(crate::security::Operation::Register, Path::new(""))?;
    if !pg_search_installed()? {
        pgrx::notice!(
            "pgokf: pg_search is not installed; rebuild_search_index is a no-op. Install \
             pg_search (and set search_backend to 'bm25') to enable BM25 search."
        );
        return Ok(false);
    }

    // Fixed identifiers, no caller input; drop-then-create so the function is
    // idempotent and safe to re-run after a schema or tokenizer change. The
    // key_field is `id`: paradedb.score scores per scanned tuple (by ctid), so
    // the non-global-uniqueness of `id` across bundles does not affect ranking
    // or visibility, and no surrogate key column is imposed on the core table.
    Spi::run(&format!("DROP INDEX IF EXISTS pgokf.{BM25_INDEX_NAME}")).map_err(|error| {
        CatalogError::internal(
            format!("failed to drop existing bm25 index: {error}"),
            Path::new(""),
        )
    })?;
    Spi::run(&format!(
        "CREATE INDEX {BM25_INDEX_NAME} ON pgokf.concepts \
         USING bm25 (id, title, description, body_text, type) \
         WITH (key_field='id')"
    ))
    .map_err(|error| {
        CatalogError::internal(
            format!("failed to create bm25 index: {error}"),
            Path::new(""),
        )
    })?;
    Ok(true)
}

/// SQL-facing BM25 index management, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{extension_sql, pg_extern};

    use super::rebuild;

    /// (Re)build the BM25 search index on `pgokf.concepts`.
    ///
    /// Requires membership in `pgokf_admin`. When the `ParadeDB` `pg_search`
    /// extension is installed this drops and recreates the `bm25` index used by
    /// `search_backend = 'bm25'`, returning `true`. When `pg_search` is absent
    /// it is a no-op that emits a `NOTICE` and returns `false`. Run it after
    /// enabling the `bm25` backend, and after a bulk re-sync if you want the
    /// index rebuilt from scratch (incremental sync maintains it automatically
    /// once it exists).
    #[pg_extern(requires = ["catalog_tables"])]
    fn rebuild_search_index() -> bool {
        rebuild().unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.rebuild_search_index()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.rebuild_search_index() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.rebuild_search_index() TO pgokf_admin;
COMMENT ON FUNCTION pgokf.rebuild_search_index() IS
    'Admin-only. (Re)build the ParadeDB pg_search bm25 index on pgokf.concepts used by search_backend=bm25; returns true when built, or false (with a NOTICE) when pg_search is not installed.';
",
        name = "rebuild_search_index_hardening",
        requires = [rebuild_search_index]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_supported_accepts_both_backends() {
        // Arrange & Act & Assert
        assert!(is_supported(NATIVE));
        assert!(is_supported(BM25));
    }

    #[test]
    fn is_supported_rejects_unknown_backend() {
        // Arrange & Act & Assert
        assert!(!is_supported("solr"));
        assert!(!is_supported(""));
    }

    #[test]
    fn supported_display_lists_both_backends() {
        // Arrange & Act
        let display = supported_display();

        // Assert
        assert!(display.contains(NATIVE));
        assert!(display.contains(BM25));
    }
}
