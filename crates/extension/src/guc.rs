// SPDX-License-Identifier: AGPL-3.0-only
//! Configuration variables (GUCs) exposed by the `pgokf` extension.
//!
//! The four resource ceilings are registered with the `Sighup` context: they
//! can only be set in `postgresql.conf` (applied at server start or on a
//! configuration reload), and no session — not even a superuser's `SET` —
//! can raise them, which keeps them trustworthy as hard safety limits. The
//! `Postmaster` context would be stricter still, but `PostgreSQL` refuses to
//! define custom `PGC_POSTMASTER` variables after startup ("cannot create
//! `PGC_POSTMASTER` variables after startup", a FATAL error) unless the
//! library is preloaded via `shared_preload_libraries`; `pgokf` is loaded on
//! demand by `CREATE EXTENSION`, so `Sighup` is the strictest context that
//! keeps that flow working. The logging threshold uses the `Suset` context
//! so a superuser can adjust it at runtime.
//!
//! The typed accessor functions in this module are the public configuration
//! API for the rest of the extension; no other code should read the raw
//! settings directly.

use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use std::ffi::CString;

/// Default ceiling for the size of one OKF Markdown file, in bytes (4 MiB).
pub const DEFAULT_MAX_FILE_BYTES: i32 = 4 * 1024 * 1024;
/// Default ceiling for the number of files discovered in one bundle.
pub const DEFAULT_MAX_BUNDLE_FILES: i32 = 100_000;
/// Default ceiling for the size of YAML frontmatter, in bytes (256 KiB).
pub const DEFAULT_MAX_FRONTMATTER_BYTES: i32 = 256 * 1024;
/// Default ceiling for graph traversal depth.
pub const DEFAULT_MAX_GRAPH_HOPS: i32 = 5;
/// Default logging threshold used when `pgokf.log_level` is unset.
pub const DEFAULT_LOG_LEVEL: &str = "warning";

/// Upper bound accepted for `pgokf.max_graph_hops`.
const MAX_GRAPH_HOPS_CEILING: i32 = 1_000;

static MAX_FILE_BYTES: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_MAX_FILE_BYTES);
static MAX_BUNDLE_FILES: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_MAX_BUNDLE_FILES);
static MAX_FRONTMATTER_BYTES: GucSetting<i32> =
    GucSetting::<i32>::new(DEFAULT_MAX_FRONTMATTER_BYTES);
static MAX_GRAPH_HOPS: GucSetting<i32> = GucSetting::<i32>::new(DEFAULT_MAX_GRAPH_HOPS);
static LOG_LEVEL: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c"warning"));

/// The per-session multi-tenant context. An empty string (the default) means the
/// session declares no tenant and therefore sees every row — exactly the
/// pre-multi-tenancy behavior — while a non-empty value scopes the session to
/// that tenant through the row-level-security policies on the projection tables.
static TENANT: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c""));

/// Register every `pgokf.*` configuration variable with `PostgreSQL`.
///
/// Must be called exactly once per shared-library load, from `_PG_init`.
/// Resource ceilings are `PGC_SIGHUP`: they change only via
/// `postgresql.conf` plus a configuration reload, never from SQL (see the
/// module docs for why `PGC_POSTMASTER` is not usable here). The logging
/// threshold is `PGC_SUSET`, so only a superuser can alter it at runtime.
pub fn register_gucs() {
    GucRegistry::define_int_guc(
        c"pgokf.max_file_bytes",
        c"Maximum bytes read from one bundle file.",
        c"Hard safety limit for an individual OKF bundle file.",
        &MAX_FILE_BYTES,
        1,
        i32::MAX,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pgokf.max_bundle_files",
        c"Maximum files accepted in one bundle.",
        c"Hard safety limit for files discovered while indexing an OKF bundle.",
        &MAX_BUNDLE_FILES,
        1,
        i32::MAX,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pgokf.max_frontmatter_bytes",
        c"Maximum bytes parsed as frontmatter.",
        c"Hard safety limit for frontmatter parsed from one OKF document.",
        &MAX_FRONTMATTER_BYTES,
        1,
        i32::MAX,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pgokf.max_graph_hops",
        c"Maximum graph traversal depth.",
        c"Hard safety limit for graph hops evaluated by pgokf queries.",
        &MAX_GRAPH_HOPS,
        1,
        MAX_GRAPH_HOPS_CEILING,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pgokf.log_level",
        c"pgokf logging threshold.",
        c"Logging threshold used by pgokf; defaults to warning.",
        &LOG_LEVEL,
        GucContext::Suset,
        GucFlags::default(),
    );
    // The multi-tenant session context. `PGC_USERSET`, so any session may set it
    // (`SET pgokf.tenant = 'acme'`), and it can be pinned per role or connection
    // with `ALTER ROLE r SET pgokf.tenant = ...` or a connection default. It is a
    // policy selector, not a safety ceiling, so `Userset` is correct here. An
    // empty default preserves the pre-multi-tenancy see-all behavior for any
    // session that never sets it.
    GucRegistry::define_string_guc(
        c"pgokf.tenant",
        c"Active pgokf tenant for row-level isolation.",
        c"Per-session tenant selector: empty (the default) sees every row \
          (backward compatible), a non-empty value scopes reads and stamps writes \
          to that tenant via the projection tables' row-level-security policies.",
        &TENANT,
        GucContext::Userset,
        GucFlags::default(),
    );
}

/// Convert a GUC integer to `usize` without sign-loss surprises.
///
/// Every integer GUC in this module is registered with a minimum of 1, so a
/// negative value is unreachable; it is clamped to zero defensively rather
/// than panicking inside a `PostgreSQL` backend.
fn to_limit(value: i32) -> usize {
    usize::try_from(value).unwrap_or(0)
}

/// Effective `pgokf.max_file_bytes`: ceiling for one bundle file, in bytes.
#[must_use]
pub fn max_file_bytes() -> usize {
    to_limit(MAX_FILE_BYTES.get())
}

/// Effective `pgokf.max_bundle_files`: ceiling for files in one bundle.
#[must_use]
pub fn max_bundle_files() -> usize {
    to_limit(MAX_BUNDLE_FILES.get())
}

/// Effective `pgokf.max_frontmatter_bytes`: ceiling for YAML frontmatter, in
/// bytes.
#[must_use]
pub fn max_frontmatter_bytes() -> usize {
    to_limit(MAX_FRONTMATTER_BYTES.get())
}

/// Effective `pgokf.max_graph_hops`: ceiling for graph traversal depth.
#[must_use]
pub fn max_graph_hops() -> usize {
    to_limit(MAX_GRAPH_HOPS.get())
}

/// Effective `pgokf.log_level` as UTF-8, falling back to
/// [`DEFAULT_LOG_LEVEL`] when the setting is unset.
#[must_use]
pub fn log_level() -> String {
    LOG_LEVEL.get().map_or_else(
        || DEFAULT_LOG_LEVEL.to_owned(),
        |value| value.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_limit_defaults_match_the_extension_contract() {
        // Arrange: defaults are compile-time constants.
        // Act: read the constants directly.
        // Assert: they match the documented extension contract.
        assert_eq!(DEFAULT_MAX_FILE_BYTES, 4 * 1024 * 1024);
        assert_eq!(DEFAULT_MAX_BUNDLE_FILES, 100_000);
        assert_eq!(DEFAULT_MAX_FRONTMATTER_BYTES, 256 * 1024);
        assert_eq!(DEFAULT_MAX_GRAPH_HOPS, 5);
        assert_eq!(DEFAULT_LOG_LEVEL, "warning");
    }

    #[test]
    fn to_limit_clamps_negative_values_to_zero() {
        // Arrange
        let unreachable_negative = -1;

        // Act
        let limit = to_limit(unreachable_negative);

        // Assert
        assert_eq!(limit, 0);
    }
}
