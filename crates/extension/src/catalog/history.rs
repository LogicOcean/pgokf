// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in temporal concept version history (`pgokf.concept_history`).
//!
//! # SCD Type-2 temporal history, opt-in for backward compatibility
//!
//! When the durable `track_history` configuration key is enabled, every
//! register/refresh/content sync records an append-only Slowly-Changing-Dimension
//! Type-2 trail of each changed concept into `pgokf.concept_history`, so an
//! operator can answer *"what did this runbook say last Tuesday?"* with a
//! point-in-time query. When the key is **off** (the default), this module
//! records nothing: an existing install, and any bundle synced with history
//! disabled, behaves exactly as before with zero extra storage. This is what
//! keeps the feature backward compatible.
//!
//! # The version chain
//!
//! Each concept has a per-concept monotonic `version` and a validity interval
//! `[valid_from, valid_to)` (`valid_to IS NULL` == the single current open
//! version). The sync engine already re-parses only content-changed files, so an
//! `updated` row always corresponds to a real change and an unchanged file
//! produces no history row. [`project`] enforces the SCD-2 invariants inside the
//! sync transaction, from the same delta the projection seam consumes:
//!
//! - **added** concept: insert version 1, `valid_from = now()`,
//!   `valid_to = NULL`, `change_kind = 'added'`, snapshotting the new core.
//! - **updated** concept: close the current open version (`valid_to = now()`)
//!   and insert `version = prev + 1`, `valid_from = now()`, `valid_to = NULL`,
//!   `change_kind = 'updated'`, snapshotting the new core.
//! - **removed** concept: close the current open version (`valid_to = now()`)
//!   and insert a zero-width tombstone `version = prev + 1`,
//!   `valid_from = valid_to = now()`, `change_kind = 'removed'` with a NULL core
//!   snapshot - the last real content stays in the closed prior version, and an
//!   as-of query at or after the removal instant returns no row.
//!
//! Because every statement of one sync binds a single captured instant
//! ([`capture_sync_instant`], a `clock_timestamp()` read once per [`project`]),
//! the closed version's `valid_to` equals the new version's `valid_from`, so
//! intervals stay contiguous and non-overlapping and exactly one open version
//! exists per live concept, while successive syncs advance in time. Enabling
//! `track_history` mid-life is
//! safe: a concept first versioned afterward simply begins its chain at the
//! sync's `change_kind` (its `prev` is 0), and the invariants hold from there
//! forward.
//!
//! # Seam contract
//!
//! Everything lives in this file only. The sync engine calls [`project`] (and
//! then [`prune`]) inside the advisory-locked atomic transaction, only when
//! `track_history` is on, so history commits atomically with the sync. Removals
//! need no core read: the tombstone snapshot is NULL, and the FK is to
//! `pgokf.bundles` (not `pgokf.concepts`) so a removed concept keeps its history
//! until the bundle is unregistered.
//!
//! # Retention
//!
//! [`prune`] bounds growth: when `history_retention_days > 0`, closed versions
//! (`valid_to IS NOT NULL`) whose `valid_to` predates the window are deleted in
//! the same transaction. The single current open version of each concept
//! (`valid_to IS NULL`) is never pruned, so point-in-time queries of the present
//! always resolve.
//!
//! # Reader surface
//!
//! [`concept_history`](pgokf::concept_history) returns a concept's version
//! timeline newest-first, and [`concept_as_of`](pgokf::concept_as_of) returns the
//! single version valid at an instant. Both are reader-level, `STABLE`, and run
//! with invoker rights over the public projection table, so the caller's own
//! opt-in tenant row-level security applies - matching `pgokf.concept_neighbors`
//! and `pgokf.list_bundle_log`.

use std::collections::BTreeMap;
use std::path::Path;

use pgrx::datum::TimestampWithTimeZone;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::batch::{self, BATCH_SIZE};
use crate::catalog::spi_read::RowReader;
use crate::catalog::types::StagedConcept;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the version-row composite type.
const CONCEPT_VERSION_TYPE: &str = "pgokf.concept_version";

extension_sql!(
    r"
CREATE TABLE pgokf.concept_history (
    bundle_id   bigint      NOT NULL,
    concept_id  text        NOT NULL,
    tenant_id   text        NOT NULL DEFAULT 'default',
    version     bigint      NOT NULL,
    valid_from  timestamptz NOT NULL,
    valid_to    timestamptz,
    change_kind text        NOT NULL,
    type        text,
    title       text,
    description text,
    tags        text[],
    resource    jsonb,
    body_text   text,
    file_hash   text,
    CONSTRAINT concept_history_pkey PRIMARY KEY (bundle_id, concept_id, version),
    CONSTRAINT concept_history_change_kind_chk
        CHECK (change_kind IN ('added', 'updated', 'removed')),
    CONSTRAINT concept_history_bundle_fk
        FOREIGN KEY (bundle_id)
        REFERENCES pgokf.bundles (id)
        ON DELETE CASCADE
);

-- Point-in-time and timeline lookups are keyed by (bundle_id, concept_id) and
-- filter/order on valid_from, so a composite index over exactly that serves both
-- concept_history (timeline) and concept_as_of (as-of instant).
CREATE INDEX concept_history_lookup_idx
    ON pgokf.concept_history (bundle_id, concept_id, valid_from);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER sync path bypasses
-- it to record a single-tenant bundle's history, while the invoker-rights reader
-- functions honor the caller's own pgokf.tenant.
ALTER TABLE pgokf.concept_history ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_history_tenant_isolation ON pgokf.concept_history
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

COMMENT ON TABLE pgokf.concept_history IS
    'Opt-in append-only SCD Type-2 version trail of each concept, populated only when the track_history configuration key is enabled. Each row is one version with a validity interval [valid_from, valid_to) (valid_to IS NULL = the current open version); versions are per-concept monotonic and contiguous. Cascades from pgokf.bundles (NOT pgokf.concepts), so a removed concept keeps its history until the bundle is unregistered. Read through pgokf.concept_history(bundle_id, concept_id, max_rows) and pgokf.concept_as_of(bundle_id, concept_id, as_of).';
COMMENT ON COLUMN pgokf.concept_history.bundle_id IS
    'Bundle the versioned concept belongs to (references pgokf.bundles.id; ON DELETE CASCADE).';
COMMENT ON COLUMN pgokf.concept_history.concept_id IS
    'The versioned concept''s path-derived OKF id. Retained across the concept''s deletion (the FK is to the bundle, not the concept), so a removed concept''s history survives until the bundle is unregistered.';
COMMENT ON COLUMN pgokf.concept_history.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.concept_history.version IS
    'Per-concept monotonic version number (1 for the first recorded version, prev+1 for each subsequent one). Part of the primary key with (bundle_id, concept_id).';
COMMENT ON COLUMN pgokf.concept_history.valid_from IS
    'Instant this version became valid (the sync transaction now()). Equals the prior version''s valid_to, so intervals are contiguous.';
COMMENT ON COLUMN pgokf.concept_history.valid_to IS
    'Instant this version stopped being valid, or NULL for the single current open version of a live concept. A removal tombstone is zero-width (valid_from = valid_to), so an as-of query at or after the removal instant returns no row.';
COMMENT ON COLUMN pgokf.concept_history.change_kind IS
    'What produced this version: added (version 1 of a new concept), updated (a content change), or removed (a zero-width tombstone marking deletion).';
COMMENT ON COLUMN pgokf.concept_history.type IS
    'Snapshot of the concept''s OKF type at this version; NULL for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.title IS
    'Snapshot of the concept''s title at this version; NULL for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.description IS
    'Snapshot of the concept''s description at this version; NULL when the concept had none, or for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.tags IS
    'Snapshot of the concept''s tags at this version; NULL for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.resource IS
    'Snapshot of the concept''s resource declaration (as jsonb) at this version; NULL when the concept had none, or for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.body_text IS
    'Snapshot of the concept''s search-indexed plain-text body at this version; NULL for a removal tombstone.';
COMMENT ON COLUMN pgokf.concept_history.file_hash IS
    'Snapshot of the concept''s source-file BLAKE3 digest at this version; NULL for a removal tombstone.';

GRANT SELECT ON pgokf.concept_history TO pgokf_reader;
",
    name = "concept_history_table",
    requires = ["catalog_tables", "config_table"]
);

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Capture the single instant that stamps every row this sync records.
///
/// `clock_timestamp()` (the real wall-clock time), read **once** per [`project`]
/// call and threaded into every close/insert, is deliberately used rather than
/// `now()` (the transaction timestamp): a sync is normally its own transaction, so
/// this equals the sync time either way, but capturing one clock reading and
/// reusing it keeps a version chain's intervals contiguous *within* the sync while
/// still letting successive syncs advance - including several syncs that happen to
/// share one transaction. All of a sync's rows therefore carry the same
/// `valid_from`/closing `valid_to`, so the closed interval abuts the new one
/// exactly.
fn capture_sync_instant() -> Result<TimestampWithTimeZone, CatalogError> {
    Spi::get_one::<TimestampWithTimeZone>("SELECT pg_catalog.clock_timestamp()")
        .map_err(|error| spi_error("failed to capture history sync instant", &error))?
        .ok_or_else(|| CatalogError::internal("clock_timestamp() returned NULL", Path::new("")))
}

/// Close the current open version of every concept named in `concept_ids`.
///
/// Sets `valid_to = now` on the single `valid_to IS NULL` row of each concept. A
/// brand-new (`added`) concept has no open version, so the update simply matches
/// nothing. `now` is the sync instant ([`capture_sync_instant`]), so the closed
/// interval abuts the new version's `valid_from`.
fn close_open_versions(
    bundle_id: i64,
    now: TimestampWithTimeZone,
    concept_ids: &[String],
) -> Result<(), CatalogError> {
    if concept_ids.is_empty() {
        return Ok(());
    }
    Spi::run_with_args(
        "UPDATE pgokf.concept_history h
         SET valid_to = $3
         FROM unnest($2::text[]) AS d(concept_id)
         WHERE h.bundle_id = $1 AND h.concept_id = d.concept_id AND h.valid_to IS NULL",
        &[bundle_id.into(), concept_ids.to_vec().into(), now.into()],
    )
    .map_err(|error| spi_error("failed to close open concept-history versions", &error))
}

/// Record the added/updated versions of one staged chunk.
///
/// Each row's next `version` is computed set-based as `max(existing) + 1` (the
/// pre-statement snapshot, so a concept appearing once per chunk increments
/// exactly once); its `change_kind` is `updated` when the concept's path was
/// already stored before this sync and `added` otherwise, matching the sync's own
/// change manifest. Tags are rebuilt from the flat array with the per-row
/// inclusive `[lo, hi]` window ([`batch::marshal_concepts`]), and the resource
/// JSON text is cast back to `jsonb`. The core is always fully populated for a
/// staged concept (its type and title are required), so no snapshot column is
/// spuriously NULL here.
const INSERT_STAGED_VERSIONS: &str = "
    INSERT INTO pgokf.concept_history
        (bundle_id, tenant_id, concept_id, version, valid_from, valid_to, change_kind,
         type, title, description, tags, resource, body_text, file_hash)
    SELECT
        $1,
        (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
        t.concept_id,
        coalesce((SELECT pg_catalog.max(h.version)
                  FROM pgokf.concept_history h
                  WHERE h.bundle_id = $1 AND h.concept_id = t.concept_id), 0) + 1,
        $13, NULL, t.change_kind,
        t.type, t.title, t.description, t.tags, t.resource::jsonb, t.body_text, t.file_hash
    FROM (
        SELECT
            d.concept_id, d.change_kind, d.type, d.title, d.description,
            d.resource, d.body_text, d.file_hash,
            COALESCE(($7::text[])[d.lo:d.hi], ARRAY[]::text[]) AS tags
        FROM unnest(
                 $2::text[], $3::text[], $4::text[], $5::text[], $6::text[],
                 $10::text[], $11::text[], $12::text[], $8::integer[], $9::integer[])
             AS d(concept_id, change_kind, type, title, description,
                   resource, body_text, file_hash, lo, hi)
    ) AS t";

/// Record the added/updated concept versions of one sync in bounded batches.
///
/// Per chunk: close each concept's open version, then insert its new open
/// version. Both statements bind the same sync instant `now`, so the closed
/// interval abuts the new one.
fn record_staged_versions(
    bundle_id: i64,
    now: TimestampWithTimeZone,
    stored: &BTreeMap<String, String>,
    staged: &[StagedConcept],
) -> Result<(), CatalogError> {
    for chunk in staged.chunks(BATCH_SIZE) {
        let columns = batch::marshal_concepts(chunk);
        // change_kind aligns with marshal_concepts' row order (both iterate the
        // chunk in order): a concept whose path was stored before this sync is an
        // update, otherwise an add - the same rule the sync's change manifest uses.
        let change_kinds: Vec<String> = chunk
            .iter()
            .map(|entry| {
                if stored.contains_key(&entry.concept.path) {
                    "updated".to_owned()
                } else {
                    "added".to_owned()
                }
            })
            .collect();

        close_open_versions(bundle_id, now, &columns.ids)?;

        Spi::run_with_args(
            INSERT_STAGED_VERSIONS,
            &[
                bundle_id.into(),
                columns.ids.into(),
                change_kinds.into(),
                columns.types.into(),
                columns.titles.into(),
                columns.descriptions.into(),
                columns.tags_flat.into(),
                columns.tag_los.into(),
                columns.tag_his.into(),
                columns.resources.into(),
                columns.body_texts.into(),
                columns.file_hashes.into(),
                now.into(),
            ],
        )
        .map_err(|error| spi_error("failed to record concept-history versions", &error))?;
    }
    Ok(())
}

/// Record the removal tombstone of every removed concept of one sync.
///
/// The concept's current open version is closed (`valid_to = now`), and a
/// zero-width tombstone (`valid_from = valid_to = now`, NULL core,
/// `change_kind = 'removed'`) is appended as `version = prev + 1`, so a
/// point-in-time query at or after the removal instant returns no row while the
/// last real content stays in the closed prior version. A concept with no prior
/// recorded version (history was enabled after it existed and it was never
/// updated) simply gets a lone version-1 tombstone documenting the removal.
fn record_removed_tombstones(
    bundle_id: i64,
    now: TimestampWithTimeZone,
    removed_ids: &[String],
) -> Result<(), CatalogError> {
    if removed_ids.is_empty() {
        return Ok(());
    }
    for chunk in removed_ids.chunks(BATCH_SIZE) {
        close_open_versions(bundle_id, now, chunk)?;
        Spi::run_with_args(
            "INSERT INTO pgokf.concept_history
                 (bundle_id, tenant_id, concept_id, version, valid_from, valid_to, change_kind,
                  type, title, description, tags, resource, body_text, file_hash)
             SELECT
                 $1,
                 (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
                 d.concept_id,
                 coalesce((SELECT pg_catalog.max(h.version)
                           FROM pgokf.concept_history h
                           WHERE h.bundle_id = $1 AND h.concept_id = d.concept_id), 0) + 1,
                 $3, $3, 'removed',
                 NULL, NULL, NULL, NULL, NULL, NULL, NULL
             FROM unnest($2::text[]) AS d(concept_id)",
            &[bundle_id.into(), chunk.to_vec().into(), now.into()],
        )
        .map_err(|error| spi_error("failed to record concept-history removals", &error))?;
    }
    Ok(())
}

/// Record the SCD-2 version history of one sync from its delta.
///
/// Called by [`crate::catalog::sync::run_bundle_sync`] inside the advisory-locked
/// sync transaction, only when the durable `track_history` key is on, so history
/// commits atomically with the sync. `stored` is the pre-sync `path -> file_hash`
/// projection (used to classify each staged concept as added vs updated),
/// `staged` are the concepts (re-)written this sync, and `removed_ids` are the
/// concept ids whose files disappeared.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding sync
/// transaction so history never commits partially.
pub fn project(
    bundle_id: i64,
    stored: &BTreeMap<String, String>,
    staged: &[StagedConcept],
    removed_ids: &[String],
) -> Result<(), CatalogError> {
    // Nothing changed this sync (for example a no-op refresh): record nothing,
    // and skip even reading the clock.
    if staged.is_empty() && removed_ids.is_empty() {
        return Ok(());
    }
    // One instant stamps the whole sync so its intervals stay contiguous.
    let now = capture_sync_instant()?;
    record_staged_versions(bundle_id, now, stored, staged)?;
    record_removed_tombstones(bundle_id, now, removed_ids)?;
    Ok(())
}

/// Prune closed history versions older than the retention window.
///
/// A no-op when `retention_days <= 0` (keep indefinitely). Otherwise deletes only
/// rows whose `valid_to` is non-NULL and predates `now() - retention_days`; the
/// single current open version of each concept (`valid_to IS NULL`) is never
/// pruned, so present-time point-in-time queries always resolve. Scoped to
/// `bundle_id` (the bundle just synced), so each prune is bounded.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding sync
/// transaction.
pub fn prune(bundle_id: i64, retention_days: i32) -> Result<(), CatalogError> {
    if retention_days <= 0 {
        return Ok(());
    }
    Spi::run_with_args(
        "DELETE FROM pgokf.concept_history
         WHERE bundle_id = $1
           AND valid_to IS NOT NULL
           AND valid_to < pg_catalog.now() - pg_catalog.make_interval(days => $2)",
        &[bundle_id.into(), retention_days.into()],
    )
    .map_err(|error| spi_error("failed to prune concept-history versions", &error))
}

/// One `pgokf.concept_history` row projected onto the `concept_version` shape.
struct ConceptVersion {
    version: i64,
    valid_from: TimestampWithTimeZone,
    valid_to: Option<TimestampWithTimeZone>,
    change_kind: String,
    concept_type: Option<String>,
    title: Option<String>,
    description: Option<String>,
    file_hash: Option<String>,
}

/// Column projection shared by both readers, in the attribute order of
/// [`CONCEPT_VERSION_TYPE`] and of [`read_version`].
const CONCEPT_VERSION_COLUMNS: &str =
    "version, valid_from, valid_to, change_kind, type, title, description, file_hash";

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {CONCEPT_VERSION_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Read one history row projected via [`CONCEPT_VERSION_COLUMNS`].
fn read_version(row: &pgrx::spi::SpiHeapTupleData<'_>) -> Result<ConceptVersion, CatalogError> {
    let reader = RowReader::new(
        row,
        "failed to read concept_version column",
        "concept_version",
    );
    Ok(ConceptVersion {
        version: reader.required(1, "version")?,
        valid_from: reader.required::<TimestampWithTimeZone>(2, "valid_from")?,
        valid_to: reader.optional::<TimestampWithTimeZone>(3)?,
        change_kind: reader.required(4, "change_kind")?,
        concept_type: reader.optional(5)?,
        title: reader.optional(6)?,
        description: reader.optional(7)?,
        file_hash: reader.optional(8)?,
    })
}

/// Pack a [`ConceptVersion`] into a `pgokf.concept_version` heap tuple.
fn version_tuple(
    entry: ConceptVersion,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type(CONCEPT_VERSION_TYPE).map_err(composite_error)?;
    tuple
        .set_by_name("version", entry.version)
        .map_err(composite_error)?;
    tuple
        .set_by_name("valid_from", entry.valid_from)
        .map_err(composite_error)?;
    tuple
        .set_by_name("valid_to", entry.valid_to)
        .map_err(composite_error)?;
    tuple
        .set_by_name("change_kind", entry.change_kind)
        .map_err(composite_error)?;
    tuple
        .set_by_name("type", entry.concept_type)
        .map_err(composite_error)?;
    tuple
        .set_by_name("title", entry.title)
        .map_err(composite_error)?;
    tuple
        .set_by_name("description", entry.description)
        .map_err(composite_error)?;
    tuple
        .set_by_name("file_hash", entry.file_hash)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// Validate `max_rows` and map it to the SQL `LIMIT` argument.
///
/// A negative bound is a caller error (SQLSTATE `22023`); `0` is accepted and
/// returns no rows.
fn validate_max_rows(max_rows: i32) -> Result<i64, CatalogError> {
    if max_rows < 0 {
        return Err(CatalogError::invalid_parameter(
            format!("max_rows must be greater than or equal to 0, got {max_rows}"),
            Path::new(""),
        ));
    }
    Ok(i64::from(max_rows))
}

/// Read one concept's version timeline, newest first.
fn concept_history_impl(
    bundle_id: i64,
    concept_id: &str,
    max_rows: i32,
) -> Result<Vec<ConceptVersion>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let limit = validate_max_rows(max_rows)?;
    // Invoker rights: the caller's own pgokf.tenant row-level security applies to
    // pgokf.concept_history, so a scoped session sees only its own history.
    let query = format!(
        "SELECT {CONCEPT_VERSION_COLUMNS}
         FROM pgokf.concept_history
         WHERE bundle_id = $1 AND concept_id = $2
         ORDER BY version DESC
         LIMIT $3"
    );
    Spi::connect(|client| {
        let table = client
            .select(
                query.as_str(),
                None,
                &[bundle_id.into(), concept_id.into(), limit.into()],
            )
            .map_err(|error| spi_error("failed to read concept history", &error))?;
        let mut versions = Vec::with_capacity(table.len());
        for row in table {
            versions.push(read_version(&row)?);
        }
        Ok(versions)
    })
}

/// Read the single concept version valid at `as_of`.
fn concept_as_of_impl(
    bundle_id: i64,
    concept_id: &str,
    as_of: TimestampWithTimeZone,
) -> Result<Vec<ConceptVersion>, CatalogError> {
    // The half-open interval [valid_from, valid_to) that covers `as_of`. A
    // zero-width removal tombstone (valid_from = valid_to) is never covered
    // (as_of < valid_to is false at the boundary), so an as-of at or after a
    // removal returns zero rows. Invoker rights: the caller's tenant RLS applies.
    const QUERY: &str = "
        SELECT version, valid_from, valid_to, change_kind, type, title, description, file_hash
        FROM pgokf.concept_history
        WHERE bundle_id = $1
          AND concept_id = $2
          AND valid_from <= $3
          AND (valid_to IS NULL OR $3 < valid_to)
        ORDER BY version DESC
        LIMIT 1";
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    Spi::connect(|client| {
        let table = client
            .select(
                QUERY,
                None,
                &[bundle_id.into(), concept_id.into(), as_of.into()],
            )
            .map_err(|error| spi_error("failed to read concept as-of version", &error))?;
        let mut versions = Vec::with_capacity(table.len());
        for row in table {
            versions.push(read_version(&row)?);
        }
        Ok(versions)
    })
}

/// SQL-facing history query entry points, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::datum::TimestampWithTimeZone;
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{concept_as_of_impl, concept_history_impl, version_tuple};

    extension_sql!(
        r"
CREATE TYPE pgokf.concept_version AS (
    version     bigint,
    valid_from  timestamptz,
    valid_to    timestamptz,
    change_kind text,
    type        text,
    title       text,
    description text,
    file_hash   text
);

COMMENT ON TYPE pgokf.concept_version IS
    'One version of a concept from pgokf.concept_history / pgokf.concept_as_of: the per-concept version number, its validity interval [valid_from, valid_to) (valid_to NULL = current), what produced it (change_kind), and a snapshot of the concept core (type, title, description, file_hash) at that version (NULL for a removal tombstone).';
",
        name = "concept_version_type",
        requires = ["concept_history_table"]
    );

    /// Return one concept's version timeline, newest first.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Returns the
    /// recorded `pgokf.concept_version` rows for `(bundle_id, concept_id)`
    /// ordered by descending version, bounded by `max_rows` (must be `>= 0`;
    /// SQLSTATE `22023` otherwise). Empty when history was never recorded for the
    /// concept (for example the bundle was synced with `track_history` off).
    #[pg_extern(stable, parallel_safe, requires = ["concept_version_type"])]
    fn concept_history(
        bundle_id: i64,
        concept_id: &str,
        max_rows: default!(i32, 100),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_version")> {
        let versions = concept_history_impl(bundle_id, concept_id, max_rows)
            .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = versions
            .into_iter()
            .map(|entry| version_tuple(entry).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    /// Return the single concept version that was valid at `as_of`.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Returns the one
    /// `pgokf.concept_version` whose validity interval `[valid_from, valid_to)`
    /// covers `as_of` (`valid_from <= as_of AND (valid_to IS NULL OR as_of <
    /// valid_to)`), or zero rows when the concept did not exist - or had been
    /// removed - at that instant. The point-in-time "what did this concept say
    /// then?" answer.
    #[pg_extern(stable, parallel_safe, requires = ["concept_version_type"])]
    fn concept_as_of(
        bundle_id: i64,
        concept_id: &str,
        as_of: TimestampWithTimeZone,
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_version")> {
        let versions =
            concept_as_of_impl(bundle_id, concept_id, as_of).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = versions
            .into_iter()
            .map(|entry| version_tuple(entry).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.concept_history(bigint, text, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.concept_as_of(bigint, text, timestamptz) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_history(bigint, text, integer) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.concept_as_of(bigint, text, timestamptz) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_history(bigint, text, integer) IS
    'List one concept''s recorded version timeline as pgokf.concept_version, newest version first, bounded by max_rows. Reader-level, STABLE, invoker rights (the caller''s tenant row-level security applies over pgokf.concept_history). Empty when track_history was off for the bundle''s syncs. Raises 22023 when max_rows < 0.';
COMMENT ON FUNCTION pgokf.concept_as_of(bigint, text, timestamptz) IS
    'Return the single concept version valid at as_of (valid_from <= as_of AND (valid_to IS NULL OR as_of < valid_to)) as pgokf.concept_version, or zero rows if the concept did not exist or had been removed at that instant. The point-in-time answer. Reader-level, STABLE, invoker rights (the caller''s tenant row-level security applies).';
",
        name = "concept_history_function_hardening",
        requires = [concept_history, concept_as_of]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_max_rows_accepts_zero_and_positive() {
        // Arrange / Act / Assert
        assert_eq!(validate_max_rows(0).expect("zero is valid"), 0);
        assert_eq!(validate_max_rows(250).expect("positive is valid"), 250);
    }

    #[test]
    fn validate_max_rows_rejects_negative() {
        // Arrange / Act
        let error = validate_max_rows(-1).expect_err("negative max_rows must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }
}
