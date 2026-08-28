//! Ranked-search backend seam: the [`SearchBackend`] Strategy and its two
//! implementations, plus BM25 index management.
//!
//! # Why a seam
//!
//! `pgokf.concept_search` has one fixed contract — signature, return shape,
//! volatility, and reader authorization — but two interchangeable execution
//! strategies behind it:
//!
//! - [`NativeBackend`] — the zero-dependency `PostgreSQL` full-text search
//!   (`websearch_to_tsquery` + `ts_rank_cd` + `ts_headline` over the weighted
//!   `body_tsv` GIN index). It is the default and works on every supported
//!   server without any extra extension.
//! - [`Bm25Backend`] — an optional adapter over the external `ParadeDB`
//!   `pg_search` extension that runs Block-Max WAND top-k over a `bm25` index,
//!   which is dramatically faster for broad, relevance-ranked queries.
//!
//! [`select`] is the factory that turns the durable `search_backend`
//! configuration value into the strategy to run; [`crate::catalog::search`]
//! reads that value once per call and dispatches through the returned trait
//! object. Adding a third backend later means adding a struct and one `match`
//! arm here — the SQL-facing function never changes (open for extension,
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
use pgrx::spi::SpiTupleTable;

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

/// One ranked-search request, resolved from the SQL-facing call.
///
/// Groups the four inputs every backend needs so the [`SearchBackend`] trait
/// stays a single-method Strategy and new backends receive the whole request
/// without a widening argument list.
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

/// Read the shared `pgokf.concept_search_result`-shaped rows from a `SPI`
/// result table into [`SearchHit`]s.
///
/// Both backends project the identical seven-column contract — `bundle_id`,
/// `concept_id`, `path`, `title`, `type`, `rank`, `headline` — so this single
/// reader keeps their row-mapping DRY and guarantees byte-identical result
/// construction whichever strategy produced the rows.
fn read_hits(table: SpiTupleTable) -> Result<Vec<SearchHit>, CatalogError> {
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
                .ok_or_else(|| missing("concept_id"))?,
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
}

/// Native `PostgreSQL` full-text-search backend — the default, dependency-free
/// strategy.
///
/// Matching uses `websearch_to_tsquery` over the weighted `body_tsv` column
/// (title `A`, tags/type/description `B`, body `D`), ranking uses
/// `ts_rank_cd`, and each hit carries a `ts_headline` snippet. The text-search
/// regconfig binds as `$4` (cast to `regconfig` in SQL, never interpolated) so
/// query parsing uses the configuration that built each row's `body_tsv`.
pub struct NativeBackend;

// `ts_rank_cd` takes no configuration; the regconfig `$4` drives both
// `websearch_to_tsquery` and `ts_headline`.
const NATIVE_QUERY: &str = "
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

impl SearchBackend for NativeBackend {
    fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, CatalogError> {
        Spi::connect(|client| {
            let table = client
                .select(
                    NATIVE_QUERY,
                    None,
                    &[
                        request.query.into(),
                        request.bundle_id.into(),
                        request.limit.into(),
                        request.text_search_config.into(),
                    ],
                )
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
// (by ctid) rather than by key — cross-bundle duplicate `id` values are ranked
// independently and correctly. The regconfig binds as `$4` and drives only the
// `ts_headline` snippet, keeping snippets identical to the native backend.
const BM25_QUERY: &str = "
    SELECT c.bundle_id,
           c.id,
           c.path,
           c.title,
           c.type,
           paradedb.score(c),
           pg_catalog.ts_headline(
               $4::pg_catalog.regconfig,
               pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
               pg_catalog.websearch_to_tsquery($4::pg_catalog.regconfig, $1))
    FROM pgokf.concepts c
    JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled
    WHERE c.id @@@ paradedb.boolean(should => ARRAY[
              paradedb.match('title', $1),
              paradedb.match('description', $1),
              paradedb.match('body_text', $1)])
      AND ($2 IS NULL OR c.bundle_id = $2)
    ORDER BY paradedb.score(c) DESC, c.id ASC
    LIMIT $3";

impl Bm25Backend {
    #[must_use]
    fn new() -> Self {
        Self {
            fallback: NativeBackend,
        }
    }

    fn run_bm25(request: &SearchRequest) -> Result<Vec<SearchHit>, CatalogError> {
        Spi::connect(|client| {
            let table = client
                .select(
                    BM25_QUERY,
                    None,
                    &[
                        request.query.into(),
                        request.bundle_id.into(),
                        request.limit.into(),
                        request.text_search_config.into(),
                    ],
                )
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
