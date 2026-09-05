// SPDX-License-Identifier: AGPL-3.0-only
//! Optional `pg_cron` scheduled bundle re-sync: `pgokf.schedule_refresh` /
//! `pgokf.unschedule_refresh`.
//!
//! These register (and remove) a recurring `pgokf.refresh_bundle` job on the
//! external `pg_cron` scheduler. Exactly like the `pg_search` BM25 adapter in
//! [`crate::catalog::search_backend`] and the `pgvector` semantic surface in
//! [`crate::catalog::embedding`], the coupling to `pg_cron` is **runtime-only**:
//! the extension is compiled and installed with no build-time reference to it, so
//! `CREATE EXTENSION pgokf` succeeds where `pg_cron` is absent, and every
//! `cron.*` object is reached solely through dynamic SPI. When `pg_cron` is not
//! installed, [`schedule_refresh`] raises a clear SQLSTATE `22023` naming the
//! missing dependency - mirroring `concept_search_semantic`'s pgvector error,
//! never a silent success - and [`unschedule_refresh`] is a clean no-op.
//!
//! # Injection safety
//!
//! The scheduled command is `SELECT pg_catalog.set_config('pgokf.tenant', <tenant>,
//! false); SELECT pgokf.refresh_bundle(<id>)`. The `<id>` is the `bigint`
//! `bundle_id` the caller passed, formatted as an integer literal that this code
//! controls - a validated `i64` can only render as digits - and `<tenant>` is
//! the bundle's stored `tenant_id`, quoted by `PostgreSQL`'s own `format('%L')`
//! inside the scheduling statement rather than by Rust string handling, so no
//! caller text ever enters the command unquoted. Pinning the tenant makes the
//! cron worker's own, tenant-less session satisfy the tenant rules (it refreshes
//! exactly the bundle's tenant, and passes the `require_tenant` policy). The
//! cron schedule and the deterministic job name both bind as parameters to
//! `cron.schedule` (never
//! interpolated). Full scheduling requires `pg_cron` in
//! `shared_preload_libraries`.

use std::path::Path;

use pgrx::Spi;

use crate::errors::CatalogError;
use crate::security;

/// Longest accepted cron schedule string. `pg_cron` accepts a 5-field cron
/// expression or a short interval phrase (`'30 seconds'`, `'1 hour'`); both are
/// comfortably under this bound, which is defense in depth on top of the
/// parameter binding.
const MAX_SCHEDULE_LEN: usize = 128;

fn spi_error(context: &'static str) -> impl Fn(pgrx::spi::Error) -> CatalogError {
    move |error| CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// The deterministic `pg_cron` job name for a bundle's refresh job.
///
/// Fixed as `pgokf_refresh_<bundle_id>` so re-scheduling the same bundle updates
/// the one job in place (idempotent) and [`unschedule_refresh`] can find it by
/// name. `bundle_id` is an `i64`, so this only ever renders `pgokf_refresh_`
/// followed by digits (and possibly a leading `-`).
fn job_name(bundle_id: i64) -> String {
    format!("pgokf_refresh_{bundle_id}")
}

/// The fixed scheduled command for a bundle. The `bundle_id` is a validated
/// `i64` formatted as an integer literal this code controls - never caller text.
fn refresh_command(bundle_id: i64) -> String {
    format!("SELECT pgokf.refresh_bundle({bundle_id})")
}

/// Report whether the `pg_cron` extension is installed in this database.
///
/// Probed by `pg_extension` catalog membership - never by referencing a `cron.*`
/// object - so it is safe to call before any SQL that would fail to resolve when
/// `pg_cron` is absent.
fn pg_cron_installed() -> Result<bool, CatalogError> {
    Spi::get_one::<bool>(
        "SELECT pg_catalog.count(*) > 0 FROM pg_catalog.pg_extension WHERE extname = 'pg_cron'",
    )
    .map_err(spi_error("failed to check for the pg_cron extension"))?
    .ok_or_else(|| CatalogError::internal("pg_cron probe returned no row", Path::new("")))
}

/// The clear `22023` raised when scheduling needs `pg_cron` but it is absent.
/// Scheduling has no in-database fallback, so this is an error rather than a
/// silent success - the same contract `concept_search_semantic` uses for
/// pgvector.
fn missing_pg_cron_error() -> CatalogError {
    CatalogError::invalid_parameter(
        "scheduled refresh requires the pg_cron extension, which is not installed; \
         add pg_cron to shared_preload_libraries and run CREATE EXTENSION pg_cron, \
         or refresh manually with pgokf.refresh_bundle",
        Path::new(""),
    )
}

/// Validate the cron schedule string: non-empty, within [`MAX_SCHEDULE_LEN`], and
/// NUL-free. The value binds as a parameter to `cron.schedule`, so this is
/// defense in depth against a surprising schedule rather than the sole barrier;
/// `pg_cron` itself rejects a malformed expression.
fn validate_schedule(schedule: &str) -> Result<(), CatalogError> {
    if schedule.trim().is_empty() {
        return Err(CatalogError::invalid_parameter(
            "schedule must not be empty (e.g. '0 * * * *' or '30 minutes')",
            Path::new(""),
        ));
    }
    if schedule.len() > MAX_SCHEDULE_LEN {
        return Err(CatalogError::invalid_parameter(
            format!(
                "schedule must be at most {MAX_SCHEDULE_LEN} bytes, got {}",
                schedule.len()
            ),
            Path::new(""),
        ));
    }
    if schedule.contains('\0') {
        return Err(CatalogError::invalid_parameter(
            "schedule must not contain NUL bytes",
            Path::new(""),
        ));
    }
    Ok(())
}

/// Authorize (admin), confine to the tenant, validate, require `pg_cron`, and
/// register the recurring refresh job. Returns the deterministic job name.
fn schedule_refresh_impl(bundle_id: i64, schedule: &str) -> Result<String, CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    // Write-side tenant confinement before any side effect: a scoped session may
    // only schedule its own tenant's bundle, and an unknown/cross-tenant bundle
    // is rejected identically (22023), so a guessed id cannot probe another
    // tenant's catalog.
    security::enforce_bundle_tenant(bundle_id)?;
    validate_schedule(schedule)?;
    if !pg_cron_installed()? {
        return Err(missing_pg_cron_error());
    }

    let name = job_name(bundle_id);
    let command = refresh_command(bundle_id);
    // cron.schedule(job_name, schedule, command) upserts by name, so re-scheduling
    // the same bundle updates the one job in place. The name, schedule, and the
    // refresh call all bind as parameters; the job command pins the bundle's
    // own tenant first (read with owner rights - enforce_bundle_tenant already
    // confirmed the caller may act on this bundle), quoted by format('%L') so
    // the cron session, which carries no pgokf.tenant of its own, refreshes
    // under exactly that tenant and satisfies the require_tenant policy.
    Spi::get_one_with_args::<i64>(
        "SELECT cron.schedule(
             $1,
             $2,
             pg_catalog.format(
                 'SELECT pg_catalog.set_config(''pgokf.tenant'', %L, false); %s',
                 (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $4),
                 $3))",
        &[
            name.as_str().into(),
            schedule.into(),
            command.as_str().into(),
            bundle_id.into(),
        ],
    )
    .map_err(spi_error("failed to register the pg_cron refresh job"))?;
    Ok(name)
}

/// Authorize (admin), confine to the tenant, and remove the refresh job if
/// present. `true` when a job was removed, `false` for a clean no-op (no
/// `pg_cron`, or no such job).
fn unschedule_refresh_impl(bundle_id: i64) -> Result<bool, CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;

    if !pg_cron_installed()? {
        pgrx::notice!(
            "pgokf: pg_cron is not installed; unschedule_refresh is a no-op. There is no \
             scheduled job to remove."
        );
        return Ok(false);
    }

    let name = job_name(bundle_id);
    // Only unschedule a job that exists - cron.unschedule(name) raises if the job
    // is absent, so a no-op removal stays a clean `false` rather than an error.
    let exists = Spi::get_one_with_args::<bool>(
        "SELECT pg_catalog.count(*) > 0 FROM cron.job WHERE jobname = $1",
        &[name.as_str().into()],
    )
    .map_err(spi_error("failed to look up the pg_cron refresh job"))?
    .unwrap_or(false);
    if !exists {
        return Ok(false);
    }

    Spi::get_one_with_args::<bool>("SELECT cron.unschedule($1)", &[name.as_str().into()])
        .map_err(spi_error("failed to remove the pg_cron refresh job"))?
        .ok_or_else(|| CatalogError::internal("cron.unschedule returned no row", Path::new("")))
}

/// SQL-facing scheduled-refresh entry points, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{extension_sql, pg_extern};

    use super::{schedule_refresh_impl, unschedule_refresh_impl};

    /// Schedule a recurring `pgokf.refresh_bundle` for a bundle via `pg_cron`.
    ///
    /// Requires membership in `pgokf_admin`. When `pg_cron` is installed, this
    /// registers (or re-schedules, idempotently) a cron job named
    /// `pgokf_refresh_<bundle_id>` that pins the bundle's tenant and runs
    /// `SELECT pgokf.refresh_bundle(<id>)`
    /// on the given cron `schedule` (a 5-field cron expression or a `pg_cron`
    /// interval phrase such as `'30 minutes'`), returning the job name. **Requires
    /// pg_cron**: raises SQLSTATE `22023` naming the missing dependency when it is
    /// not installed (no silent success). Raises `22023` for an unknown or
    /// cross-tenant `bundle_id`, or an empty/oversized `schedule`.
    #[pg_extern(requires = ["catalog_tables"])]
    fn schedule_refresh(bundle_id: i64, schedule: &str) -> String {
        schedule_refresh_impl(bundle_id, schedule).unwrap_or_else(|error| error.raise())
    }

    /// Remove the scheduled `pgokf.refresh_bundle` job for a bundle.
    ///
    /// Requires membership in `pgokf_admin`. Removes the `pgokf_refresh_<bundle_id>`
    /// `pg_cron` job when present, returning `true`; returns `false` for a clean
    /// no-op when `pg_cron` is not installed (with a `NOTICE`) or no such job
    /// exists. Raises SQLSTATE `22023` for an unknown or cross-tenant `bundle_id`.
    #[pg_extern(requires = ["catalog_tables"])]
    fn unschedule_refresh(bundle_id: i64) -> bool {
        unschedule_refresh_impl(bundle_id).unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.schedule_refresh(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.unschedule_refresh(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.schedule_refresh(bigint, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.unschedule_refresh(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.schedule_refresh(bigint, text) TO pgokf_admin;
GRANT EXECUTE ON FUNCTION pgokf.unschedule_refresh(bigint) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.schedule_refresh(bigint, text) IS
    'Schedule a recurring pgokf.refresh_bundle(<bundle_id>) via pg_cron under the deterministic job name pgokf_refresh_<bundle_id> (idempotent/re-schedulable), returning the job name. Admin-only, SECURITY DEFINER, tenant-confined. The scheduled command pins the bundle''s tenant (set_config(''pgokf.tenant'', <tenant_id>, false), quoted by format(%L)) and then runs SELECT pgokf.refresh_bundle(<id>) with the id as a trusted integer literal, so the cron worker''s own session satisfies the tenant rules and the require_tenant policy; the schedule and job name bind as parameters. Requires pg_cron: raises 22023 naming the missing dependency when it is not installed (no silent success), and 22023 for an unknown/cross-tenant bundle_id or an empty/oversized schedule. Full scheduling requires pg_cron in shared_preload_libraries.';
COMMENT ON FUNCTION pgokf.unschedule_refresh(bigint) IS
    'Remove the pgokf_refresh_<bundle_id> pg_cron refresh job when present (returns true); a clean no-op returning false (with a NOTICE) when pg_cron is not installed or no such job exists. Admin-only, SECURITY DEFINER, tenant-confined; raises 22023 for an unknown/cross-tenant bundle_id.';
",
        name = "schedule_refresh_hardening",
        requires = [schedule_refresh, unschedule_refresh]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_name_is_deterministic_for_a_bundle() {
        // Arrange / Act / Assert
        assert_eq!(job_name(42), "pgokf_refresh_42");
        assert_eq!(job_name(1), "pgokf_refresh_1");
    }

    #[test]
    fn refresh_command_binds_the_bundle_id_as_an_integer_literal() {
        // Arrange / Act
        let command = refresh_command(7);

        // Assert: the id renders only as digits inside the fixed command.
        assert_eq!(command, "SELECT pgokf.refresh_bundle(7)");
    }

    #[test]
    fn validate_schedule_accepts_a_cron_expression_and_an_interval_phrase() {
        // Arrange / Act / Assert
        assert!(validate_schedule("0 * * * *").is_ok());
        assert!(validate_schedule("30 minutes").is_ok());
    }

    #[test]
    fn validate_schedule_rejects_an_empty_schedule() {
        // Arrange / Act
        let error = validate_schedule("   ").expect_err("a blank schedule must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_schedule_rejects_an_oversized_schedule() {
        // Arrange: one byte over the bound.
        let schedule = "*".repeat(MAX_SCHEDULE_LEN + 1);

        // Act
        let error =
            validate_schedule(&schedule).expect_err("an oversized schedule must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn missing_pg_cron_error_is_invalid_parameter_and_names_the_dependency() {
        // Arrange / Act
        let error = missing_pg_cron_error();

        // Assert
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains("pg_cron"));
    }
}
