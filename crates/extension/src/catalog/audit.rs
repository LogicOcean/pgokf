//! Sync/audit log: `pgokf_private.sync_log` and `pgokf.list_sync_log`.
//!
//! # What this records
//!
//! Every successful catalog-mutating operation appends exactly one row to the
//! administrator-only `pgokf_private.sync_log`:
//!
//! - `register` / `refresh` / `content` rows are written at the successful tail
//!   of [`crate::catalog::sync::run_bundle_sync`], carrying the per-bucket
//!   change counts (`added`/`updated`/`removed`/`unchanged`/`total`) and the
//!   aggregate `sync_hash` of the synced snapshot;
//! - one `unregister` row is written by
//!   [`crate::catalog::admin`] when a bundle is removed, capturing the bundle's
//!   path (the counts and hash are `NULL` — an unregister has no diff).
//!
//! # v1 transactional semantics
//!
//! The audit row is inserted inside the very transaction that performs the
//! operation, under the bundle advisory lock. It therefore commits atomically
//! with the operation: **a logged row always means the operation committed**,
//! and a sync that fails and rolls back leaves no row behind. There is
//! deliberately no autonomous-transaction "attempted but failed" logging in
//! this version — the log is a durable record of what happened, not of what was
//! tried.
//!
//! # Retention
//!
//! After the row is appended, history older than the durable
//! `sync_log_retention_days` policy is pruned in the same transaction (see
//! [`record`]). A retention of `0` (or simply no rows older than the window)
//! keeps history indefinitely. This is the mechanism that activates the
//! `sync_log_retention_days` configuration key.
//!
//! # Reader surface
//!
//! [`list_sync_log`](pgokf::list_sync_log) is the reader-level projection over
//! the log. Because the table lives in the administrator-only `pgokf_private`
//! schema, the function is `SECURITY DEFINER` (with a pinned `search_path`) and
//! its `EXECUTE` is granted to `pgokf_reader`, so operators can audit sync
//! activity without direct access to the private schema.

use std::path::Path;

use okf_sync::SyncReport;
use pgrx::datum::TimestampWithTimeZone;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::batch::BATCH_SIZE;
use crate::catalog::spi_read::RowReader;
use crate::catalog::types::count_to_i32;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the sync-log-entry composite type.
const SYNC_LOG_ENTRY_TYPE: &str = "pgokf.sync_log_entry";

/// Column projection shared by every `sync_log_entry` read, in the attribute
/// order of [`SYNC_LOG_ENTRY_TYPE`] and of [`read_entry`].
const SYNC_LOG_ENTRY_COLUMNS: &str =
    "id, bundle_id, bundle_path, op, actor, synced_at, added, updated, removed, unchanged, total";

extension_sql!(
    r"
CREATE TABLE pgokf_private.sync_log (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    bundle_id   bigint,
    bundle_path text,
    op          text NOT NULL,
    actor       text NOT NULL DEFAULT session_user,
    synced_at   timestamptz NOT NULL DEFAULT now(),
    added       integer,
    updated     integer,
    removed     integer,
    unchanged   integer,
    total       integer,
    sync_hash   text,
    tenant_id   text NOT NULL DEFAULT 'default',
    CONSTRAINT sync_log_op_chk CHECK (op IN ('register', 'refresh', 'content', 'unregister'))
);

CREATE INDEX sync_log_bundle_id_idx ON pgokf_private.sync_log (bundle_id);
CREATE INDEX sync_log_synced_at_idx ON pgokf_private.sync_log (synced_at);

REVOKE ALL ON pgokf_private.sync_log FROM PUBLIC;

COMMENT ON TABLE pgokf_private.sync_log IS
    'Append-only audit trail of catalog-mutating operations: one row per successful register/refresh/content sync or bundle unregister, written inside the operation''s own transaction under the bundle advisory lock (so a logged row always means the operation committed). History is pruned to the sync_log_retention_days policy after each append. Administrator-only; read through the reader-granted pgokf.list_sync_log function.';
COMMENT ON COLUMN pgokf_private.sync_log.id IS
    'Surrogate primary key (GENERATED ALWAYS AS IDENTITY); monotonic append order of the audit trail.';
COMMENT ON COLUMN pgokf_private.sync_log.bundle_id IS
    'Identity of the affected bundle. Retained for unregister rows even though the pgokf.bundles row is gone, so there is intentionally no foreign key.';
COMMENT ON COLUMN pgokf_private.sync_log.tenant_id IS
    'Multi-tenant owner of the operation, stamped from pgokf.tenant (effective_tenant(); ''default'' when unset). The table stays administrator-only (no row-level security); the reader-facing pgokf.list_sync_log applies the same opt-in tenant filter so a tenant session sees only its own audit rows.';
COMMENT ON COLUMN pgokf_private.sync_log.bundle_path IS
    'Canonical path (filesystem root or the content:<name> synthetic key) of the affected bundle, captured at operation time.';
COMMENT ON COLUMN pgokf_private.sync_log.op IS
    'The operation: register / refresh / content (register_bundle_content) / unregister.';
COMMENT ON COLUMN pgokf_private.sync_log.actor IS
    'The session_user that performed the operation, captured by column default.';
COMMENT ON COLUMN pgokf_private.sync_log.synced_at IS
    'When the operation committed (transaction now()); the column pruning compares against sync_log_retention_days.';
COMMENT ON COLUMN pgokf_private.sync_log.added IS
    'Count of concepts added by the sync; NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.updated IS
    'Count of concepts updated by the sync; NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.removed IS
    'Count of concepts removed by the sync; NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.unchanged IS
    'Count of concepts left unchanged by the sync; NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.total IS
    'Total files considered by the sync (added + updated + removed + unchanged); NULL for an unregister row.';
COMMENT ON COLUMN pgokf_private.sync_log.sync_hash IS
    'Aggregate BLAKE3 digest of the synced snapshot (matches pgokf.bundles.sync_hash); NULL for an unregister row.';
",
    name = "sync_log_table",
    requires = ["catalog_tables", "config_table"]
);

extension_sql!(
    r"
CREATE TABLE pgokf_private.sync_log_change (
    sync_id     bigint NOT NULL REFERENCES pgokf_private.sync_log (id) ON DELETE CASCADE,
    tenant_id   text NOT NULL DEFAULT 'default',
    bundle_id   bigint,
    concept_id  text,
    change_kind text CHECK (change_kind IN ('added', 'updated', 'removed'))
);

CREATE INDEX sync_log_change_sync_id_idx ON pgokf_private.sync_log_change (sync_id);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER sync path bypasses
-- it to record a single-tenant bundle's change manifest, and the reader-facing
-- pgokf.list_sync_changes (also SECURITY DEFINER over this administrator-only
-- table) applies the same opt-in tenant filter explicitly.
ALTER TABLE pgokf_private.sync_log_change ENABLE ROW LEVEL SECURITY;
CREATE POLICY sync_log_change_tenant_isolation ON pgokf_private.sync_log_change
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

REVOKE ALL ON pgokf_private.sync_log_change FROM PUBLIC;

COMMENT ON TABLE pgokf_private.sync_log_change IS
    'Per-concept change manifest for one sync: the concrete concepts a register/refresh/content sync added, updated, or removed, hung off the parent pgokf_private.sync_log row (ON DELETE CASCADE, so retention pruning of the parent drops the manifest too). Administrator-only; read through the reader-granted pgokf.list_sync_changes function.';
COMMENT ON COLUMN pgokf_private.sync_log_change.sync_id IS
    'Parent pgokf_private.sync_log.id this change belongs to; ON DELETE CASCADE ties the manifest to the audit row''s lifetime and retention window.';
COMMENT ON COLUMN pgokf_private.sync_log_change.tenant_id IS
    'Multi-tenant owner of the change, stamped from the parent bundle''s tenant_id; the row-level-security policy and the reader function apply the same opt-in pgokf.tenant filter.';
COMMENT ON COLUMN pgokf_private.sync_log_change.bundle_id IS
    'Identity of the bundle whose sync produced this change. FK-free (like sync_log.bundle_id) so the manifest survives the bundle''s later deletion.';
COMMENT ON COLUMN pgokf_private.sync_log_change.concept_id IS
    'The affected concept''s path-derived OKF id.';
COMMENT ON COLUMN pgokf_private.sync_log_change.change_kind IS
    'What happened to the concept in this sync: added, updated, or removed.';
",
    name = "sync_log_change_table",
    requires = ["sync_log_table"]
);

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Append one audit row, prune history older than the retention window, and
/// return the new row's identity.
///
/// The row is inserted with the caller-supplied identity, operation, per-bucket
/// counts (`None` for an unregister), and snapshot hash; `actor` and
/// `synced_at` take their column defaults (`session_user` and `now()`), so an
/// audit row is always attributed to the invoking session at commit time. When
/// `retention_days > 0`, rows whose `synced_at` predates `now() - retention_days`
/// are deleted in the same transaction; `0` keeps history indefinitely. The
/// returned `sync_log.id` is the parent key the per-concept change manifest
/// ([`record_changes`]) hangs its child rows off of.
///
/// Runs on the `SECURITY DEFINER` sync/admin path, which holds privileges on
/// the administrator-only table; parameterized SPI only.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding
/// transaction so the audit row commits atomically with the operation.
pub(crate) fn record(
    bundle_id: i64,
    bundle_path: &str,
    op: &str,
    report: Option<&SyncReport>,
    sync_hash: Option<&str>,
    retention_days: i32,
) -> Result<i64, CatalogError> {
    let (added, updated, removed, unchanged, total) = match report {
        Some(report) => (
            Some(count_to_i32(report.added)),
            Some(count_to_i32(report.updated)),
            Some(count_to_i32(report.removed)),
            Some(count_to_i32(report.unchanged)),
            Some(count_to_i32(report.total())),
        ),
        None => (None, None, None, None, None),
    };

    let sync_id = Spi::get_one_with_args::<i64>(
        "INSERT INTO pgokf_private.sync_log
             (bundle_id, tenant_id, bundle_path, op,
              added, updated, removed, unchanged, total, sync_hash)
         VALUES ($1, pgokf_private.effective_tenant(), $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id",
        &[
            bundle_id.into(),
            bundle_path.into(),
            op.into(),
            added.into(),
            updated.into(),
            removed.into(),
            unchanged.into(),
            total.into(),
            sync_hash.into(),
        ],
    )
    .map_err(|error| spi_error("failed to append sync-log row", &error))?
    .ok_or_else(|| CatalogError::internal("sync-log insert returned no id", Path::new("")))?;

    if retention_days > 0 {
        // Pruning the parent sync_log cascades to sync_log_change via the child
        // table's ON DELETE CASCADE, so the change manifest honors the same
        // retention window without a second delete.
        Spi::run_with_args(
            "DELETE FROM pgokf_private.sync_log
             WHERE synced_at < pg_catalog.now() - pg_catalog.make_interval(days => $1)",
            &[retention_days.into()],
        )
        .map_err(|error| spi_error("failed to prune sync-log history", &error))?;
    }
    Ok(sync_id)
}

/// Append the per-concept change manifest for one committed sync.
///
/// `concept_ids` and `change_kinds` are parallel slices (each `change_kind` is
/// one of `added` / `updated` / `removed`, matching the
/// `pgokf_private.sync_log_change` CHECK constraint) describing exactly which
/// concepts the sync added, updated, or removed. Every child row is stamped with
/// the parent `sync_id` and the bundle's own `tenant_id`, and is written in
/// bounded [`BATCH_SIZE`] chunks so a very large diff never builds one unbounded
/// statement. An empty manifest writes nothing.
///
/// Runs inside the same `SECURITY DEFINER` sync transaction as [`record`], so
/// the manifest commits atomically with the operation and cascades away when the
/// parent `sync_log` row is pruned.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding sync
/// transaction.
pub(crate) fn record_changes(
    sync_id: i64,
    bundle_id: i64,
    concept_ids: &[String],
    change_kinds: &[String],
) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf_private.sync_log_change
            (sync_id, tenant_id, bundle_id, concept_id, change_kind)
        SELECT $1,
               (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $2),
               $2, d.concept_id, d.change_kind
        FROM unnest($3::text[], $4::text[]) AS d(concept_id, change_kind)";

    debug_assert_eq!(concept_ids.len(), change_kinds.len());
    if concept_ids.is_empty() {
        return Ok(());
    }

    let total = concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            INSERT,
            &[
                sync_id.into(),
                bundle_id.into(),
                concept_ids[start..end].to_vec().into(),
                change_kinds[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to append sync-log change rows", &error))?;
    }
    Ok(())
}

/// One `pgokf_private.sync_log` row projected onto the `sync_log_entry` shape.
struct SyncLogEntry {
    id: i64,
    bundle_id: Option<i64>,
    bundle_path: Option<String>,
    op: String,
    actor: String,
    synced_at: TimestampWithTimeZone,
    added: Option<i32>,
    updated: Option<i32>,
    removed: Option<i32>,
    unchanged: Option<i32>,
    total: Option<i32>,
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {SYNC_LOG_ENTRY_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Read one log row projected via [`SYNC_LOG_ENTRY_COLUMNS`].
fn read_entry(row: &pgrx::spi::SpiHeapTupleData<'_>) -> Result<SyncLogEntry, CatalogError> {
    let reader = RowReader::new(
        row,
        "failed to read sync_log_entry column",
        "sync_log_entry",
    );
    Ok(SyncLogEntry {
        id: reader.required(1, "id")?,
        bundle_id: reader.optional(2)?,
        bundle_path: reader.optional(3)?,
        op: reader.required(4, "op")?,
        actor: reader.required(5, "actor")?,
        synced_at: reader.required::<TimestampWithTimeZone>(6, "synced_at")?,
        added: reader.optional(7)?,
        updated: reader.optional(8)?,
        removed: reader.optional(9)?,
        unchanged: reader.optional(10)?,
        total: reader.optional(11)?,
    })
}

/// Pack a [`SyncLogEntry`] into a `pgokf.sync_log_entry` heap tuple.
fn entry_tuple(entry: SyncLogEntry) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type(SYNC_LOG_ENTRY_TYPE).map_err(composite_error)?;
    tuple.set_by_name("id", entry.id).map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", entry.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_path", entry.bundle_path)
        .map_err(composite_error)?;
    tuple.set_by_name("op", entry.op).map_err(composite_error)?;
    tuple
        .set_by_name("actor", entry.actor)
        .map_err(composite_error)?;
    tuple
        .set_by_name("synced_at", entry.synced_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("added", entry.added)
        .map_err(composite_error)?;
    tuple
        .set_by_name("updated", entry.updated)
        .map_err(composite_error)?;
    tuple
        .set_by_name("removed", entry.removed)
        .map_err(composite_error)?;
    tuple
        .set_by_name("unchanged", entry.unchanged)
        .map_err(composite_error)?;
    tuple
        .set_by_name("total", entry.total)
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

/// Read the most recent log rows, optionally scoped to one bundle.
fn list_sync_log_impl(
    bundle_id: Option<i64>,
    max_rows: i32,
) -> Result<Vec<SyncLogEntry>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let limit = validate_max_rows(max_rows)?;
    // list_sync_log is SECURITY DEFINER and so bypasses row-level security; it
    // therefore applies the same opt-in tenant filter explicitly, so a session
    // that set pgokf.tenant sees only its own audit rows while an unset session
    // sees every row (backward compatible).
    let query = format!(
        "SELECT {SYNC_LOG_ENTRY_COLUMNS}
         FROM pgokf_private.sync_log
         WHERE ($1::bigint IS NULL OR bundle_id = $1)
           AND (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = ''
             OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
         ORDER BY synced_at DESC, id DESC
         LIMIT $2"
    );
    Spi::connect(|client| {
        let table = client
            .select(query.as_str(), None, &[bundle_id.into(), limit.into()])
            .map_err(|error| spi_error("failed to read sync log", &error))?;
        let mut entries = Vec::with_capacity(table.len());
        for row in table {
            entries.push(read_entry(&row)?);
        }
        Ok(entries)
    })
}

/// Qualified SQL name of the sync-change composite type.
const SYNC_CHANGE_TYPE: &str = "pgokf.sync_change";

/// One `pgokf_private.sync_log_change` row projected onto the `sync_change`
/// shape.
struct SyncChangeEntry {
    sync_id: i64,
    bundle_id: Option<i64>,
    concept_id: Option<String>,
    change_kind: Option<String>,
}

fn change_composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {SYNC_CHANGE_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Pack a [`SyncChangeEntry`] into a `pgokf.sync_change` heap tuple.
fn change_entry_tuple(
    entry: SyncChangeEntry,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type(SYNC_CHANGE_TYPE).map_err(change_composite_error)?;
    tuple
        .set_by_name("sync_id", entry.sync_id)
        .map_err(change_composite_error)?;
    tuple
        .set_by_name("bundle_id", entry.bundle_id)
        .map_err(change_composite_error)?;
    tuple
        .set_by_name("concept_id", entry.concept_id)
        .map_err(change_composite_error)?;
    tuple
        .set_by_name("change_kind", entry.change_kind)
        .map_err(change_composite_error)?;
    Ok(tuple)
}

/// Read the per-concept change manifest of one sync, tenant-scoped.
fn list_sync_changes_impl(
    sync_id: i64,
    max_rows: i32,
) -> Result<Vec<SyncChangeEntry>, CatalogError> {
    // list_sync_changes is SECURITY DEFINER and so bypasses row-level security;
    // it therefore applies the same opt-in tenant filter explicitly, exactly as
    // list_sync_log does — a session that set pgokf.tenant sees only its own
    // change rows while an unset session sees every row (backward compatible).
    // Ordered by change_kind then concept_id for a stable listing.
    const QUERY: &str = "
        SELECT sync_id, bundle_id, concept_id, change_kind
        FROM pgokf_private.sync_log_change
        WHERE sync_id = $1
          AND (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
            OR pg_catalog.current_setting('pgokf.tenant', true) = ''
            OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
        ORDER BY change_kind, concept_id
        LIMIT $2";

    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let limit = validate_max_rows(max_rows)?;
    Spi::connect(|client| {
        let table = client
            .select(QUERY, None, &[sync_id.into(), limit.into()])
            .map_err(|error| spi_error("failed to read sync change manifest", &error))?;
        let mut entries = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read sync_change column", "sync_change");
            entries.push(SyncChangeEntry {
                sync_id: reader.required(1, "sync_id")?,
                bundle_id: reader.optional(2)?,
                concept_id: reader.optional(3)?,
                change_kind: reader.optional(4)?,
            });
        }
        Ok(entries)
    })
}

/// SQL-facing sync-log projection, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{change_entry_tuple, entry_tuple, list_sync_changes_impl, list_sync_log_impl};

    extension_sql!(
        r"
CREATE TYPE pgokf.sync_log_entry AS (
    id          bigint,
    bundle_id   bigint,
    bundle_path text,
    op          text,
    actor       text,
    synced_at   timestamptz,
    added       integer,
    updated     integer,
    removed     integer,
    unchanged   integer,
    total       integer
);

COMMENT ON TYPE pgokf.sync_log_entry IS
    'One audit-trail entry from pgokf.list_sync_log: the operation (register/refresh/content/unregister), who ran it, when it committed, and its per-bucket change counts (NULL for an unregister).';
",
        name = "sync_log_entry_type",
        requires = ["catalog_tables"]
    );

    /// List recent catalog sync/audit-log entries, most recent first.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Pass
    /// `bundle_id` to scope the listing to one bundle, or leave it `NULL` for
    /// every bundle. `max_rows` bounds the number of rows returned (must be
    /// `>= 0`; SQLSTATE `22023` otherwise).
    #[pg_extern(requires = ["sync_log_entry_type", "sync_log_table"])]
    fn list_sync_log(
        bundle_id: default!(Option<i64>, "NULL"),
        max_rows: default!(i32, 100),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.sync_log_entry")> {
        let entries = list_sync_log_impl(bundle_id, max_rows).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = entries
            .into_iter()
            .map(|entry| entry_tuple(entry).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.list_sync_log(bigint, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.list_sync_log(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_sync_log(bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.list_sync_log(bigint, integer) IS
    'List recent pgokf_private.sync_log audit entries as pgokf.sync_log_entry, newest first, optionally scoped to one bundle and bounded by max_rows. Reader-level (SECURITY DEFINER over the admin-only log table); raises 22023 when max_rows < 0.';
",
        name = "sync_log_function_hardening",
        requires = [list_sync_log]
    );

    extension_sql!(
        r"
CREATE TYPE pgokf.sync_change AS (
    sync_id     bigint,
    bundle_id   bigint,
    concept_id  text,
    change_kind text
);

COMMENT ON TYPE pgokf.sync_change IS
    'One entry of a sync''s per-concept change manifest from pgokf.list_sync_changes: the parent sync id, the affected bundle and concept, and what happened to it (added/updated/removed).';
",
        name = "sync_change_type",
        requires = ["sync_log_change_table"]
    );

    /// List the per-concept change manifest of one sync, ordered stably.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). `sync_id` is a
    /// `pgokf_private.sync_log.id` (as returned in `pgokf.list_sync_log`);
    /// `max_rows` bounds the rows returned (must be `>= 0`; SQLSTATE `22023`
    /// otherwise). Returns the concepts the sync added, updated, or removed.
    #[pg_extern(requires = ["sync_change_type", "sync_log_change_table"])]
    fn list_sync_changes(
        sync_id: i64,
        max_rows: default!(i32, 1000),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.sync_change")> {
        let entries =
            list_sync_changes_impl(sync_id, max_rows).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = entries
            .into_iter()
            .map(|entry| change_entry_tuple(entry).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.list_sync_changes(bigint, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.list_sync_changes(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_sync_changes(bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.list_sync_changes(bigint, integer) IS
    'List the per-concept change manifest of one sync (pgokf_private.sync_log.id) as pgokf.sync_change: the concepts it added, updated, or removed, ordered by change_kind then concept_id and bounded by max_rows. Reader-level (SECURITY DEFINER over the admin-only manifest table, tenant-scoped); raises 22023 when max_rows < 0.';
",
        name = "sync_change_function_hardening",
        requires = [list_sync_changes]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_max_rows_accepts_zero_and_positive() {
        // Arrange / Act / Assert
        assert_eq!(validate_max_rows(0).expect("zero is valid"), 0);
        assert_eq!(validate_max_rows(100).expect("positive is valid"), 100);
    }

    #[test]
    fn validate_max_rows_rejects_negative() {
        // Arrange / Act
        let error = validate_max_rows(-1).expect_err("negative max_rows must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }
}
