// SPDX-License-Identifier: AGPL-3.0-only
//! Exfiltration / access audit: `pgokf_private.access_log` and
//! `pgokf.list_access_log`.
//!
//! # What this records
//!
//! The catalog's *exfiltration surface* - the operations that move concept
//! content out of the database - each append exactly one row to the
//! administrator-only `pgokf_private.access_log`:
//!
//! - [`crate::catalog::export::export_parquet`] (`export_parquet`) - the
//!   Parquet snapshot of a bundle's catalog tables written to a server
//!   directory;
//! - [`crate::catalog::source::export_sources`] (`export_sources`) - the
//!   verbatim source files of a bundle reconstructed on disk;
//! - [`crate::catalog::source::get_concept_source`] (`get_concept_source`) -
//!   the single-concept verbatim source-byte read delivered to the client.
//!
//! Each row is stamped with `pgokf_private.effective_tenant()` so a tenant's
//! audit is isolated, and with the `session_user` and `now()` column defaults so
//! the actor and time are always attributed to the invoking session at commit.
//!
//! # Retention
//!
//! The access log shares the durable `sync_log_retention_days` policy (keeping
//! the operator surface to one retention knob): after each append, [`record`]
//! prunes rows older than the window in the same transaction, exactly as the
//! sync-log audit does. A retention of `0` (or no older rows) keeps history
//! indefinitely.
//!
//! # Security model
//!
//! Because the append target lives in the administrator-only `pgokf_private`
//! schema, [`record`] must run under owner rights - every caller is therefore a
//! `SECURITY DEFINER` function (the two exports already were; `get_concept_source`
//! is `SECURITY DEFINER` for exactly this reason, applying the tenant predicate
//! explicitly). The reader surface, [`list_access_log`](pgokf::list_access_log),
//! is deliberately **admin-tier** (an exfiltration audit is sensitive): it is
//! `SECURITY DEFINER`, granted only to `pgokf_admin`, and tenant-scoped.

use std::path::Path;

use pgrx::datum::TimestampWithTimeZone;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::config;
use crate::catalog::spi_read::RowReader;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the access-log-entry composite type.
const ACCESS_LOG_ENTRY_TYPE: &str = "pgokf.access_log_entry";

extension_sql!(
    r"
CREATE TABLE pgokf_private.access_log (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id  text NOT NULL DEFAULT 'default',
    actor      text NOT NULL DEFAULT session_user,
    at         timestamptz NOT NULL DEFAULT now(),
    op         text CHECK (op IN ('export_parquet', 'export_sources', 'get_concept_source')),
    bundle_id  bigint,
    concept_id text,
    detail     text
);

CREATE INDEX access_log_at_idx ON pgokf_private.access_log (at);
CREATE INDEX access_log_bundle_id_idx ON pgokf_private.access_log (bundle_id);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER exfiltration paths
-- bypass it to append a row, and the admin-only pgokf.list_access_log (also
-- SECURITY DEFINER over this administrator-only table) applies the same opt-in
-- tenant filter explicitly.
ALTER TABLE pgokf_private.access_log ENABLE ROW LEVEL SECURITY;
CREATE POLICY access_log_tenant_isolation ON pgokf_private.access_log
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

REVOKE ALL ON pgokf_private.access_log FROM PUBLIC;

COMMENT ON TABLE pgokf_private.access_log IS
    'Append-only exfiltration/access audit: one row per content-exporting operation (export_parquet, export_sources, get_concept_source) - who read or exported what, and when. Written inside the operation''s own transaction under owner rights, then pruned to the sync_log_retention_days policy. Administrator-only; read through the admin-granted pgokf.list_access_log function.';
COMMENT ON COLUMN pgokf_private.access_log.id IS
    'Surrogate primary key (GENERATED ALWAYS AS IDENTITY); monotonic append order of the access trail.';
COMMENT ON COLUMN pgokf_private.access_log.tenant_id IS
    'Multi-tenant owner of the access, stamped from pgokf.tenant (effective_tenant(); ''default'' when unset). The row-level-security policy and the reader function apply the same opt-in tenant filter so a tenant session sees only its own access rows.';
COMMENT ON COLUMN pgokf_private.access_log.actor IS
    'The session_user that performed the operation, captured by column default.';
COMMENT ON COLUMN pgokf_private.access_log.at IS
    'When the operation committed (transaction now()); the pruning compares against sync_log_retention_days.';
COMMENT ON COLUMN pgokf_private.access_log.op IS
    'The exfiltration operation: export_parquet / export_sources / get_concept_source.';
COMMENT ON COLUMN pgokf_private.access_log.bundle_id IS
    'Identity of the bundle whose content was read or exported. FK-free so the row survives the bundle''s later deletion.';
COMMENT ON COLUMN pgokf_private.access_log.concept_id IS
    'The specific concept read, for get_concept_source; NULL for the whole-bundle exports.';
COMMENT ON COLUMN pgokf_private.access_log.detail IS
    'Optional free-text context (for the exports, the resolved destination directory).';
",
    name = "access_log_table",
    requires = ["catalog_tables", "config_table"]
);

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {ACCESS_LOG_ENTRY_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Append one access-audit row for an exfiltration operation and prune history
/// older than the retention window.
///
/// `op` is one of the three audited operations; `concept_id` is the specific
/// concept for a single-concept read (`None` for the whole-bundle exports); and
/// `detail` is optional free-text context. The row is stamped with the session's
/// effective tenant and the `session_user` / `now()` defaults, then rows older
/// than `sync_log_retention_days` are pruned in the same transaction.
///
/// Must run under owner rights (every caller is a `SECURITY DEFINER` function),
/// since the append target is the administrator-only `pgokf_private.access_log`.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding
/// transaction so the audit row commits atomically with the operation.
pub(crate) fn record(
    op: &str,
    bundle_id: i64,
    concept_id: Option<&str>,
    detail: Option<&str>,
) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "INSERT INTO pgokf_private.access_log (tenant_id, op, bundle_id, concept_id, detail)
         VALUES (pgokf_private.effective_tenant(), $1, $2, $3, $4)",
        &[
            op.into(),
            bundle_id.into(),
            concept_id.into(),
            detail.into(),
        ],
    )
    .map_err(|error| spi_error("failed to append access-log row", &error))?;

    let retention_days = config::sync_log_retention_days()?;
    if retention_days > 0 {
        Spi::run_with_args(
            "DELETE FROM pgokf_private.access_log
             WHERE at < pg_catalog.now() - pg_catalog.make_interval(days => $1)",
            &[retention_days.into()],
        )
        .map_err(|error| spi_error("failed to prune access-log history", &error))?;
    }
    Ok(())
}

/// One `pgokf_private.access_log` row projected onto the `access_log_entry`
/// shape.
struct AccessLogEntry {
    id: i64,
    actor: String,
    at: TimestampWithTimeZone,
    op: Option<String>,
    bundle_id: Option<i64>,
    concept_id: Option<String>,
    detail: Option<String>,
}

/// Pack an [`AccessLogEntry`] into a `pgokf.access_log_entry` heap tuple.
fn entry_tuple(
    entry: AccessLogEntry,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type(ACCESS_LOG_ENTRY_TYPE).map_err(composite_error)?;
    tuple.set_by_name("id", entry.id).map_err(composite_error)?;
    tuple
        .set_by_name("actor", entry.actor)
        .map_err(composite_error)?;
    tuple.set_by_name("at", entry.at).map_err(composite_error)?;
    tuple.set_by_name("op", entry.op).map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", entry.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("concept_id", entry.concept_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("detail", entry.detail)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// Validate `max_rows` and map it to the SQL `LIMIT` argument.
///
/// A negative bound is a caller error (SQLSTATE `22023`); `0` returns no rows.
fn validate_max_rows(max_rows: i32) -> Result<i64, CatalogError> {
    if max_rows < 0 {
        return Err(CatalogError::invalid_parameter(
            format!("max_rows must be greater than or equal to 0, got {max_rows}"),
            Path::new(""),
        ));
    }
    Ok(i64::from(max_rows))
}

/// Read the most recent access-log rows, optionally scoped to one bundle,
/// tenant-filtered.
fn list_access_log_impl(
    bundle_id: Option<i64>,
    max_rows: i32,
) -> Result<Vec<AccessLogEntry>, CatalogError> {
    // SECURITY DEFINER bypasses row-level security, so apply the same opt-in
    // tenant filter explicitly (a scoped session sees only its own access rows;
    // an unset session sees every row).
    const QUERY: &str = "
        SELECT id, actor, at, op, bundle_id, concept_id, detail
        FROM pgokf_private.access_log
        WHERE ($1::bigint IS NULL OR bundle_id = $1)
          AND (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
            OR pg_catalog.current_setting('pgokf.tenant', true) = ''
            OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
        ORDER BY at DESC, id DESC
        LIMIT $2";

    // Admin-tier: an exfiltration audit is sensitive.
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    let limit = validate_max_rows(max_rows)?;
    Spi::connect(|client| {
        let table = client
            .select(QUERY, None, &[bundle_id.into(), limit.into()])
            .map_err(|error| spi_error("failed to read access log", &error))?;
        let mut entries = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(
                &row,
                "failed to read access_log_entry column",
                "access_log_entry",
            );
            entries.push(AccessLogEntry {
                id: reader.required(1, "id")?,
                actor: reader.required(2, "actor")?,
                at: reader.required::<TimestampWithTimeZone>(3, "at")?,
                op: reader.optional(4)?,
                bundle_id: reader.optional(5)?,
                concept_id: reader.optional(6)?,
                detail: reader.optional(7)?,
            });
        }
        Ok(entries)
    })
}

/// SQL-facing access-log projection, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{entry_tuple, list_access_log_impl};

    extension_sql!(
        r"
CREATE TYPE pgokf.access_log_entry AS (
    id         bigint,
    actor      text,
    at         timestamptz,
    op         text,
    bundle_id  bigint,
    concept_id text,
    detail     text
);

COMMENT ON TYPE pgokf.access_log_entry IS
    'One exfiltration/access-audit entry from pgokf.list_access_log: the operation (export_parquet/export_sources/get_concept_source), who ran it, when, the affected bundle and concept, and optional detail.';
",
        name = "access_log_entry_type",
        requires = ["catalog_tables"]
    );

    /// List recent exfiltration/access-audit entries, most recent first.
    ///
    /// Requires membership in `pgokf_admin` (an exfiltration audit is
    /// sensitive). Pass `bundle_id` to scope the listing to one bundle, or leave
    /// it `NULL` for every bundle. `max_rows` bounds the rows returned (must be
    /// `>= 0`; SQLSTATE `22023` otherwise).
    #[pg_extern(requires = ["access_log_entry_type", "access_log_table"])]
    fn list_access_log(
        bundle_id: default!(Option<i64>, "NULL"),
        max_rows: default!(i32, 100),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.access_log_entry")> {
        let entries =
            list_access_log_impl(bundle_id, max_rows).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = entries
            .into_iter()
            .map(|entry| entry_tuple(entry).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.list_access_log(bigint, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.list_access_log(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_access_log(bigint, integer) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.list_access_log(bigint, integer) IS
    'List recent pgokf_private.access_log exfiltration-audit entries as pgokf.access_log_entry, newest first, optionally scoped to one bundle and bounded by max_rows. Admin-only (SECURITY DEFINER over the admin-only, tenant-scoped access log); raises 22023 when max_rows < 0.';
",
        name = "access_log_function_hardening",
        requires = [list_access_log]
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
