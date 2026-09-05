// SPDX-License-Identifier: AGPL-3.0-only
//! Observability surface: `catalog_stats`, `health`, and `stale_concepts`.
//!
//! These three reader-level functions expose the operational state of the
//! catalog for monitoring and readiness routing, without any mutation:
//!
//! - [`catalog_stats`](pgokf::catalog_stats) - one row per registered bundle
//!   with its indexed-concept, link, and resolved-link counts, sync recency,
//!   and a staleness flag. It reads only `pgokf.bundles`, `pgokf.concepts`, and
//!   `pgokf.links`, all of which `pgokf_reader` already holds `SELECT` on, so it
//!   runs with **invoker rights** (like `concept_search` / `list_bundles`) - no
//!   `SECURITY DEFINER` is warranted because escalating would grant nothing.
//! - [`health`](pgokf::health) - a single `jsonb` document for
//!   liveness/readiness probes (bundle and concept totals, the configured
//!   search backend and whether BM25 is ready, replica-recovery state, and role
//!   and configuration sanity). It must read the administrator-only
//!   `pgokf_private.config`, so it is `SECURITY DEFINER` with a pinned
//!   `search_path`, mirroring `get_config`; `EXECUTE` is granted to
//!   `pgokf_reader`.
//! - [`stale_concepts`](pgokf::stale_concepts) - the concepts whose OKF
//!   `stale_after` instant has passed as of a chosen time, surfacing the
//!   lifecycle signal that `pgokf.concept_provenance` already models but never
//!   exposed. It reads only reader-granted tables, so it too runs with invoker
//!   rights.
//!
//! All three are `STABLE`: they observe committed catalog state and never write.

use std::path::Path;

use pgrx::datum::{Interval, TimestampWithTimeZone};
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi};

use crate::catalog::spi_read::RowReader;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the per-bundle statistics composite type.
const CATALOG_STAT_TYPE: &str = "pgokf.catalog_stat";
/// Qualified SQL name of the stale-concept composite type.
const STALE_CONCEPT_TYPE: &str = "pgokf.stale_concept";

/// Staleness threshold: a bundle is flagged stale when its last successful sync
/// is older than this. A fixed, conservative 24-hour window keeps the signal
/// simple and dependency-free (no extra configuration key).
const STALE_AFTER_INTERVAL: &str = "24 hours";

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

fn composite_error(type_name: &str, error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {type_name} composite: {error}"),
        Path::new(""),
    )
}

// ---------------------------------------------------------------------------
// catalog_stats
// ---------------------------------------------------------------------------

/// One bundle's operational statistics, prior to being packed into the
/// `pgokf.catalog_stat` composite.
struct CatalogStat {
    bundle_id: i64,
    name: Option<String>,
    enabled: bool,
    source_type: String,
    file_count: i32,
    indexed_concepts: i64,
    link_count: i64,
    resolved_link_count: i64,
    last_synced_at: Option<TimestampWithTimeZone>,
    sync_age: Option<Interval>,
    is_stale: bool,
    retired_at: Option<TimestampWithTimeZone>,
}

const CATALOG_STATS_QUERY: &str = "
    SELECT b.id,
           b.name,
           b.enabled,
           b.source_type,
           b.file_count,
           (SELECT pg_catalog.count(*) FROM pgokf.concepts c WHERE c.bundle_id = b.id),
           (SELECT pg_catalog.count(*) FROM pgokf.links l WHERE l.bundle_id = b.id),
           (SELECT pg_catalog.count(*) FROM pgokf.links l
            WHERE l.bundle_id = b.id AND l.resolved),
           b.last_synced_at,
           (pg_catalog.now() - b.last_synced_at),
           COALESCE(
               b.last_synced_at < pg_catalog.now() - $1::pg_catalog.interval, false),
           b.retired_at
    FROM pgokf.bundles b
    ORDER BY b.id";

fn read_catalog_stat(row: &pgrx::spi::SpiHeapTupleData<'_>) -> Result<CatalogStat, CatalogError> {
    let reader = RowReader::new(row, "failed to read catalog_stat column", "catalog_stat");
    Ok(CatalogStat {
        bundle_id: reader.required(1, "bundle_id")?,
        name: reader.optional(2)?,
        enabled: reader.required(3, "enabled")?,
        source_type: reader.required(4, "source_type")?,
        file_count: reader.required(5, "file_count")?,
        indexed_concepts: reader.required(6, "indexed_concepts")?,
        link_count: reader.required(7, "link_count")?,
        resolved_link_count: reader.required(8, "resolved_link_count")?,
        last_synced_at: reader.optional::<TimestampWithTimeZone>(9)?,
        sync_age: reader.optional::<Interval>(10)?,
        is_stale: reader.required(11, "is_stale")?,
        retired_at: reader.optional::<TimestampWithTimeZone>(12)?,
    })
}

fn catalog_stat_tuple(
    stat: CatalogStat,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type(CATALOG_STAT_TYPE)
        .map_err(|error| composite_error(CATALOG_STAT_TYPE, error))?;
    let set = |error| composite_error(CATALOG_STAT_TYPE, error);
    tuple
        .set_by_name("bundle_id", stat.bundle_id)
        .map_err(set)?;
    tuple.set_by_name("name", stat.name).map_err(set)?;
    tuple.set_by_name("enabled", stat.enabled).map_err(set)?;
    tuple
        .set_by_name("source_type", stat.source_type)
        .map_err(set)?;
    tuple
        .set_by_name("file_count", stat.file_count)
        .map_err(set)?;
    tuple
        .set_by_name("indexed_concepts", stat.indexed_concepts)
        .map_err(set)?;
    tuple
        .set_by_name("link_count", stat.link_count)
        .map_err(set)?;
    tuple
        .set_by_name("resolved_link_count", stat.resolved_link_count)
        .map_err(set)?;
    tuple
        .set_by_name("last_synced_at", stat.last_synced_at)
        .map_err(set)?;
    tuple.set_by_name("sync_age", stat.sync_age).map_err(set)?;
    tuple.set_by_name("is_stale", stat.is_stale).map_err(set)?;
    tuple
        .set_by_name("retired_at", stat.retired_at)
        .map_err(set)?;
    Ok(tuple)
}

fn catalog_stats_impl() -> Result<Vec<CatalogStat>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    Spi::connect(|client| {
        let table = client
            .select(CATALOG_STATS_QUERY, None, &[STALE_AFTER_INTERVAL.into()])
            .map_err(|error| spi_error("failed to read catalog stats", &error))?;
        let mut stats = Vec::with_capacity(table.len());
        for row in table {
            stats.push(read_catalog_stat(&row)?);
        }
        Ok(stats)
    })
}

// ---------------------------------------------------------------------------
// health
// ---------------------------------------------------------------------------

/// Build the health document (`$1` binds the pre-computed `bm25_ready`).
/// `SECURITY DEFINER` at the SQL layer, so it may read `pgokf_private.config`. Because `SECURITY DEFINER` bypasses row-level
/// security, the bundle and concept counts apply the same opt-in tenant filter
/// explicitly: a session that set `pgokf.tenant` sees only its own counts, while
/// an unset session counts every row (backward compatible). The role and config
/// checks are cluster-global, not tenant data, so they are never scoped.
const HEALTH_QUERY: &str = "
    WITH h AS (
        SELECT
            (SELECT pg_catalog.count(*) = 3 FROM pg_catalog.pg_roles
             WHERE rolname IN ('pgokf_reader', 'pgokf_writer', 'pgokf_admin')) AS roles_ok,
            (SELECT pg_catalog.count(*) = 1 FROM pgokf_private.config) AS config_ok,
            (SELECT pg_catalog.count(*) FROM pgokf.bundles
             WHERE pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                OR pg_catalog.current_setting('pgokf.tenant', true) = ''
                OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true)) AS bundle_count,
            (SELECT pg_catalog.count(*) FROM pgokf.concepts
             WHERE pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                OR pg_catalog.current_setting('pgokf.tenant', true) = ''
                OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true)) AS concept_count,
            (SELECT search_backend FROM pgokf_private.config WHERE singleton) AS search_backend,
            $1::pg_catalog.bool AS bm25_ready,
            pg_catalog.pg_is_in_recovery() AS in_recovery
    )
    SELECT pg_catalog.jsonb_build_object(
        'ok', roles_ok AND config_ok,
        'bundle_count', bundle_count,
        'concept_count', concept_count,
        'search_backend', search_backend,
        'bm25_ready', bm25_ready,
        'in_recovery', in_recovery,
        'roles_ok', roles_ok,
        'config_ok', config_ok)
    FROM h";

fn health_impl() -> Result<pgrx::JsonB, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let bm25_ready = bm25_ready()?;
    Spi::get_one_with_args::<pgrx::JsonB>(HEALTH_QUERY, &[bm25_ready.into()])
        .map_err(|error| spi_error("failed to read catalog health", &error))?
        .ok_or_else(|| CatalogError::internal("health query returned no row", Path::new("")))
}

/// Whether `search_backend = bm25` would serve rather than fall back: the
/// provider the `bm25_provider` policy resolves to is installed *and* the
/// index that provider's query needs exists - the same two checks the
/// backend makes, so `health()` and `search_index_status()` never disagree.
fn bm25_ready() -> Result<bool, CatalogError> {
    use crate::catalog::search_backend::{
        bm25_index_present, configured_provider, resolve_provider,
    };
    match resolve_provider(&configured_provider()?)? {
        Some(provider) => bm25_index_present(&provider),
        None => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// stale_concepts
// ---------------------------------------------------------------------------

/// One concept whose `stale_after` instant has passed, prior to being packed
/// into the `pgokf.stale_concept` composite.
struct StaleConcept {
    bundle_id: i64,
    concept_id: String,
    path: String,
    concept_type: Option<String>,
    stale_after: TimestampWithTimeZone,
}

const STALE_CONCEPTS_QUERY: &str = "
    SELECT p.bundle_id, p.concept_id, c.path, c.type, p.stale_after
    FROM pgokf.concept_provenance p
    JOIN pgokf.concepts c ON c.bundle_id = p.bundle_id AND c.id = p.concept_id
    WHERE p.stale_after IS NOT NULL
      AND p.stale_after < COALESCE($2, pg_catalog.now())
      AND ($1::bigint IS NULL OR p.bundle_id = $1)
    ORDER BY p.stale_after, p.bundle_id, p.concept_id";

fn read_stale_concept(row: &pgrx::spi::SpiHeapTupleData<'_>) -> Result<StaleConcept, CatalogError> {
    let reader = RowReader::new(row, "failed to read stale_concept column", "stale_concept");
    Ok(StaleConcept {
        bundle_id: reader.required(1, "bundle_id")?,
        concept_id: reader.required(2, "concept_id")?,
        path: reader.required(3, "path")?,
        concept_type: reader.optional(4)?,
        stale_after: reader.required::<TimestampWithTimeZone>(5, "stale_after")?,
    })
}

fn stale_concept_tuple(
    concept: StaleConcept,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type(STALE_CONCEPT_TYPE)
        .map_err(|error| composite_error(STALE_CONCEPT_TYPE, error))?;
    let set = |error| composite_error(STALE_CONCEPT_TYPE, error);
    tuple
        .set_by_name("bundle_id", concept.bundle_id)
        .map_err(set)?;
    tuple
        .set_by_name("concept_id", concept.concept_id)
        .map_err(set)?;
    tuple.set_by_name("path", concept.path).map_err(set)?;
    tuple
        .set_by_name("concept_type", concept.concept_type)
        .map_err(set)?;
    tuple
        .set_by_name("stale_after", concept.stale_after)
        .map_err(set)?;
    Ok(tuple)
}

fn stale_concepts_impl(
    bundle_id: Option<i64>,
    as_of: Option<TimestampWithTimeZone>,
) -> Result<Vec<StaleConcept>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    Spi::connect(|client| {
        let table = client
            .select(
                STALE_CONCEPTS_QUERY,
                None,
                &[bundle_id.into(), as_of.into()],
            )
            .map_err(|error| spi_error("failed to read stale concepts", &error))?;
        let mut concepts = Vec::with_capacity(table.len());
        for row in table {
            concepts.push(read_stale_concept(&row)?);
        }
        Ok(concepts)
    })
}

/// SQL-facing observability entry points, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::datum::TimestampWithTimeZone;
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{
        catalog_stat_tuple, catalog_stats_impl, health_impl, stale_concept_tuple,
        stale_concepts_impl,
    };

    extension_sql!(
        r"
CREATE TYPE pgokf.catalog_stat AS (
    bundle_id           bigint,
    name                text,
    enabled             boolean,
    source_type         text,
    file_count          integer,
    indexed_concepts    bigint,
    link_count          bigint,
    resolved_link_count bigint,
    last_synced_at      timestamptz,
    sync_age            interval,
    is_stale            boolean,
    retired_at          timestamptz
);

CREATE TYPE pgokf.stale_concept AS (
    bundle_id    bigint,
    concept_id   text,
    path         text,
    concept_type text,
    stale_after  timestamptz
);

COMMENT ON TYPE pgokf.catalog_stat IS
    'Per-bundle operational statistics from pgokf.catalog_stats: identity and state, indexed-concept / link / resolved-link counts, sync recency (last_synced_at, sync_age), a 24-hour staleness flag, and retired_at (the soft-delete/retirement instant, NULL when active) so retired bundles - hidden from list_bundles - remain visible here.';
COMMENT ON TYPE pgokf.stale_concept IS
    'One concept whose OKF stale_after instant has passed (as of the chosen time), from pgokf.stale_concepts: its bundle, id, path, type, and the stale_after instant.';
",
        name = "stats_types",
        requires = ["catalog_tables", "provenance_table"]
    );

    /// Per-bundle catalog statistics for monitoring.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Returns one row
    /// per registered bundle with its indexed-concept, link, and resolved-link
    /// counts, sync recency, and a staleness flag (`is_stale` is true when the
    /// last sync is more than 24 hours old).
    #[pg_extern(stable, parallel_safe, requires = ["stats_types"])]
    fn catalog_stats()
    -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.catalog_stat")> {
        let stats = catalog_stats_impl().unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = stats
            .into_iter()
            .map(|stat| catalog_stat_tuple(stat).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    /// Return a catalog health document for liveness/readiness probes.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). The `jsonb`
    /// document reports `ok`, `bundle_count`, `concept_count`, `search_backend`,
    /// `bm25_ready`, `in_recovery` (replica routing), `roles_ok`, and
    /// `config_ok`.
    #[pg_extern(stable, requires = ["catalog_tables"])]
    fn health() -> pgrx::JsonB {
        health_impl().unwrap_or_else(|error| error.raise())
    }

    /// List concepts whose OKF `stale_after` instant has passed.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Returns
    /// concepts whose `stale_after` is earlier than `as_of` (or `now()` when
    /// `as_of` is `NULL`), optionally scoped to one `bundle_id`.
    #[pg_extern(stable, parallel_safe, requires = ["stats_types"])]
    fn stale_concepts(
        bundle_id: default!(Option<i64>, "NULL"),
        as_of: default!(Option<TimestampWithTimeZone>, "NULL"),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.stale_concept")> {
        let concepts = stale_concepts_impl(bundle_id, as_of).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = concepts
            .into_iter()
            .map(|concept| stale_concept_tuple(concept).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.health()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.catalog_stats() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.catalog_stats() TO pgokf_reader;
REVOKE ALL ON FUNCTION pgokf.health() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.health() TO pgokf_reader;
REVOKE ALL ON FUNCTION pgokf.stale_concepts(bigint, timestamptz) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.stale_concepts(bigint, timestamptz) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.catalog_stats() IS
    'Per-bundle operational statistics (indexed-concept/link/resolved-link counts, sync recency, 24h staleness flag) as pgokf.catalog_stat. Reader-level, STABLE, invoker rights over reader-granted tables.';
COMMENT ON FUNCTION pgokf.health() IS
    'Catalog health document (jsonb) for liveness/readiness probes: ok, bundle_count, concept_count, search_backend, bm25_ready, in_recovery, roles_ok, config_ok. Reader-level, STABLE, SECURITY DEFINER (reads the admin-only config).';
COMMENT ON FUNCTION pgokf.stale_concepts(bigint, timestamptz) IS
    'List concepts whose OKF stale_after instant has passed as of the given time (or now()), as pgokf.stale_concept, optionally scoped to one bundle. Reader-level, STABLE, invoker rights over reader-granted tables.';
",
        name = "stats_function_hardening",
        requires = [catalog_stats, health, stale_concepts]
    );
}
