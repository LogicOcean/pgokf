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
//! - [`Bm25Backend`] - an optional adapter over an external BM25 provider
//!   extension: Tiger Data `pg_textsearch` (`PostgreSQL` license; `PostgreSQL`
//!   17 and 18) or `ParadeDB` `pg_search`, selected by the `bm25_provider` policy
//!   (`auto` prefers `pg_textsearch`). Each runs top-k over a `bm25` index,
//!   which is dramatically faster for broad, relevance-ranked queries.
//!
//! [`select`] is the factory that turns the durable `search_backend`
//! configuration value into the strategy to run; [`crate::catalog::search`]
//! reads that value once per call and dispatches through the returned trait
//! object. Adding a third backend later means adding a struct and one `match`
//! arm here - the SQL-facing function never changes (open for extension,
//! closed for modification).
//!
//! # Runtime-only coupling to the BM25 providers
//!
//! The extension is compiled and installed with **no build-time reference** to
//! either provider: `CREATE EXTENSION pgokf` succeeds on a server where neither
//! is present. Every provider object ([`bm25 index`], `pg_textsearch`'s `<@>`
//! operator and `to_bm25query`, `pg_search`'s `@@@` operator, `paradedb.score`,
//! `paradedb.match`) is reached only through **dynamic SPI** at query time.
//! When the `bm25` backend is selected but the provider `bm25_provider`
//! resolves to is not installed, or no `bm25` index exists on
//! `pgokf.concepts`, [`Bm25Backend`] logs a warning and transparently falls
//! back to [`NativeBackend`] rather than erroring, so a mis-set configuration
//! degrades gracefully instead of breaking search.
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
/// Canonical wire name of the optional BM25 backend, served by whichever BM25
/// provider extension is installed (see [`Bm25Provider`]).
pub const BM25: &str = "bm25";

/// `bm25_provider` value: pick the installed provider automatically, preferring
/// `pg_textsearch` (PostgreSQL-licensed) over `pg_search`.
pub const PROVIDER_AUTO: &str = "auto";
/// `bm25_provider` value: `ParadeDB` `pg_search` (AGPL-3.0 community edition).
pub const PROVIDER_PG_SEARCH: &str = "pg_search";
/// `bm25_provider` value: Tiger Data `pg_textsearch` (`PostgreSQL` license;
/// `PostgreSQL` 17 and 18 only).
pub const PROVIDER_PG_TEXTSEARCH: &str = "pg_textsearch";

/// Fixed name of the `bm25` index [`rebuild`] manages on `pgokf.concepts`.
/// Both providers name their access method `bm25` and cannot coexist in one
/// database, so one name serves either.
const BM25_INDEX_NAME: &str = "concepts_bm25_idx";

/// The text expression the `pg_textsearch` index covers and every
/// `pg_textsearch` query must repeat verbatim (an expression index is only
/// usable by a textually identical expression). Title, description, and body
/// in one field: `pg_textsearch` scores a single expression, where `pg_search`
/// scored three fields with a boolean `should`.
const TEXTSEARCH_EXPRESSION: &str =
    "(coalesce(c.title, '') || ' ' || coalesce(c.description, '') || ' ' || c.body_text)";
/// The same expression written against the bare table, for `CREATE INDEX`.
const TEXTSEARCH_INDEX_EXPRESSION: &str =
    "(coalesce(title, '') || ' ' || coalesce(description, '') || ' ' || body_text)";

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

/// Report whether `name` is a supported `bm25_provider` value.
#[must_use]
pub fn is_supported_provider(name: &str) -> bool {
    matches!(
        name,
        PROVIDER_AUTO | PROVIDER_PG_SEARCH | PROVIDER_PG_TEXTSEARCH
    )
}

/// The supported provider names, formatted for an error message.
#[must_use]
pub fn supported_providers_display() -> String {
    format!("'{PROVIDER_AUTO}', '{PROVIDER_PG_SEARCH}', '{PROVIDER_PG_TEXTSEARCH}'")
}

/// The BM25 provider extension a `bm25` search or index build runs on.
///
/// Resolved at call time from the durable `bm25_provider` policy and what is
/// actually installed, never assumed: the extension has no build-time
/// reference to either provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Bm25Provider {
    /// `ParadeDB` `pg_search`, reached through the `SECURITY DEFINER`
    /// `pgokf.bm25_hits` helper.
    PgSearch,
    /// Tiger Data `pg_textsearch`, queried inline with invoker rights. Carries
    /// the schema the extension was created in, so the query can qualify its
    /// function and operator regardless of the caller's `search_path`.
    PgTextsearch { schema: String },
}

impl Bm25Provider {
    /// The provider's wire name, as reported in `search_index_status`.
    const fn name(&self) -> &'static str {
        match self {
            Self::PgSearch => PROVIDER_PG_SEARCH,
            Self::PgTextsearch { .. } => PROVIDER_PG_TEXTSEARCH,
        }
    }
}

/// The installed BM25 provider extensions and their schemas.
fn installed_providers() -> Result<Vec<(String, String)>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT e.extname::pg_catalog.text, n.nspname::pg_catalog.text
                 FROM pg_catalog.pg_extension e
                 JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
                 WHERE e.extname IN ('pg_search', 'pg_textsearch')
                 ORDER BY e.extname",
                None,
                &[],
            )
            .map_err(spi_error("failed to probe the installed BM25 providers"))?;
        let mut installed = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read provider row", "provider");
            installed.push((
                reader.required(1, "extname")?,
                reader.required(2, "nspname")?,
            ));
        }
        Ok(installed)
    })
}

/// Resolve the provider for the configured `bm25_provider` policy value, or
/// `None` when the wanted provider is not installed.
///
/// `auto` prefers `pg_textsearch` over `pg_search`; a named provider must be
/// installed itself. Both providers register an access method called `bm25`,
/// so at most one can exist in a database and the result is unambiguous.
pub(crate) fn resolve_provider(configured: &str) -> Result<Option<Bm25Provider>, CatalogError> {
    let installed = installed_providers()?;
    let find = |name: &str| {
        installed
            .iter()
            .find(|(extname, _)| extname == name)
            .map(|(_, schema)| schema.clone())
    };
    let textsearch =
        find(PROVIDER_PG_TEXTSEARCH).map(|schema| Bm25Provider::PgTextsearch { schema });
    let pg_search = find(PROVIDER_PG_SEARCH).map(|_| Bm25Provider::PgSearch);
    Ok(match configured {
        PROVIDER_PG_SEARCH => pg_search,
        PROVIDER_PG_TEXTSEARCH => textsearch,
        _ => textsearch.or(pg_search),
    })
}

/// Explain why `resolve_provider` found nothing for `configured`, naming what
/// *is* installed when a pinned provider is the one missing, so the operator
/// sees the actual fix (install it, or point `bm25_provider` at the other).
fn describe_missing_provider(configured: &str) -> Result<String, CatalogError> {
    let installed: Vec<String> = installed_providers()?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    Ok(match (configured, installed.first()) {
        (PROVIDER_AUTO, _) | (_, None) if installed.is_empty() => format!(
            "no BM25 provider extension is installed (bm25_provider = '{configured}'; supported: \
             {})",
            supported_providers_display()
        ),
        (pinned, Some(other)) => format!(
            "bm25_provider = '{pinned}' names a provider that is not installed ({other} is; set \
             bm25_provider to '{other}' or 'auto' to use it)"
        ),
        (_, None) => unreachable!("installed is non-empty in this arm"),
    })
}

/// The configured `bm25_provider` policy value, read through the reader-callable
/// `pgokf.get_config()` (the private table is not reader-readable).
pub(crate) fn configured_provider() -> Result<String, CatalogError> {
    Spi::get_one::<String>("SELECT pgokf.get_config() ->> 'bm25_provider'")
        .map_err(spi_error("failed to read bm25_provider"))?
        .ok_or_else(|| {
            CatalogError::internal("bm25_provider is missing from configuration", Path::new(""))
        })
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
/// When the provider `bm25_provider` resolves to is not installed, or the
/// `bm25` index it needs does not exist on `pgokf.concepts`,
/// [`Bm25Backend::search`] logs a warning and delegates to its
/// [`NativeBackend`] fallback.
pub struct Bm25Backend {
    fallback: NativeBackend,
}

// The BM25 hit query lives in SQL, as the `SECURITY DEFINER` helper
// `pgokf.bm25_hits` created by the `bm25_hits_function` SQL block below, and this is
// merely its call. Why not a query string like the native backend: for any
// session that is not the table owner, row-level security injects the tenant
// policy - a predicate that calls `current_setting()` inline - around
// `pgokf.concepts`, and `pg_search` cannot plan its custom scan under such a
// predicate: it falls back to a sequential scan and `paradedb.score` then
// raises "Unsupported query shape". Every production reader is a non-owner, so
// the BM25 path must run with the owner's privileges (no policy injected) and
// apply the tenant scope itself, through a bound parameter rather than an
// inline function call. The helper does exactly that and keeps every other
// property of the query: the same eleven parameters as [`bind_search_args`],
// the same seven-column projection, the same keyset tail, the same limit.
const BM25_HITS_CALL: &str = "
    SELECT bundle_id, concept_id, path, title, type, rank, headline
    FROM pgokf.bm25_hits($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)";

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
                .select(BM25_HITS_CALL, None, &bind_search_args(request))
                .map_err(spi_error("bm25 search query failed"))?;
            read_hits(table)
        })
    }
}

impl SearchBackend for Bm25Backend {
    fn search(&self, request: &SearchRequest) -> Result<Vec<SearchHit>, CatalogError> {
        let configured = configured_provider()?;
        let Some(provider) = resolve_provider(&configured)? else {
            pgrx::warning!(
                "pgokf: search_backend is 'bm25' but {}; falling back to native full-text \
                 search. Install the provider, or set search_backend to 'native', to silence \
                 this warning.",
                describe_missing_provider(&configured)?
            );
            return self.fallback.search(request);
        };
        if !bm25_index_present(&provider)? {
            pgrx::warning!(
                "pgokf: search_backend is 'bm25' ({}) but the bm25 index pgokf.{} does not \
                 exist on pgokf.concepts; falling back to native full-text search. Run \
                 pgokf.rebuild_search_index() to build the index.",
                provider.name(),
                BM25_INDEX_NAME
            );
            return self.fallback.search(request);
        }
        match provider {
            Bm25Provider::PgSearch => Self::run_bm25(request),
            Bm25Provider::PgTextsearch { schema } => Self::run_textsearch(request, &schema),
        }
    }
}

/// How many rows beyond the page the `pg_textsearch` candidate scan may spend
/// on closing a tie band at the page boundary.
///
/// Keyset pages tile the result set only if every row that ties the page's
/// last rank is ordered by the `(bundle_id, concept_id)` tiebreak, and the
/// provider's index-ordered scan returns equal scores in its own internal
/// order. The candidate scan therefore keeps reading past the page while the
/// rank stays equal to the boundary rank, so the SQL ordering step can apply
/// the tiebreak over the whole band. This cap bounds that extra work (each
/// candidate costs one standalone scoring call) and seeds the provider's
/// top-k with `LIMIT $3 + cap`; a band longer than the cap is reported with a
/// `WARNING` and paginated approximately.
const TEXTSEARCH_TIE_CLOSURE: i64 = 256;

/// The `pg_textsearch` candidate scan: the page's rows, in the provider's
/// index order, as `(bundle_id, concept_id, rank)` triples.
///
/// The shape is the one the provider's index access method plans as a top-k
/// index scan: `ORDER BY <index expression> <@> to_bm25query(...) LIMIT n`,
/// with every filter - the active-bundle join, the structured filters, the
/// keyset predicate, and any row-level-security policy - applied as ordinary
/// quals on that scan. `to_bm25query` takes the query text as a bound
/// parameter (a `Param`, not a `Var`, which is what keeps the order-by
/// operator indexable) and names the index schema-qualified because the
/// provider resolves it through the caller's `search_path`; the operator and
/// function are qualified with the schema `pg_textsearch` was created in, so
/// the query is independent of `search_path` too.
///
/// `pg_textsearch` returns the *negated* BM25 score as `float8`, so ascending
/// index order is best-first; the scan yields matching rows only. The rank
/// projected here is that score negated and cast to the result's `real`, the
/// same value the keyset predicate compares against `$9`, so a cursor copied
/// from a previous page continues exactly where that page ended. Standalone
/// scoring (the operator evaluated outside the index order) happens only for
/// the rows the scan returns and for the rows the keyset predicate skips, so
/// a page costs `O(page + tie band + cursor offset)` scoring calls, never a
/// scan of the corpus. Runs with invoker rights: the access method plans
/// normally under row-level security, so the policies scope the rows exactly
/// as they do for native search and no privileged helper is needed.
pub(crate) fn textsearch_candidate_query(schema: &str) -> String {
    let schema = quote_identifier(schema);
    let expr = TEXTSEARCH_EXPRESSION;
    let index = BM25_INDEX_NAME;
    let closure = TEXTSEARCH_TIE_CLOSURE;
    let score =
        format!("({expr} OPERATOR({schema}.<@>) {schema}.to_bm25query($1, 'pgokf.{index}'))");
    let rank = format!("(-{score})::pg_catalog.float4");
    format!(
        "SELECT c.bundle_id AS bundle_id,
                c.id AS concept_id,
                {rank} AS rank
         FROM pgokf.concepts c
         JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
         LEFT JOIN pgokf.concept_provenance cp
                ON cp.bundle_id = c.bundle_id AND cp.concept_id = c.id
         WHERE ($2 IS NULL OR c.bundle_id = $2)
           AND ($5 IS NULL OR c.type = $5)
           AND ($6 IS NULL OR c.tags @> $6)
           AND ($7 IS NULL OR cp.status = $7)
           AND ($8 IS NULL OR cp.trust_tier = $8)
           AND ($9 IS NULL
                OR {rank} < $9
                OR ({rank} = $9 AND c.bundle_id > $10)
                OR ({rank} = $9 AND c.bundle_id = $10 AND c.id > $11))
         ORDER BY {score}
         LIMIT $3 + {closure}"
    )
}

/// The `pg_textsearch` ordering-and-projection step over the collected
/// candidates.
///
/// The candidates arrive as three parallel arrays (`$3` bundle ids, `$4`
/// concept ids, `$5` ranks), zipped with `ROWS FROM` (the multi-argument
/// `unnest(a, b, c)` spelling is a parser special case that only the
/// unqualified name gets). The inner query applies the stable total order
/// `rank DESC, bundle_id ASC, concept_id ASC` - in SQL, so the `concept_id`
/// tiebreak uses the database collation the keyset predicate compares with -
/// and keeps the page (`$6`); only those rows are joined back to
/// `pgokf.concepts` for `path`, `title`, `type`, and the `ts_headline` snippet
/// (`$1` query text, `$2` text-search configuration), computed the same way
/// the native backend computes it. Invoker rights, so row-level security
/// applies to the join as it did to the scan.
const TEXTSEARCH_PROJECTION_QUERY: &str = "
    SELECT c.bundle_id,
           c.id AS concept_id,
           c.path,
           c.title,
           c.type,
           page.rank,
           pg_catalog.ts_headline(
               $2::pg_catalog.regconfig,
               pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
               pg_catalog.websearch_to_tsquery($2::pg_catalog.regconfig, $1)) AS headline
    FROM (
        SELECT k.bundle_id, k.concept_id, k.rank
        FROM ROWS FROM (pg_catalog.unnest($3::pg_catalog.int8[]),
                        pg_catalog.unnest($4::pg_catalog.text[]),
                        pg_catalog.unnest($5::pg_catalog.float4[]))
             AS k(bundle_id, concept_id, rank)
        ORDER BY k.rank DESC, k.bundle_id ASC, k.concept_id ASC
        LIMIT $6
    ) AS page
    JOIN pgokf.concepts c ON c.bundle_id = page.bundle_id AND c.id = page.concept_id
    ORDER BY page.rank DESC, page.bundle_id ASC, page.concept_id ASC";

/// One row of the `pg_textsearch` candidate scan.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    bundle_id: i64,
    concept_id: String,
    rank: f32,
}

/// Collects one page of candidates from an index-ordered (best-first) scan:
/// the first `limit` rows, then every following row that ties the page's
/// boundary rank, up to [`TEXTSEARCH_TIE_CLOSURE`] extra rows.
///
/// Pure over the scan order, so the tie-closure rule is unit-testable
/// without a database.
struct PageCollector {
    limit: usize,
    boundary_rank: Option<f32>,
    rows: Vec<Candidate>,
    truncated: bool,
}

impl PageCollector {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            boundary_rank: None,
            rows: Vec::with_capacity(limit),
            truncated: false,
        }
    }

    /// Feed the next candidate in scan order. Returns `false` once the page
    /// (and its tie band) is complete, so the caller can stop fetching.
    fn push(&mut self, candidate: Candidate) -> bool {
        // The scan yields matches only, best first; a non-positive (or NaN)
        // rank can only mean the provider scored a non-match, past every hit.
        if candidate.rank.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            return false;
        }
        if let Some(boundary) = self.boundary_rank {
            // Exact identity, not a tolerance: a tie is two rows carrying the
            // same float4 rank the SQL keyset predicate compares with `=`.
            if candidate.rank.to_bits() != boundary.to_bits() {
                return false;
            }
            if self.rows.len()
                >= self.limit + usize::try_from(TEXTSEARCH_TIE_CLOSURE).unwrap_or(usize::MAX)
            {
                self.truncated = true;
                return false;
            }
        }
        let rank = candidate.rank;
        self.rows.push(candidate);
        if self.rows.len() == self.limit {
            self.boundary_rank = Some(rank);
        }
        true
    }

    /// Whether a tie band at the boundary exceeded the closure cap, in which
    /// case keyset pages across that band are approximate.
    fn truncated(&self) -> bool {
        self.truncated
    }

    fn into_rows(self) -> Vec<Candidate> {
        self.rows
    }
}

/// Read one candidate row (`bundle_id`, `concept_id`, `rank`).
fn read_candidate(row: &pgrx::spi::SpiHeapTupleData<'_>) -> Result<Candidate, CatalogError> {
    let reader = RowReader::new(row, "failed to read bm25 candidate row", "bm25 candidate");
    Ok(Candidate {
        bundle_id: reader.required(1, "bundle_id")?,
        concept_id: reader.required(2, "concept_id")?,
        rank: reader.required(3, "rank")?,
    })
}

/// Quote a schema name for interpolation into SQL text.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Bm25Backend {
    /// Run one page through `pg_textsearch`: collect the page's candidates
    /// from the provider's index-ordered scan (closing the tie band at the
    /// boundary), then order and project them in SQL.
    fn run_textsearch(
        request: &SearchRequest,
        schema: &str,
    ) -> Result<Vec<SearchHit>, CatalogError> {
        let (candidates, truncated) = Self::textsearch_candidates(request, schema)?;
        if truncated {
            pgrx::warning!(
                "pgokf: more than {TEXTSEARCH_TIE_CLOSURE} concepts share the BM25 score at \
                 this page boundary; keyset pagination across them is approximate (rows may \
                 be skipped or repeated between pages). Use a larger page, or the native \
                 backend when exact tiling matters."
            );
        }
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let bundle_ids: Vec<i64> = candidates.iter().map(|c| c.bundle_id).collect();
        let concept_ids: Vec<String> = candidates.iter().map(|c| c.concept_id.clone()).collect();
        let ranks: Vec<f32> = candidates.iter().map(|c| c.rank).collect();
        Spi::connect(|client| {
            let table = client
                .select(
                    TEXTSEARCH_PROJECTION_QUERY,
                    None,
                    &[
                        request.query.into(),
                        request.text_search_config.into(),
                        bundle_ids.into(),
                        concept_ids.into(),
                        ranks.into(),
                        request.limit.into(),
                    ],
                )
                .map_err(spi_error("pg_textsearch projection query failed"))?;
            read_hits(table)
        })
    }

    /// Fetch the page's candidates through a cursor over the index-ordered
    /// scan, stopping as soon as the page and its boundary tie band are
    /// complete so no more rows than needed are scored. Returns the
    /// candidates and whether the tie band was cut at the closure cap.
    fn textsearch_candidates(
        request: &SearchRequest,
        schema: &str,
    ) -> Result<(Vec<Candidate>, bool), CatalogError> {
        let query = textsearch_candidate_query(schema);
        let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
        let batch = std::ffi::c_long::try_from(request.limit).unwrap_or(std::ffi::c_long::MAX);
        Spi::connect(|client| {
            let mut cursor = client
                .try_open_cursor(&query, &bind_search_args(request))
                .map_err(spi_error("pg_textsearch candidate scan failed"))?;
            let mut page = PageCollector::new(limit);
            loop {
                let table = cursor
                    .fetch(batch)
                    .map_err(spi_error("pg_textsearch candidate fetch failed"))?;
                let fetched = table.len();
                let mut wants_more = true;
                for row in table {
                    if !page.push(read_candidate(&row)?) {
                        wants_more = false;
                        break;
                    }
                }
                if !wants_more
                    || fetched == 0
                    || fetched < usize::try_from(batch).unwrap_or(usize::MAX)
                {
                    break;
                }
            }
            let truncated = page.truncated();
            Ok((page.into_rows(), truncated))
        })
    }
}

/// Report whether the `bm25` index the provider's query needs exists on
/// `pgokf.concepts`.
///
/// `pg_search` scores through its `@@@` operator, which finds any `bm25`
/// index on the table, so detection is by access method and an index built
/// out of band (any name) counts. `pg_textsearch` names the index in
/// `to_bm25query` and raises when the name does not resolve, so for that
/// provider the probe requires the index [`rebuild`] creates, by name; an
/// out-of-band index then falls back to native with the warning naming what
/// is missing, rather than erroring in the query.
pub(crate) fn bm25_index_present(provider: &Bm25Provider) -> Result<bool, CatalogError> {
    let by_name = match provider {
        Bm25Provider::PgSearch => "",
        Bm25Provider::PgTextsearch { .. } => {
            "AND ic.relname = $1 AND ic.relnamespace = 'pgokf'::pg_catalog.regnamespace"
        }
    };
    let query = format!(
        "SELECT pg_catalog.count(*) > 0
         FROM pg_catalog.pg_index i
         JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid
         JOIN pg_catalog.pg_am am ON am.oid = ic.relam
         WHERE i.indrelid = 'pgokf.concepts'::pg_catalog.regclass
           AND am.amname = 'bm25' {by_name}"
    );
    Spi::connect(|client| {
        client
            .select(&query, Some(1), &[BM25_INDEX_NAME.into()])
            .map_err(spi_error(
                "failed to check for a bm25 index on pgokf.concepts",
            ))?
            .first()
            .get_one::<bool>()
            .map_err(spi_error("failed to read the bm25 index probe"))
    })?
    .ok_or_else(|| CatalogError::internal("bm25 index probe returned no row", Path::new("")))
}

/// (Re)build the `bm25` index on `pgokf.concepts` with the resolved provider,
/// or report the no-op.
///
/// Returns `true` when the index was (re)built and `false` when no provider is
/// installed for the configured `bm25_provider` (a logged no-op). Runs entirely
/// through dynamic SQL over fixed, input-free statements so the extension never
/// statically references either provider.
fn rebuild() -> Result<bool, CatalogError> {
    crate::security::authorize_current_user(crate::security::Operation::Register, Path::new(""))?;
    let configured = configured_provider()?;
    let Some(provider) = resolve_provider(&configured)? else {
        pgrx::notice!(
            "pgokf: {}; rebuild_search_index is a no-op. Install the provider (and set \
             search_backend to 'bm25') to enable BM25 search.",
            describe_missing_provider(&configured)?
        );
        return Ok(false);
    };

    // Fixed identifiers, no caller input; drop-then-create so the function is
    // idempotent and safe to re-run after a schema or tokenizer change.
    Spi::run(&format!("DROP INDEX IF EXISTS pgokf.{BM25_INDEX_NAME}")).map_err(|error| {
        CatalogError::internal(
            format!("failed to drop existing bm25 index: {error}"),
            Path::new(""),
        )
    })?;
    let create = match provider {
        // The key_field is `id`: paradedb.score scores per scanned tuple (by
        // ctid), so the non-global-uniqueness of `id` across bundles does not
        // affect ranking or visibility, and no surrogate key column is imposed
        // on the core table.
        Bm25Provider::PgSearch => format!(
            "CREATE INDEX {BM25_INDEX_NAME} ON pgokf.concepts \
             USING bm25 (id, title, description, body_text, type) \
             WITH (key_field='id')"
        ),
        // One expression index over title, description, and body, tokenized
        // with the catalog's text-search configuration (baked into the index,
        // so a configuration change needs a rebuild).
        Bm25Provider::PgTextsearch { .. } => format!(
            "CREATE INDEX {BM25_INDEX_NAME} ON pgokf.concepts \
             USING bm25 ({TEXTSEARCH_INDEX_EXPRESSION}) \
             WITH (text_config = {})",
            quote_literal(&textsearch_config()?)
        ),
    };
    Spi::run(&create).map_err(|error| {
        CatalogError::internal(
            format!("failed to create bm25 index: {error}"),
            Path::new(""),
        )
    })?;
    Ok(true)
}

/// The catalog's `default_text_search_config`, passed to `pg_textsearch`'s
/// `text_config` index option exactly as configured. The provider resolves the
/// value as a (possibly schema-qualified) configuration name the same way
/// `regconfig` does, so `pg_catalog.english` and `english` both work and a
/// configuration outside `pg_catalog` keeps its qualifier.
fn textsearch_config() -> Result<String, CatalogError> {
    Spi::get_one::<String>("SELECT pgokf.get_config() ->> 'default_text_search_config'")
        .map_err(spi_error("failed to read default_text_search_config"))?
        .ok_or_else(|| {
            CatalogError::internal("default_text_search_config is missing", Path::new(""))
        })
}

/// Quote a text value as a SQL string literal for interpolation into DDL.
fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// The `SECURITY DEFINER` BM25 hit query (see `BM25_HITS_CALL` for why).
//
// `plpgsql` on purpose: its body is not resolved until first execution, so
// `CREATE EXTENSION pgokf` succeeds on a server without `pg_search` (the
// runtime-only coupling this module guarantees), and [`Bm25Backend::search`]
// only calls it after confirming the extension and its index exist. The
// `search_path` is pinned to `pg_catalog` - `pg_search` installs its `@@@`
// operators there and every `paradedb.*` object is schema-qualified - so a
// caller cannot hijack a name resolved under the owner's privileges. The
// tenant predicate is the one the row-level-security policies inline, applied
// here explicitly because the owner bypasses the policies. `EXECUTE` is
// granted to `pgokf_reader` only, the tier `concept_search` itself requires;
// direct calls return the same rows `concept_search` would.
pgrx::extension_sql!(
    r"
CREATE FUNCTION pgokf.bm25_hits(
    p_query text,
    p_bundle_id bigint,
    p_limit bigint,
    p_text_search_config text,
    p_concept_type text,
    p_tags text[],
    p_status text,
    p_trust_tier text,
    p_after_rank real,
    p_after_bundle_id bigint,
    p_after_concept_id text)
RETURNS TABLE (
    bundle_id bigint,
    concept_id text,
    path text,
    title text,
    type text,
    rank real,
    headline text)
LANGUAGE plpgsql
STABLE PARALLEL SAFE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    -- Resolved once, up front, and bound into the query as a parameter: the
    -- policies inline current_setting() directly, but pg_search 0.25 cannot
    -- plan its scan under a predicate that calls a function (that inline
    -- form is exactly what row-level security injects for non-owners, and
    -- exactly what the Unsupported-query-shape error was about). Empty = unset.
    v_tenant text := NULLIF(pg_catalog.current_setting('pgokf.tenant', true), '');
BEGIN
    -- The policies' rule, applied here because this body bypasses them: an
    -- unscoped session sees nothing when the catalog requires a tenant.
    IF v_tenant IS NULL AND pgokf.tenant_required() THEN
        RETURN;
    END IF;
    RETURN QUERY
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
                   p_text_search_config::pg_catalog.regconfig,
                   pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
                   pg_catalog.websearch_to_tsquery(p_text_search_config::pg_catalog.regconfig, p_query)) AS headline
        FROM pgokf.concepts c
        JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
        LEFT JOIN pgokf.concept_provenance cp
               ON cp.bundle_id = c.bundle_id AND cp.concept_id = c.id
        WHERE c.id @@@ paradedb.boolean(should => ARRAY[
                  paradedb.match('title', p_query),
                  paradedb.match('description', p_query),
                  paradedb.match('body_text', p_query)])
          AND (v_tenant IS NULL OR c.tenant_id = v_tenant)
          AND (p_bundle_id IS NULL OR c.bundle_id = p_bundle_id)
          AND (p_concept_type IS NULL OR c.type = p_concept_type)
          AND (p_tags IS NULL OR c.tags @> p_tags)
          AND (p_status IS NULL OR cp.status = p_status)
          AND (p_trust_tier IS NULL OR cp.trust_tier = p_trust_tier)
    ) AS hits
    WHERE p_after_rank IS NULL
       OR hits.rank < p_after_rank
       OR (hits.rank = p_after_rank AND hits.bundle_id > p_after_bundle_id)
       OR (hits.rank = p_after_rank AND hits.bundle_id = p_after_bundle_id AND hits.concept_id > p_after_concept_id)
    ORDER BY hits.rank DESC, hits.bundle_id ASC, hits.concept_id ASC
    LIMIT p_limit;
END
$fn$;

REVOKE ALL ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text) IS
    'Internal helper behind concept_search when search_backend = bm25 resolves to the ParadeDB pg_search provider (the pg_textsearch provider runs inline with invoker rights and does not use it); not part of the stable API. Runs the ParadeDB pg_search BM25 hit query with the owner''s privileges (row-level security wraps the catalog tables in a shape pg_search cannot plan for non-owners) while applying the same pgokf.tenant scoping the policies enforce, over active bundles only, with concept_search''s filters, keyset cursor, and limit. Reader-level; returns exactly the rows concept_search would.';
",
    name = "bm25_hits_function",
    requires = ["catalog_tables", "provenance_table"]
);

/// SQL-facing BM25 index management, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{extension_sql, pg_extern};

    use super::rebuild;

    /// (Re)build the BM25 search index on `pgokf.concepts`.
    ///
    /// Requires membership in `pgokf_admin`. When the provider `bm25_provider`
    /// resolves to (Tiger Data `pg_textsearch` or `ParadeDB` `pg_search`) is
    /// installed this drops and recreates the `bm25` index used by
    /// `search_backend = 'bm25'` - dropping an index the other provider built
    /// first - and returns `true`. When no usable provider is installed it is
    /// a no-op that emits a `NOTICE` and returns `false`. Run it after
    /// enabling the `bm25` backend, after changing `bm25_provider` or
    /// `default_text_search_config` (the `pg_textsearch` index bakes the
    /// configuration in), and after a bulk re-sync if you want the index
    /// rebuilt from scratch (incremental sync maintains it automatically once
    /// it exists).
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
    'Admin-only. (Re)build the bm25 index on pgokf.concepts used by search_backend=bm25, with the provider the bm25_provider policy resolves to (pg_textsearch, or ParadeDB pg_search); returns true when built, or false (with a NOTICE) when no provider is installed.';
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
    fn is_supported_provider_accepts_the_three_provider_values() {
        // Arrange & Act & Assert
        assert!(is_supported_provider(PROVIDER_AUTO));
        assert!(is_supported_provider(PROVIDER_PG_SEARCH));
        assert!(is_supported_provider(PROVIDER_PG_TEXTSEARCH));
    }

    #[test]
    fn is_supported_provider_rejects_unknown_provider() {
        // Arrange & Act & Assert
        assert!(!is_supported_provider("bm25"));
        assert!(!is_supported_provider(""));
    }

    #[test]
    fn textsearch_candidate_query_is_an_index_ordered_top_k_scan() {
        // Arrange
        let schema = "public";

        // Act
        let query = textsearch_candidate_query(schema);

        // Assert: the ORDER BY is the bare index operator expression (what the
        // access method plans as an index scan), the query text is a bound
        // parameter, the index is qualified with pgokf, the limit seeds the
        // provider's top-k with the tie-closure slack, and no score
        // comparison filters the scan (the provider's documented anti-pattern).
        let score = format!(
            "({TEXTSEARCH_EXPRESSION} OPERATOR(\"public\".<@>) \"public\".to_bm25query($1, 'pgokf.concepts_bm25_idx'))"
        );
        assert!(query.contains(&format!("ORDER BY {score}\n")));
        assert!(query.contains(&format!("LIMIT $3 + {TEXTSEARCH_TIE_CLOSURE}")));
        assert!(
            query.contains("OR (-(")
                && query
                    .contains(")::pg_catalog.float4 = $9 AND c.bundle_id = $10 AND c.id > $11)")
        );
        assert!(!query.contains("< 0"));
    }

    #[test]
    fn textsearch_projection_orders_in_sql_before_joining_back() {
        // Arrange & Act & Assert: the tiebreak and page cut happen over the
        // unnested candidates, the join back to concepts comes after.
        let order = "ORDER BY k.rank DESC, k.bundle_id ASC, k.concept_id ASC\n        LIMIT $6";
        let join =
            "JOIN pgokf.concepts c ON c.bundle_id = page.bundle_id AND c.id = page.concept_id";
        let order_at = TEXTSEARCH_PROJECTION_QUERY
            .find(order)
            .expect("ordering step present");
        let join_at = TEXTSEARCH_PROJECTION_QUERY
            .find(join)
            .expect("join back present");
        assert!(order_at < join_at);
    }

    fn candidate(bundle_id: i64, concept_id: &str, rank: f32) -> Candidate {
        Candidate {
            bundle_id,
            concept_id: concept_id.to_owned(),
            rank,
        }
    }

    #[test]
    fn page_collector_stops_after_the_page_when_the_next_rank_differs() {
        // Arrange
        let mut page = PageCollector::new(2);

        // Act
        let wants = [
            page.push(candidate(1, "a", 3.0)),
            page.push(candidate(1, "b", 2.0)),
            page.push(candidate(1, "c", 1.5)),
        ];

        // Assert
        assert_eq!(wants, [true, true, false]);
        assert_eq!(page.rows.len(), 2);
        assert!(!page.truncated());
    }

    #[test]
    fn page_collector_keeps_every_row_tied_with_the_boundary_rank() {
        // Arrange
        let mut page = PageCollector::new(2);

        // Act
        page.push(candidate(1, "a", 3.0));
        page.push(candidate(1, "z", 2.0));
        let tied = page.push(candidate(1, "b", 2.0));
        let after_band = page.push(candidate(1, "c", 1.0));

        // Assert: the tie is collected, the first different rank ends the page.
        assert!(tied);
        assert!(!after_band);
        assert_eq!(page.rows.len(), 3);
        assert!(!page.truncated());
    }

    #[test]
    fn page_collector_caps_the_tie_band_and_reports_truncation() {
        // Arrange
        let cap = usize::try_from(TEXTSEARCH_TIE_CLOSURE).expect("cap fits usize");
        let mut page = PageCollector::new(1);
        page.push(candidate(1, "first", 1.0));

        // Act: feed cap tied rows (all accepted), then one more.
        let accepted = (0..cap).all(|i| page.push(candidate(1, &format!("t{i}"), 1.0)));
        let overflow = page.push(candidate(1, "overflow", 1.0));

        // Assert
        assert!(accepted);
        assert!(!overflow);
        assert!(page.truncated());
        assert_eq!(page.rows.len(), 1 + cap);
    }

    #[test]
    fn page_collector_stops_at_a_non_positive_rank() {
        // Arrange
        let mut page = PageCollector::new(3);

        // Act
        let first = page.push(candidate(1, "a", 0.5));
        let non_match = page.push(candidate(1, "b", 0.0));

        // Assert: a non-match ends the page even before it is full.
        assert!(first);
        assert!(!non_match);
        assert_eq!(page.into_rows().len(), 1);
    }

    #[test]
    fn quote_identifier_and_literal_escape_their_delimiters() {
        // Arrange & Act & Assert
        assert_eq!(quote_identifier("my\"schema"), "\"my\"\"schema\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
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
