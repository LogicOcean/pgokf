// SPDX-License-Identifier: AGPL-3.0-only
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
//! - `pgokf.unregister_bundle(bundle_id)` - writer-tier
//!   ([`crate::security::Operation::Ingest`]). It serializes on the bundle
//!   advisory lock ([`crate::catalog::sync::advisory_lock_key`]) keyed on the
//!   **stored** canonical path, deletes the `pgokf.bundles` row (concepts,
//!   metadata, and every feature projection cascade through their foreign
//!   keys), and returns the removed [`bundle_info`](self) for good UX. An
//!   unknown `bundle_id` raises SQLSTATE `22023`. It is `SECURITY DEFINER`
//!   because write access to the base tables stays with the extension owner;
//!   `EXECUTE` is revoked from `PUBLIC` and granted to `pgokf_writer` (which
//!   `pgokf_admin` inherits);
//! - `pgokf.list_bundles()` / `pgokf.bundle_info(bundle_id)` - reader-level
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

use pgrx::datum::{Interval, TimestampWithTimeZone};
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
/// type cannot be resolved or an attribute cannot be set - both indicate a
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

/// Load every active (non-retired) bundle, ordered by identity for a stable
/// listing.
///
/// Retired bundles are excluded from the default listing (mirroring their
/// exclusion from search and traversal); they remain reachable by id through
/// `bundle_info(bundle_id)` and surface, with their `retired_at` instant, in
/// `catalog_stats`. Disabled-but-not-retired bundles are still listed, unchanged.
fn list_bundles_impl() -> Result<Vec<BundleInfo>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let query = format!(
        "SELECT {BUNDLE_INFO_COLUMNS} FROM pgokf.bundles WHERE retired_at IS NULL ORDER BY id"
    );
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
    // Write-side tenant confinement: a scoped session may only unregister its own
    // tenant's bundle; a cross-tenant or absent id looks identically unknown.
    security::enforce_bundle_tenant(bundle_id)?;
    let stored_path =
        select_bundle_path(bundle_id)?.ok_or_else(|| unknown_bundle_error(bundle_id))?;

    // Serialize with a concurrent register/refresh/unregister of the same
    // bundle on its stored canonical path before mutating catalog state.
    acquire_bundle_lock(&stored_path)?;
    let removed = delete_bundle(bundle_id)?.ok_or_else(|| unknown_bundle_error(bundle_id))?;

    // Audit trail: record the unregister in the same transaction as the delete,
    // so a logged row always means the bundle was actually removed. The counts
    // and sync_hash are NULL - an unregister has no diff. sync_log.bundle_id is
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
    // Write-side tenant confinement: a scoped session may only toggle its own
    // tenant's bundle; a cross-tenant or absent id looks identically unknown.
    security::enforce_bundle_tenant(bundle_id)?;
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

/// Authorize, lock on the stored canonical path, and set the bundle's
/// `retired_at`, returning the updated projection.
///
/// Retirement is the reversible undo window for the hard unregister cascade: it
/// sets `retired_at = now()` (which, combined with the `enabled AND retired_at IS
/// NULL` active predicate, hides the bundle from search, graph traversal, and the
/// default `list_bundles`) without deleting any catalog rows or touching the
/// independent `enabled` flag, so [`unretire_bundle_impl`] fully restores it.
///
/// Retirement is idempotent: re-retiring an already-retired bundle preserves the
/// original `retired_at` instant (`COALESCE(retired_at, now())`), so the
/// `purge_retired` age window always measures from when the bundle was first
/// retired. Serializes on the bundle advisory lock. An unknown `bundle_id` raises
/// SQLSTATE `22023`.
fn retire_bundle_impl(bundle_id: i64) -> Result<BundleInfo, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    // Write-side tenant confinement: a scoped session may only retire its own
    // tenant's bundle; a cross-tenant or absent id looks identically unknown.
    security::enforce_bundle_tenant(bundle_id)?;
    let stored_path =
        select_bundle_path(bundle_id)?.ok_or_else(|| unknown_bundle_error(bundle_id))?;

    acquire_bundle_lock(&stored_path)?;
    let statement = format!(
        "UPDATE pgokf.bundles
         SET retired_at = COALESCE(retired_at, pg_catalog.now())
         WHERE id = $1 RETURNING {BUNDLE_INFO_COLUMNS}"
    );
    Spi::connect_mut(|client| {
        let mut table = client
            .update(statement.as_str(), None, &[bundle_id.into()])
            .map_err(|error| spi_error("failed to retire bundle", &error))?;
        table.next().map(|row| read_bundle_info(&row)).transpose()
    })?
    .ok_or_else(|| unknown_bundle_error(bundle_id))
}

/// Authorize, lock on the stored canonical path, and clear the bundle's
/// `retired_at`, returning the updated projection.
///
/// Reverses [`retire_bundle_impl`]: clearing `retired_at` makes the bundle active
/// again (subject to its `enabled` flag) with every catalog row intact.
/// Serializes on the bundle advisory lock. An unknown `bundle_id` raises SQLSTATE
/// `22023`.
fn unretire_bundle_impl(bundle_id: i64) -> Result<BundleInfo, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;
    let stored_path =
        select_bundle_path(bundle_id)?.ok_or_else(|| unknown_bundle_error(bundle_id))?;

    acquire_bundle_lock(&stored_path)?;
    let statement = format!(
        "UPDATE pgokf.bundles SET retired_at = NULL WHERE id = $1 RETURNING {BUNDLE_INFO_COLUMNS}"
    );
    Spi::connect_mut(|client| {
        let mut table = client
            .update(statement.as_str(), None, &[bundle_id.into()])
            .map_err(|error| spi_error("failed to unretire bundle", &error))?;
        table.next().map(|row| read_bundle_info(&row)).transpose()
    })?
    .ok_or_else(|| unknown_bundle_error(bundle_id))
}

/// Read the (id, path) of every bundle retired longer than `older_than`, scoped
/// to the session's tenant.
///
/// The tenant predicate mirrors the row-level-security policy inline (this runs
/// on the `SECURITY DEFINER` purge path, which bypasses RLS): an unset/empty
/// `pgokf.tenant` sees every retired bundle (the backward-compatible
/// operator/superuser default; under `require_tenant` the caller was already
/// refused before reaching this), while a scoped session sees only its own.
fn select_purgeable_bundles(older_than: Interval) -> Result<Vec<(i64, String)>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT id, path
                 FROM pgokf.bundles
                 WHERE retired_at IS NOT NULL
                   AND retired_at < pg_catalog.now() - $1
                   AND (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                       OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                      AND NOT (SELECT pgokf.tenant_required()))
                     OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
                 ORDER BY id",
                None,
                &[older_than.into()],
            )
            .map_err(|error| spi_error("failed to select purgeable bundles", &error))?;
        let mut rows = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read purgeable bundle", "bundle");
            let id = reader.required::<i64>(1, "id")?;
            let path = reader.required::<String>(2, "path")?;
            rows.push((id, path));
        }
        Ok(rows)
    })
}

/// Hard-delete a single bundle by id under the caller-held advisory lock, but
/// only if it is *still* purge-eligible, returning whether a row was removed.
///
/// This re-evaluates the eligibility predicate (`retired_at IS NOT NULL AND
/// retired_at < now() - older_than`) inside the `DELETE ... WHERE`, atomically,
/// so it closes the TOCTOU window between [`select_purgeable_bundles`] snapshot
/// and this delete: the candidate list is taken without the per-bundle advisory
/// lock, so a concurrent `unretire_bundle` (which takes that same lock) can
/// commit in between and restore a bundle. Because the re-check runs under the
/// now-held lock - after that `unretire` has committed and released it - a
/// bundle that is no longer retired (or was re-retired inside the window) simply
/// fails the `WHERE` and is skipped, never hard-deleted. `older_than` is bound
/// as a parameter; the same transaction-stable `now()` the snapshot used is
/// re-read here.
fn delete_bundle_row_if_eligible(
    bundle_id: i64,
    older_than: Interval,
) -> Result<bool, CatalogError> {
    Spi::connect_mut(|client| {
        let mut table = client
            .update(
                "DELETE FROM pgokf.bundles
                 WHERE id = $1
                   AND retired_at IS NOT NULL
                   AND retired_at < pg_catalog.now() - $2
                 RETURNING id",
                None,
                &[bundle_id.into(), older_than.into()],
            )
            .map_err(|error| spi_error("failed to purge retired bundle", &error))?;
        Ok(table.next().is_some())
    })
}

/// Authorize and hard-delete every bundle retired longer than `older_than`,
/// returning the count purged.
///
/// Each purged bundle is serialized on its own advisory lock and deleted exactly
/// like `unregister_bundle` - the `pgokf.concepts` cascade removes concepts,
/// metadata, and every feature projection - and an `unregister` audit row is
/// written per purged bundle (its counts and hash `NULL`, like a real
/// unregister). Only bundles whose `retired_at` predates `now() - older_than` are
/// eligible, so an in-window retired bundle stays recoverable; `unregister_bundle`
/// remains a separate immediate hard delete.
fn purge_retired_impl(older_than: Interval) -> Result<i64, CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    // A bulk mutation with no bundle id: under the require_tenant policy an
    // unscoped session is refused (42501) like every other writer, rather than
    // silently purging nothing - a nightly purge job must fail loudly, not
    // stop working unnoticed.
    security::enforce_active_tenant()?;
    let retention_days = crate::catalog::config::sync_log_retention_days()?;
    let candidates = select_purgeable_bundles(older_than)?;

    let mut purged: i64 = 0;
    for (bundle_id, stored_path) in candidates {
        // Serialize with a concurrent register/refresh/unregister/unretire of the
        // same bundle before mutating, exactly as unregister does. The eligibility
        // predicate is then re-checked inside the DELETE under this lock, so a
        // bundle a concurrent unretire_bundle restored between the snapshot and
        // this iteration is skipped rather than silently hard-deleted.
        acquire_bundle_lock(&stored_path)?;
        if delete_bundle_row_if_eligible(bundle_id, older_than)? {
            // Same FK-free unregister audit row a manual unregister writes; the
            // returned sync id is unused (a purge has no per-concept manifest).
            let _ = crate::catalog::audit::record(
                bundle_id,
                &stored_path,
                "unregister",
                None,
                None,
                retention_days,
            )?;
            purged += 1;
        }
    }
    Ok(purged)
}

/// SQL-facing administration entry points, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::datum::Interval;
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{
        bundle_info_impl, bundle_info_tuple, list_bundles_impl, purge_retired_impl,
        retire_bundle_impl, set_bundle_enabled_impl, unknown_bundle_error, unregister_bundle_impl,
        unretire_bundle_impl,
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

    /// Retire (soft-delete) a registered bundle, returning the updated info.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). Sets `retired_at = now()`, which excludes the bundle from
    /// `concept_search`, `concept_neighbors`, and the default `list_bundles`
    /// without deleting any rows - a reversible undo window for the hard
    /// unregister cascade. Idempotent: re-retiring keeps the original
    /// `retired_at`. Does not alter the independent `enabled` flag. Serializes on
    /// the bundle advisory lock. Raises SQLSTATE `22023` when `bundle_id` is
    /// unknown.
    #[pg_extern(requires = ["bundle_info_type"])]
    fn retire_bundle(bundle_id: i64) -> pgrx::composite_type!('static, "pgokf.bundle_info") {
        let info = retire_bundle_impl(bundle_id).unwrap_or_else(|error| error.raise());
        bundle_info_tuple(info).unwrap_or_else(|error| error.raise())
    }

    /// Un-retire a bundle, clearing `retired_at`, and return the updated info.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). Fully reverses `retire_bundle`: the bundle becomes active
    /// again (subject to its `enabled` flag) with every catalog row intact.
    /// Serializes on the bundle advisory lock. Raises SQLSTATE `22023` when
    /// `bundle_id` is unknown.
    #[pg_extern(requires = ["bundle_info_type"])]
    fn unretire_bundle(bundle_id: i64) -> pgrx::composite_type!('static, "pgokf.bundle_info") {
        let info = unretire_bundle_impl(bundle_id).unwrap_or_else(|error| error.raise());
        bundle_info_tuple(info).unwrap_or_else(|error| error.raise())
    }

    /// Hard-delete every bundle retired longer than `older_than`, returning the
    /// count purged.
    ///
    /// Requires membership in `pgokf_admin`. Each eligible bundle (its
    /// `retired_at` older than `now() - older_than`) is deleted like
    /// `unregister_bundle` - concepts, metadata, and every feature projection
    /// cascade - and an `unregister` audit row is written per purged bundle. A
    /// retired bundle still inside the window is left recoverable via
    /// `unretire_bundle`.
    #[pg_extern(requires = ["bundle_info_type"])]
    fn purge_retired(older_than: default!(Interval, "'7 days'")) -> i64 {
        purge_retired_impl(older_than).unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.unregister_bundle(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.set_bundle_enabled(bigint, boolean)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.retire_bundle(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.unretire_bundle(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.purge_retired(interval)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.retire_bundle(bigint) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.unretire_bundle(bigint) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.purge_retired(interval) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.retire_bundle(bigint) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.unretire_bundle(bigint) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.purge_retired(interval) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.retire_bundle(bigint) IS
    'Retire (soft-delete) a bundle: set retired_at = now(), returning the updated pgokf.bundle_info. Writer-tier (pgokf_writer; admin inherits it). Excludes the bundle from concept_search, concept_neighbors, and the default list_bundles without deleting rows (reversible via unretire_bundle); idempotent (keeps the original retired_at); does not change enabled. Raises 22023 if the bundle_id is unknown.';
COMMENT ON FUNCTION pgokf.unretire_bundle(bigint) IS
    'Un-retire a bundle: clear retired_at, returning the updated pgokf.bundle_info. Writer-tier (pgokf_writer; admin inherits it); fully reverses retire_bundle. Raises 22023 if the bundle_id is unknown.';
COMMENT ON FUNCTION pgokf.purge_retired(interval) IS
    'Hard-delete every bundle whose retired_at is older than now() - older_than (default 7 days); returns the count purged. Admin-only (pgokf_admin). Each purge cascades concept/metadata/feature rows and writes an unregister audit row; a bundle retired within the window stays recoverable via unretire_bundle. unregister_bundle remains a separate immediate hard delete.';
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
    'List every active (non-retired) registered bundle as pgokf.bundle_info, ordered by id. Reader-level. Retired bundles are excluded (reachable by id via bundle_info, and visible with their retired_at in catalog_stats); disabled-but-not-retired bundles are still listed.';
COMMENT ON FUNCTION pgokf.bundle_info(bigint) IS
    'Return one registered bundle as pgokf.bundle_info. Reader-level; raises 22023 if the bundle_id is unknown.';
",
        name = "admin_function_hardening",
        requires = [
            unregister_bundle,
            list_bundles,
            bundle_info,
            set_bundle_enabled,
            retire_bundle,
            unretire_bundle,
            purge_retired
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
