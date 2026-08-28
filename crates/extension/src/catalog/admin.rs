//! Bundle-administration API: `bundle_info`, `unregister_bundle`,
//! `list_bundles`.
//!
//! This module fills the admin seam described in
//! [`crate::catalog`](crate::catalog): the SQL-facing administration surface,
//! attached without touching the sync engine.
//!
//! # Objects owned here
//!
//! - the `pgokf.bundle_info` composite type
//!   `(id, path, name, okf_version, file_count, last_synced_at, enabled)`,
//!   created in its own `bundle_info_type` SQL block ordered after
//!   `catalog_tables`. The core deliberately does not create it because
//!   neither sync nor search needs it;
//! - `pgokf.unregister_bundle(bundle_id)` — writer-tier
//!   ([`crate::security::Operation::Ingest`]). It serializes on the bundle
//!   advisory lock ([`crate::catalog::sync::advisory_lock_key`]) keyed on the
//!   **stored** canonical path, deletes the `pgokf.bundles` row (concepts,
//!   metadata, and every feature projection cascade through their foreign
//!   keys), and returns the removed [`bundle_info`](self) for good UX. An
//!   unknown `bundle_id` raises SQLSTATE `22023`. It is `SECURITY DEFINER`
//!   because write access to the base tables stays with the extension owner;
//!   `EXECUTE` is revoked from `PUBLIC` and granted to `pgokf_writer` (which
//!   `pgokf_admin` inherits);
//! - `pgokf.list_bundles()` / `pgokf.bundle_info(bundle_id)` — reader-level
//!   ([`crate::security::Operation::Search`]), `STABLE` projections over
//!   `pgokf.bundles`. Like [`crate::catalog::search`], they run with invoker
//!   rights over the `SELECT` grant `pgokf_reader` already holds; `EXECUTE`
//!   is granted to `pgokf_reader`.
//!
//! # `okf_version`
//!
//! `pgokf.bundles.okf_version` is projected verbatim and is currently always
//! `NULL`: the core sync engine never populates it, and this module
//! deliberately does not either. Surfacing the bundle-level `index.md`
//! metadata that would fill it is left to the configuration wave (see the
//! seam note in [`crate::catalog::sync`]).

use std::path::Path;

use pgrx::datum::TimestampWithTimeZone;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::spi::SpiHeapTupleData;
use pgrx::{AllocatedByRust, Spi};

use crate::catalog::spi_read::RowReader;
use crate::catalog::sync::advisory_lock_key;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the bundle-info composite type.
const BUNDLE_INFO_TYPE: &str = "pgokf.bundle_info";

/// Column projection shared by every `bundle_info` read, in the attribute
/// order of [`BUNDLE_INFO_TYPE`] and of [`read_bundle_info`].
const BUNDLE_INFO_COLUMNS: &str =
    "id, path, name, okf_version, file_count, last_synced_at, enabled";

/// One registered bundle projected onto the `pgokf.bundle_info` shape.
///
/// `okf_version` is carried through unchanged from `pgokf.bundles` and is
/// presently always `None` (see the module docs).
struct BundleInfo {
    id: i64,
    path: String,
    name: Option<String>,
    okf_version: Option<String>,
    file_count: i32,
    last_synced_at: Option<TimestampWithTimeZone>,
    enabled: bool,
}

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {BUNDLE_INFO_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Build the error raised when a caller names a `bundle_id` that is not
/// registered.
///
/// Kept as a standalone helper so both the read and unregister paths raise an
/// identical, contractually fixed SQLSTATE `22023`
/// ([`crate::errors::ErrorKind::InvalidParameter`]).
fn unknown_bundle_error(bundle_id: i64) -> CatalogError {
    CatalogError::invalid_parameter(
        format!("bundle {bundle_id} is not registered"),
        Path::new(""),
    )
}

/// Read one `pgokf.bundles` row projected via [`BUNDLE_INFO_COLUMNS`].
///
/// Ordinals follow the column list: `id`, `path`, `name`, `okf_version`,
/// `file_count`, `last_synced_at`, `enabled`. The `NOT NULL` columns are
/// treated as internal invariants and surface as `XX000` if ever violated.
fn read_bundle_info(row: &SpiHeapTupleData<'_>) -> Result<BundleInfo, CatalogError> {
    let reader = RowReader::new(row, "failed to read bundle_info column", "bundle_info");
    Ok(BundleInfo {
        id: reader.required(1, "id")?,
        path: reader.required(2, "path")?,
        name: reader.optional(3)?,
        okf_version: reader.optional(4)?,
        file_count: reader.required(5, "file_count")?,
        last_synced_at: reader.optional::<TimestampWithTimeZone>(6)?,
        enabled: reader.required(7, "enabled")?,
    })
}

/// Pack a [`BundleInfo`] into a `pgokf.bundle_info` heap tuple.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::Internal`] error when the composite
/// type cannot be resolved or an attribute cannot be set — both indicate a
/// corrupted installation, since `bundle_info_type` defines the type.
fn bundle_info_tuple(
    info: BundleInfo,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type(BUNDLE_INFO_TYPE).map_err(composite_error)?;
    tuple.set_by_name("id", info.id).map_err(composite_error)?;
    tuple
        .set_by_name("path", info.path)
        .map_err(composite_error)?;
    tuple
        .set_by_name("name", info.name)
        .map_err(composite_error)?;
    tuple
        .set_by_name("okf_version", info.okf_version)
        .map_err(composite_error)?;
    tuple
        .set_by_name("file_count", info.file_count)
        .map_err(composite_error)?;
    tuple
        .set_by_name("last_synced_at", info.last_synced_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("enabled", info.enabled)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// Look up the stored canonical path of a bundle, if it is registered.
///
/// Read-only SPI: the caller uses the returned path only to key the advisory
/// lock before mutating.
fn select_bundle_path(bundle_id: i64) -> Result<Option<String>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT path FROM pgokf.bundles WHERE id = $1",
                Some(1),
                &[bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to look up bundle path", &error))?;
        if table.is_empty() {
            return Ok(None);
        }
        table
            .first()
            .get_one::<String>()
            .map_err(|error| spi_error("failed to read bundle path", &error))
    })
}

/// Acquire the transaction-scoped bundle advisory lock for a canonical path,
/// mirroring the sync engine's serialization so administration cannot race a
/// concurrent register/refresh of the same bundle.
fn acquire_bundle_lock(canonical_path: &str) -> Result<(), CatalogError> {
    let key = advisory_lock_key(canonical_path);
    Spi::run_with_args("SELECT pg_catalog.pg_advisory_xact_lock($1)", &[key.into()])
        .map_err(|error| spi_error("failed to acquire bundle advisory lock", &error))
}

/// Delete a bundle row and return its projected `bundle_info`.
///
/// The `DELETE ... RETURNING` is atomic under the advisory lock, so a bundle
/// deleted by a concurrent unregister simply returns no row and is reported as
/// unknown by the caller.
fn delete_bundle(bundle_id: i64) -> Result<Option<BundleInfo>, CatalogError> {
    let statement =
        format!("DELETE FROM pgokf.bundles WHERE id = $1 RETURNING {BUNDLE_INFO_COLUMNS}");
    Spi::connect_mut(|client| {
        let mut table = client
            .update(statement.as_str(), None, &[bundle_id.into()])
            .map_err(|error| spi_error("failed to delete bundle", &error))?;
        table.next().map(|row| read_bundle_info(&row)).transpose()
    })
}

/// Load every bundle, ordered by identity for a stable listing.
fn list_bundles_impl() -> Result<Vec<BundleInfo>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let query = format!("SELECT {BUNDLE_INFO_COLUMNS} FROM pgokf.bundles ORDER BY id");
    Spi::connect(|client| {
        let table = client
            .select(query.as_str(), None, &[])
            .map_err(|error| spi_error("failed to list bundles", &error))?;
        let mut bundles = Vec::with_capacity(table.len());
        for row in table {
            bundles.push(read_bundle_info(&row)?);
        }
        Ok(bundles)
    })
}

/// Load one bundle by identity, or `None` when it is not registered.
fn bundle_info_impl(bundle_id: i64) -> Result<Option<BundleInfo>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let query = format!("SELECT {BUNDLE_INFO_COLUMNS} FROM pgokf.bundles WHERE id = $1");
    Spi::connect(|client| {
        let mut table = client
            .select(query.as_str(), Some(1), &[bundle_id.into()])
            .map_err(|error| spi_error("failed to look up bundle", &error))?;
        table.next().map(|row| read_bundle_info(&row)).transpose()
    })
}

/// Authorize, lock on the stored canonical path, and delete the bundle,
/// returning the removed projection.
fn unregister_bundle_impl(bundle_id: i64) -> Result<BundleInfo, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    let stored_path =
        select_bundle_path(bundle_id)?.ok_or_else(|| unknown_bundle_error(bundle_id))?;

    // Serialize with a concurrent register/refresh/unregister of the same
    // bundle on its stored canonical path before mutating catalog state.
    acquire_bundle_lock(&stored_path)?;
    let removed = delete_bundle(bundle_id)?.ok_or_else(|| unknown_bundle_error(bundle_id))?;

    // Audit trail: record the unregister in the same transaction as the delete,
    // so a logged row always means the bundle was actually removed. The counts
    // and sync_hash are NULL — an unregister has no diff. sync_log.bundle_id is
    // intentionally FK-free, so the row survives the bundle row's deletion.
    let retention_days = crate::catalog::config::sync_log_retention_days()?;
    crate::catalog::audit::record(
        bundle_id,
        &stored_path,
        "unregister",
        None,
        None,
        retention_days,
    )?;
    Ok(removed)
}

/// Authorize, lock on the stored canonical path, and flip the bundle's
/// `enabled` flag, returning the updated projection.
///
/// A disabled bundle's concepts are excluded from ranked search
/// ([`crate::catalog::search`]) and graph traversal
/// ([`crate::catalog::neighbors`]) without removing any catalog rows, so
/// re-enabling restores the bundle exactly. Serializes on the bundle advisory
/// lock so the toggle cannot race a concurrent sync of the same bundle. An
/// unknown `bundle_id` raises SQLSTATE `22023`.
fn set_bundle_enabled_impl(bundle_id: i64, enabled: bool) -> Result<BundleInfo, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    let stored_path =
        select_bundle_path(bundle_id)?.ok_or_else(|| unknown_bundle_error(bundle_id))?;

    acquire_bundle_lock(&stored_path)?;
    let statement = format!(
        "UPDATE pgokf.bundles SET enabled = $2 WHERE id = $1 RETURNING {BUNDLE_INFO_COLUMNS}"
    );
    Spi::connect_mut(|client| {
        let mut table = client
            .update(
                statement.as_str(),
                None,
                &[bundle_id.into(), enabled.into()],
            )
            .map_err(|error| spi_error("failed to set bundle enabled flag", &error))?;
        table.next().map(|row| read_bundle_info(&row)).transpose()
    })?
    .ok_or_else(|| unknown_bundle_error(bundle_id))
}

/// SQL-facing administration entry points, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{extension_sql, pg_extern};

    use super::{
        bundle_info_impl, bundle_info_tuple, list_bundles_impl, set_bundle_enabled_impl,
        unknown_bundle_error, unregister_bundle_impl,
    };

    extension_sql!(
        r"
CREATE TYPE pgokf.bundle_info AS (
    id             bigint,
    path           text,
    name           text,
    okf_version    text,
    file_count     integer,
    last_synced_at timestamptz,
    enabled        boolean
);

COMMENT ON TYPE pgokf.bundle_info IS
    'Administrative view of one registered OKF bundle: identity, canonical path, sync state, and enabled flag.';
",
        name = "bundle_info_type",
        requires = ["catalog_tables"]
    );

    /// Unregister a bundle and return the removed bundle's info.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). Serializes on the bundle advisory lock keyed on the
    /// stored canonical path, then deletes the `pgokf.bundles` row; concepts,
    /// metadata, and every feature projection cascade through their foreign
    /// keys. Raises SQLSTATE `22023` when `bundle_id` is not registered.
    #[pg_extern(requires = ["bundle_info_type"])]
    fn unregister_bundle(bundle_id: i64) -> pgrx::composite_type!('static, "pgokf.bundle_info") {
        let info = unregister_bundle_impl(bundle_id).unwrap_or_else(|error| error.raise());
        bundle_info_tuple(info).unwrap_or_else(|error| error.raise())
    }

    /// List every registered bundle, ordered by identity.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`).
    #[pg_extern(stable, requires = ["bundle_info_type"])]
    fn list_bundles() -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.bundle_info")>
    {
        let bundles = list_bundles_impl().unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = bundles
            .into_iter()
            .map(|info| bundle_info_tuple(info).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    /// Return the info for one registered bundle.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Raises
    /// SQLSTATE `22023` when `bundle_id` is not registered.
    #[pg_extern(stable, requires = ["bundle_info_type"])]
    fn bundle_info(bundle_id: i64) -> pgrx::composite_type!('static, "pgokf.bundle_info") {
        let info = bundle_info_impl(bundle_id)
            .unwrap_or_else(|error| error.raise())
            .unwrap_or_else(|| unknown_bundle_error(bundle_id).raise());
        bundle_info_tuple(info).unwrap_or_else(|error| error.raise())
    }

    /// Enable or disable a registered bundle, returning the updated info.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). A disabled bundle's concepts are excluded from ranked
    /// search and graph traversal without deleting any rows, so the toggle is
    /// fully reversible. Serializes on the bundle advisory lock. Raises SQLSTATE
    /// `22023` when `bundle_id` is not registered.
    #[pg_extern(requires = ["bundle_info_type"])]
    fn set_bundle_enabled(
        bundle_id: i64,
        enabled: bool,
    ) -> pgrx::composite_type!('static, "pgokf.bundle_info") {
        let info =
            set_bundle_enabled_impl(bundle_id, enabled).unwrap_or_else(|error| error.raise());
        bundle_info_tuple(info).unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.unregister_bundle(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.set_bundle_enabled(bigint, boolean)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.set_bundle_enabled(bigint, boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.set_bundle_enabled(bigint, boolean) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.set_bundle_enabled(bigint, boolean) IS
    'Enable or disable a registered bundle, returning the updated pgokf.bundle_info. Writer-tier (pgokf_writer; admin inherits it); a disabled bundle is hidden from concept_search and concept_neighbors without deleting rows (reversible). Raises 22023 if the bundle_id is unknown.';
REVOKE ALL ON FUNCTION pgokf.unregister_bundle(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.unregister_bundle(bigint) TO pgokf_writer;
REVOKE ALL ON FUNCTION pgokf.list_bundles() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_bundles() TO pgokf_reader;
REVOKE ALL ON FUNCTION pgokf.bundle_info(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.bundle_info(bigint) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.unregister_bundle(bigint) IS
    'Unregister a bundle and return the removed bundle_info. Writer-tier (pgokf_writer; admin inherits it); concept/metadata/feature rows cascade. Raises 22023 if the bundle_id is unknown.';
COMMENT ON FUNCTION pgokf.list_bundles() IS
    'List every registered bundle as pgokf.bundle_info, ordered by id. Reader-level.';
COMMENT ON FUNCTION pgokf.bundle_info(bigint) IS
    'Return one registered bundle as pgokf.bundle_info. Reader-level; raises 22023 if the bundle_id is unknown.';
",
        name = "admin_function_hardening",
        requires = [
            unregister_bundle,
            list_bundles,
            bundle_info,
            set_bundle_enabled
        ]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;

    #[test]
    fn unknown_bundle_error_maps_to_invalid_parameter_sqlstate() {
        // Arrange
        let bundle_id = 42_i64;

        // Act
        let error = unknown_bundle_error(bundle_id);

        // Assert
        assert_eq!(error.kind(), ErrorKind::InvalidParameter);
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn unknown_bundle_error_names_the_offending_bundle_id() {
        // Arrange
        let bundle_id = 7_i64;

        // Act
        let message = unknown_bundle_error(bundle_id).message().to_owned();

        // Assert
        assert!(message.contains('7'));
        assert!(message.contains("not registered"));
    }
}
