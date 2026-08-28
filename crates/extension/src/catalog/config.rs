//! Durable catalog configuration surface (`allowed_roots` and friends).
//!
//! This module owns the cluster-persistent policy that governs bundle
//! registration and sync behavior. It is deliberately separate from the
//! per-session resource ceilings in [`crate::guc`]: the GUCs are hard safety
//! limits set only in `postgresql.conf`, while the values here are catalog
//! policy an administrator manages through SQL and that survives restarts.
//!
//! # Storage
//!
//! Configuration lives in a single row of `pgokf_private.config` (a schema
//! bootstrap already created and hardened to administrators only). The table
//! is modeled with one typed column per setting and a boolean singleton
//! primary key so exactly one policy row can ever exist. No role is granted
//! direct DML on the table; every read and write flows through the
//! `SECURITY DEFINER` functions below, which authorize the caller first.
//!
//! # Admin API
//!
//! `pgokf.set_config(key, value)` and `pgokf.reset_config(key)` are the
//! admin-only mutators. The setter is intentionally `jsonb`-in rather than a
//! family of typed overloads: a single polymorphic entry point keeps the SQL
//! surface small while still letting each key carry its natural shape — a
//! `jsonb` array of strings for `allowed_roots`/`default_exclude`, a boolean
//! for `default_strict`/`store_source`, an integer for
//! `sync_log_retention_days`, and a string for `default_text_search_config`
//! and `search_backend`. Every value is validated and
//! coerced per key ([`coerce`]); an unknown key or a value of the wrong shape
//! or domain is rejected with SQLSTATE `22023`. `pgokf.get_config()` is a
//! reader-level projection returning the effective policy as `jsonb`.
//!
//! # Enforcement seam
//!
//! [`allowed_roots`] reads the configured roots via SPI, and
//! [`enforce_allowed_roots`] is the seam the sync engine calls during bundle
//! registration: when roots are configured, a candidate path must resolve
//! inside one of them (symlink-escape-safe containment via
//! [`crate::security::canonicalize_contained_path`]); when none are
//! configured the interim policy documented in [`crate::catalog::sync`]
//! applies unchanged.

use std::path::{Path, PathBuf};

use pgrx::Spi;

use crate::errors::CatalogError;
use crate::security;

/// The durable configuration keys settable through `pgokf.set_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigKey {
    /// Absolute directory roots that bundle paths must resolve inside.
    AllowedRoots,
    /// Default text-search configuration used when weighting concept bodies.
    DefaultTextSearchConfig,
    /// Whether sync rejects malformed files instead of skipping them.
    DefaultStrict,
    /// Retention window, in days, for sync-log history.
    SyncLogRetentionDays,
    /// Default bundle-relative glob patterns excluded from discovery.
    DefaultExclude,
    /// Whether sync stores each concept's verbatim source bytes in Postgres.
    StoreSource,
    /// Ranked-search execution backend (`native` FTS or optional `bm25`).
    SearchBackend,
}

impl ConfigKey {
    /// The canonical wire name of the key, as accepted by `set_config`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::AllowedRoots => "allowed_roots",
            Self::DefaultTextSearchConfig => "default_text_search_config",
            Self::DefaultStrict => "default_strict",
            Self::SyncLogRetentionDays => "sync_log_retention_days",
            Self::DefaultExclude => "default_exclude",
            Self::StoreSource => "store_source",
            Self::SearchBackend => "search_backend",
        }
    }

    /// Parse a caller-supplied key, rejecting unknown keys with SQLSTATE
    /// `22023`.
    fn parse(key: &str) -> Result<Self, CatalogError> {
        match key {
            "allowed_roots" => Ok(Self::AllowedRoots),
            "default_text_search_config" => Ok(Self::DefaultTextSearchConfig),
            "default_strict" => Ok(Self::DefaultStrict),
            "sync_log_retention_days" => Ok(Self::SyncLogRetentionDays),
            "default_exclude" => Ok(Self::DefaultExclude),
            "store_source" => Ok(Self::StoreSource),
            "search_backend" => Ok(Self::SearchBackend),
            other => Err(CatalogError::invalid_parameter(
                format!("unknown configuration key: {other}"),
                Path::new(""),
            )),
        }
    }
}

/// A per-key value that has passed validation and is ready to persist.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigValue {
    AllowedRoots(Vec<String>),
    DefaultTextSearchConfig(String),
    DefaultStrict(bool),
    SyncLogRetentionDays(i32),
    DefaultExclude(Vec<String>),
    StoreSource(bool),
    SearchBackend(String),
}

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Build the `22023` error raised when a value has the wrong JSON shape.
fn type_error(key: ConfigKey, expected: &str) -> CatalogError {
    CatalogError::invalid_parameter(
        format!("configuration key {} expects {expected}", key.as_str()),
        Path::new(""),
    )
}

/// Validate `allowed_roots` entries: each must be an absolute, traversal-free
/// path (reusing the shared path-syntax policy).
fn validate_allowed_roots(roots: &[String]) -> Result<(), CatalogError> {
    for root in roots {
        security::validate_path_syntax(Path::new(root), Path::new(""))?;
    }
    Ok(())
}

/// Validate `default_exclude` glob patterns: non-empty and NUL-free.
fn validate_exclude_patterns(patterns: &[String]) -> Result<(), CatalogError> {
    for pattern in patterns {
        if pattern.is_empty() {
            return Err(CatalogError::invalid_parameter(
                "default_exclude patterns must not be empty",
                Path::new(""),
            ));
        }
        if pattern.contains('\0') {
            return Err(CatalogError::invalid_parameter(
                "default_exclude patterns must not contain NUL bytes",
                Path::new(""),
            ));
        }
    }
    Ok(())
}

/// Validate `sync_log_retention_days`: non-negative and representable as the
/// `integer` column that stores it.
fn validate_retention_days(days: i64) -> Result<i32, CatalogError> {
    if days < 0 {
        return Err(CatalogError::invalid_parameter(
            "sync_log_retention_days must be greater than or equal to 0",
            Path::new(""),
        ));
    }
    i32::try_from(days).map_err(|_| {
        CatalogError::invalid_parameter(
            format!("sync_log_retention_days is out of range: {days}"),
            Path::new(""),
        )
    })
}

/// Validate the structural shape of `default_text_search_config`. Existence of
/// the configuration is checked separately against `pg_catalog.pg_ts_config`.
fn validate_text_search_config_name(name: &str) -> Result<(), CatalogError> {
    if name.trim().is_empty() {
        return Err(CatalogError::invalid_parameter(
            "default_text_search_config must not be empty",
            Path::new(""),
        ));
    }
    Ok(())
}

/// Validate `search_backend`: it must name one of the supported ranked-search
/// backends (`native` or `bm25`), matched by the shared registry in
/// [`crate::catalog::search_backend`] so the accepted set never drifts from the
/// backends the dispatcher can actually construct.
fn validate_search_backend(name: &str) -> Result<(), CatalogError> {
    if crate::catalog::search_backend::is_supported(name) {
        Ok(())
    } else {
        Err(CatalogError::invalid_parameter(
            format!(
                "search_backend must be one of {}, got {name}",
                crate::catalog::search_backend::supported_display()
            ),
            Path::new(""),
        ))
    }
}

/// Coerce and validate a `jsonb` value for `key` into a typed [`ConfigValue`].
///
/// Consumes the wrapper so no argument is passed by value only to be borrowed,
/// and never names `serde_json` directly — shape inspection goes through the
/// inherent accessors on the wrapped value.
fn coerce(key: ConfigKey, value: pgrx::JsonB) -> Result<ConfigValue, CatalogError> {
    let json = value.0;
    let string_array = |array_key: ConfigKey| -> Result<Vec<String>, CatalogError> {
        let array = json
            .as_array()
            .ok_or_else(|| type_error(array_key, "an array of strings"))?;
        let mut out = Vec::with_capacity(array.len());
        for element in array {
            let text = element
                .as_str()
                .ok_or_else(|| type_error(array_key, "an array of strings"))?;
            out.push(text.to_owned());
        }
        Ok(out)
    };

    match key {
        ConfigKey::AllowedRoots => {
            let roots = string_array(key)?;
            validate_allowed_roots(&roots)?;
            Ok(ConfigValue::AllowedRoots(roots))
        }
        ConfigKey::DefaultExclude => {
            let patterns = string_array(key)?;
            validate_exclude_patterns(&patterns)?;
            Ok(ConfigValue::DefaultExclude(patterns))
        }
        ConfigKey::DefaultStrict => {
            let flag = json.as_bool().ok_or_else(|| type_error(key, "a boolean"))?;
            Ok(ConfigValue::DefaultStrict(flag))
        }
        ConfigKey::StoreSource => {
            let flag = json.as_bool().ok_or_else(|| type_error(key, "a boolean"))?;
            Ok(ConfigValue::StoreSource(flag))
        }
        ConfigKey::SyncLogRetentionDays => {
            let raw = json.as_i64().ok_or_else(|| type_error(key, "an integer"))?;
            Ok(ConfigValue::SyncLogRetentionDays(validate_retention_days(
                raw,
            )?))
        }
        ConfigKey::DefaultTextSearchConfig => {
            let name = json.as_str().ok_or_else(|| type_error(key, "a string"))?;
            validate_text_search_config_name(name)?;
            Ok(ConfigValue::DefaultTextSearchConfig(name.to_owned()))
        }
        ConfigKey::SearchBackend => {
            let name = json.as_str().ok_or_else(|| type_error(key, "a string"))?;
            validate_search_backend(name)?;
            Ok(ConfigValue::SearchBackend(name.to_owned()))
        }
    }
}

/// Confirm that `name` names an installed text-search configuration.
///
/// Resolution mirrors identifier lookup without ever raising a `PostgreSQL`
/// error: a name containing a `.` is matched as `schema.config` against
/// `pg_catalog.pg_ts_config`, and an unqualified name is matched by
/// visibility on the effective `search_path`. Unquoted identifiers fold to
/// lower case, which the comparison replicates. A missing configuration
/// returns cleanly as an invalid parameter (`22023`) rather than escaping as a
/// raw catalog-cast error via `longjmp` — the earlier `to_regconfig` approach
/// referenced a function `PostgreSQL` does not provide.
fn ensure_text_search_config_exists(name: &str) -> Result<(), CatalogError> {
    const EXISTS_QUERY: &str = "\
        SELECT EXISTS (
            SELECT 1
            FROM pg_catalog.pg_ts_config c
            JOIN pg_catalog.pg_namespace n ON n.oid = c.cfgnamespace
            WHERE CASE
                WHEN pg_catalog.strpos($1, '.') > 0 THEN
                    pg_catalog.lower(n.nspname)
                        = pg_catalog.lower(pg_catalog.split_part($1, '.', 1))
                    AND pg_catalog.lower(c.cfgname)
                        = pg_catalog.lower(pg_catalog.split_part($1, '.', 2))
                ELSE
                    pg_catalog.lower(c.cfgname) = pg_catalog.lower($1)
                    AND pg_catalog.pg_ts_config_is_visible(c.oid)
            END
        )";

    let exists = Spi::get_one_with_args::<bool>(EXISTS_QUERY, &[name.into()])
        .map_err(|error| spi_error("failed to verify text search configuration", &error))?
        .unwrap_or(false);

    if exists {
        Ok(())
    } else {
        Err(CatalogError::invalid_parameter(
            format!("text search configuration does not exist: {name}"),
            Path::new(""),
        ))
    }
}

/// Persist a validated value into its column of the singleton config row.
///
/// The column identifier is fixed per [`ConfigValue`] variant (never derived
/// from caller input); only the value is bound as a parameter.
fn persist(value: &ConfigValue) -> Result<(), CatalogError> {
    match value {
        ConfigValue::AllowedRoots(roots) => Spi::run_with_args(
            "UPDATE pgokf_private.config SET allowed_roots = $1 WHERE singleton",
            &[roots.clone().into()],
        ),
        ConfigValue::DefaultExclude(patterns) => Spi::run_with_args(
            "UPDATE pgokf_private.config SET default_exclude = $1 WHERE singleton",
            &[patterns.clone().into()],
        ),
        ConfigValue::DefaultStrict(flag) => Spi::run_with_args(
            "UPDATE pgokf_private.config SET default_strict = $1 WHERE singleton",
            &[(*flag).into()],
        ),
        ConfigValue::SyncLogRetentionDays(days) => Spi::run_with_args(
            "UPDATE pgokf_private.config SET sync_log_retention_days = $1 WHERE singleton",
            &[(*days).into()],
        ),
        ConfigValue::DefaultTextSearchConfig(name) => Spi::run_with_args(
            "UPDATE pgokf_private.config SET default_text_search_config = $1 WHERE singleton",
            &[name.clone().into()],
        ),
        ConfigValue::StoreSource(flag) => Spi::run_with_args(
            "UPDATE pgokf_private.config SET store_source = $1 WHERE singleton",
            &[(*flag).into()],
        ),
        ConfigValue::SearchBackend(name) => Spi::run_with_args(
            "UPDATE pgokf_private.config SET search_backend = $1 WHERE singleton",
            &[name.clone().into()],
        ),
    }
    .map_err(|error| spi_error("failed to persist configuration", &error))
}

/// Reset a single key to its column default.
fn reset_key(key: ConfigKey) -> Result<(), CatalogError> {
    let statement = match key {
        ConfigKey::AllowedRoots => {
            "UPDATE pgokf_private.config SET allowed_roots = DEFAULT WHERE singleton"
        }
        ConfigKey::DefaultTextSearchConfig => {
            "UPDATE pgokf_private.config SET default_text_search_config = DEFAULT WHERE singleton"
        }
        ConfigKey::DefaultStrict => {
            "UPDATE pgokf_private.config SET default_strict = DEFAULT WHERE singleton"
        }
        ConfigKey::SyncLogRetentionDays => {
            "UPDATE pgokf_private.config SET sync_log_retention_days = DEFAULT WHERE singleton"
        }
        ConfigKey::DefaultExclude => {
            "UPDATE pgokf_private.config SET default_exclude = DEFAULT WHERE singleton"
        }
        ConfigKey::StoreSource => {
            "UPDATE pgokf_private.config SET store_source = DEFAULT WHERE singleton"
        }
        ConfigKey::SearchBackend => {
            "UPDATE pgokf_private.config SET search_backend = DEFAULT WHERE singleton"
        }
    };
    Spi::run(statement).map_err(|error| spi_error("failed to reset configuration key", &error))
}

/// Reset every key to its column default.
fn reset_all() -> Result<(), CatalogError> {
    Spi::run(
        "UPDATE pgokf_private.config SET \
             allowed_roots = DEFAULT, \
             default_text_search_config = DEFAULT, \
             default_strict = DEFAULT, \
             sync_log_retention_days = DEFAULT, \
             default_exclude = DEFAULT, \
             store_source = DEFAULT, \
             search_backend = DEFAULT \
         WHERE singleton",
    )
    .map_err(|error| spi_error("failed to reset configuration", &error))
}

fn set_config_impl(key: &str, value: pgrx::JsonB) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    let parsed = ConfigKey::parse(key)?;
    let coerced = coerce(parsed, value)?;
    if let ConfigValue::DefaultTextSearchConfig(name) = &coerced {
        ensure_text_search_config_exists(name)?;
    }
    persist(&coerced)
}

fn reset_config_impl(key: Option<String>) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    match key {
        None => reset_all(),
        Some(key) => reset_key(ConfigKey::parse(&key)?),
    }
}

fn get_config_impl() -> Result<pgrx::JsonB, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    Spi::get_one::<pgrx::JsonB>(
        "SELECT pg_catalog.jsonb_build_object(
             'allowed_roots', pg_catalog.to_jsonb(allowed_roots),
             'default_text_search_config', pg_catalog.to_jsonb(default_text_search_config),
             'default_strict', pg_catalog.to_jsonb(default_strict),
             'sync_log_retention_days', pg_catalog.to_jsonb(sync_log_retention_days),
             'default_exclude', pg_catalog.to_jsonb(default_exclude),
             'store_source', pg_catalog.to_jsonb(store_source),
             'search_backend', pg_catalog.to_jsonb(search_backend))
         FROM pgokf_private.config
         WHERE singleton",
    )
    .map_err(|error| spi_error("failed to read configuration", &error))?
    .ok_or_else(|| CatalogError::internal("configuration row is missing", Path::new("")))
}

/// The configured allowed bundle roots, as absolute paths.
///
/// Reads the singleton `pgokf_private.config` row via SPI. An empty result
/// means no roots are configured and the interim registration policy applies.
///
/// # Errors
///
/// Returns a [`CatalogError`] when the configuration row cannot be read.
pub fn allowed_roots() -> Result<Vec<PathBuf>, CatalogError> {
    let roots = Spi::get_one::<Vec<String>>(
        "SELECT allowed_roots FROM pgokf_private.config WHERE singleton",
    )
    .map_err(|error| spi_error("failed to read allowed_roots configuration", &error))?
    .unwrap_or_default();
    Ok(roots.into_iter().map(PathBuf::from).collect())
}

/// Enforce configured allowed roots for a candidate bundle path.
///
/// When no roots are configured this is a no-op and the interim policy in
/// [`crate::catalog::sync`] governs. When roots are configured, the path must
/// resolve inside one of them via
/// [`crate::security::canonicalize_contained_path`] (which resolves symlinks
/// on both sides so containment cannot be escaped); otherwise registration is
/// rejected with SQLSTATE `22023`.
///
/// # Errors
///
/// Returns a [`CatalogError`] when the configuration cannot be read, or when
/// the resolved path escapes every configured root.
pub fn enforce_allowed_roots(requested_path: &str) -> Result<(), CatalogError> {
    let roots = allowed_roots()?;
    if roots.is_empty() {
        return Ok(());
    }
    security::canonicalize_contained_path(Path::new(requested_path), &roots, Path::new(""))?;
    Ok(())
}

/// The durable, sync-time defaults consumed by the register/refresh engine.
///
/// Read once per sync from the singleton `pgokf_private.config` row through the
/// `SECURITY DEFINER` sync path, which holds privileges on the administrator-only
/// table. Reader-level callers cannot read the table directly and must instead
/// obtain the effective policy through the `pgokf.get_config` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncDefaults {
    /// Whether a malformed file aborts the sync (`true`) or is logged and
    /// skipped (`false`).
    pub strict: bool,
    /// Bundle-relative glob patterns excluded from discovery.
    pub exclude: Vec<String>,
    /// Text-search configuration used to build concept `tsvector`s at index
    /// time.
    pub text_search_config: String,
    /// Whether the sync persists each concept's verbatim source bytes into
    /// `pgokf.concept_source` (small self-contained tier) or leaves the source
    /// in its external store (enterprise data-lake tier, the default).
    pub store_source: bool,
}

/// Read the durable sync-time defaults from the singleton config row.
///
/// Combines `default_strict`, `default_exclude`, `default_text_search_config`,
/// and `store_source` into one round trip so the register/refresh engine can
/// honor every knob without an N+1 read. Intended for the `SECURITY DEFINER`
/// sync path only (it reads the admin-only config table).
///
/// # Errors
///
/// Returns a [`CatalogError`] when the configuration row cannot be read or is
/// missing.
pub fn sync_defaults() -> Result<SyncDefaults, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT default_strict, default_exclude, default_text_search_config, store_source
                 FROM pgokf_private.config
                 WHERE singleton",
                Some(1),
                &[],
            )
            .map_err(|error| spi_error("failed to read sync defaults", &error))?;
        let Some(row) = table.into_iter().next() else {
            return Err(CatalogError::internal(
                "configuration row is missing",
                Path::new(""),
            ));
        };
        let strict = row
            .get::<bool>(1)
            .map_err(|error| spi_error("failed to read default_strict", &error))?
            .ok_or_else(|| CatalogError::internal("default_strict is NULL", Path::new("")))?;
        let exclude = row
            .get::<Vec<String>>(2)
            .map_err(|error| spi_error("failed to read default_exclude", &error))?
            .unwrap_or_default();
        let text_search_config = row
            .get::<String>(3)
            .map_err(|error| spi_error("failed to read default_text_search_config", &error))?
            .ok_or_else(|| {
                CatalogError::internal("default_text_search_config is NULL", Path::new(""))
            })?;
        let store_source = row
            .get::<bool>(4)
            .map_err(|error| spi_error("failed to read store_source", &error))?
            .ok_or_else(|| CatalogError::internal("store_source is NULL", Path::new("")))?;
        Ok(SyncDefaults {
            strict,
            exclude,
            text_search_config,
            store_source,
        })
    })
}

// Durable configuration storage: one typed, singleton policy row in the
// administrator-only `pgokf_private` schema.
pgrx::extension_sql!(
    r"
CREATE TABLE pgokf_private.config (
    singleton                  boolean PRIMARY KEY DEFAULT true,
    allowed_roots              text[]  NOT NULL DEFAULT '{}',
    default_text_search_config text    NOT NULL DEFAULT 'pg_catalog.english',
    default_strict             boolean NOT NULL DEFAULT true,
    sync_log_retention_days    integer NOT NULL DEFAULT 30,
    default_exclude            text[]  NOT NULL DEFAULT '{}',
    store_source               boolean NOT NULL DEFAULT false,
    search_backend             text    NOT NULL DEFAULT 'native',
    CONSTRAINT config_singleton_chk CHECK (singleton),
    CONSTRAINT config_retention_nonneg_chk CHECK (sync_log_retention_days >= 0),
    CONSTRAINT config_search_backend_chk CHECK (search_backend IN ('native', 'bm25'))
);

INSERT INTO pgokf_private.config DEFAULT VALUES;

REVOKE ALL ON pgokf_private.config FROM PUBLIC;

COMMENT ON TABLE pgokf_private.config IS
    'Cluster-persistent catalog policy: a single row managed only through the pgokf.set_config / reset_config SECURITY DEFINER functions.';
COMMENT ON COLUMN pgokf_private.config.allowed_roots IS
    'Absolute directory roots that a registered bundle path must resolve inside; empty means the interim any-absolute-path policy applies.';
COMMENT ON COLUMN pgokf_private.config.default_text_search_config IS
    'Default text-search configuration used to build concept tsvectors at index time and to parse search queries; must name an installed configuration (verified against pg_catalog.pg_ts_config). A change takes effect for bundles synced or refreshed afterward; existing rows keep their tsvector until refresh_bundle re-indexes them.';
COMMENT ON COLUMN pgokf_private.config.default_strict IS
    'Whether sync rejects malformed files (true) instead of skipping them.';
COMMENT ON COLUMN pgokf_private.config.sync_log_retention_days IS
    'Retention window in days for sync-log history; must be >= 0.';
COMMENT ON COLUMN pgokf_private.config.default_exclude IS
    'Default bundle-relative glob patterns excluded from discovery.';
COMMENT ON COLUMN pgokf_private.config.store_source IS
    'Whether sync stores each concept''s verbatim source bytes in pgokf.concept_source (true = small self-contained tier: the original files live in Postgres) or leaves the source in its external object-store/data-lake (false, the default = enterprise tier: Postgres holds only metadata and search). Not retroactive: a change takes effect for bundles synced or refreshed afterward; existing rows keep their stored source (or absence of one) until refresh_bundle re-indexes them.';
COMMENT ON COLUMN pgokf_private.config.search_backend IS
    'Ranked-search execution backend for pgokf.concept_search: ''native'' (the default, zero-dependency PostgreSQL FTS available on every supported server) or ''bm25'' (Block-Max WAND top-k via the external ParadeDB pg_search extension). When set to ''bm25'' the search transparently falls back to native, with a warning, if pg_search is not installed or no bm25 index exists on pgokf.concepts (build one with pgokf.rebuild_search_index).';
",
    name = "config_table",
    requires = ["catalog_tables"]
);

/// SQL-facing configuration API, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::{default, extension_sql, pg_extern};

    use super::{get_config_impl, reset_config_impl, set_config_impl};

    /// Set a durable configuration key. Requires membership in `pgokf_admin`.
    ///
    /// The `value` is `jsonb` and is validated and coerced per key: an array
    /// of strings for `allowed_roots` / `default_exclude`, a boolean for
    /// `default_strict` / `store_source`, an integer for
    /// `sync_log_retention_days`, and a string for
    /// `default_text_search_config` (any installed configuration) and
    /// `search_backend` (`native` or `bm25`). Unknown keys and wrong-shaped or
    /// out-of-domain values raise SQLSTATE `22023`.
    #[pg_extern(requires = ["config_table"])]
    fn set_config(key: &str, value: pgrx::JsonB) {
        set_config_impl(key, value).unwrap_or_else(|error| error.raise());
    }

    /// Reset one configuration key to its default, or every key when `key`
    /// is `NULL`. Requires membership in `pgokf_admin`.
    #[pg_extern(requires = ["config_table"])]
    fn reset_config(key: default!(Option<String>, "NULL")) {
        reset_config_impl(key).unwrap_or_else(|error| error.raise());
    }

    /// Return the effective catalog configuration as a `jsonb` object.
    ///
    /// Reader-level: available to `pgokf_reader` and `pgokf_admin`.
    #[pg_extern(requires = ["config_table"])]
    fn get_config() -> pgrx::JsonB {
        get_config_impl().unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.set_config(text, jsonb)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.reset_config(text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.get_config()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.set_config(text, jsonb) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.reset_config(text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.get_config() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.set_config(text, jsonb) TO pgokf_admin;
GRANT EXECUTE ON FUNCTION pgokf.reset_config(text) TO pgokf_admin;
GRANT EXECUTE ON FUNCTION pgokf.get_config() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.set_config(text, jsonb) IS
    'Set a durable catalog configuration key from a validated, coerced jsonb value. Admin-only; raises 22023 on unknown keys or invalid values.';
COMMENT ON FUNCTION pgokf.reset_config(text) IS
    'Reset one configuration key to its default, or every key when the argument is NULL. Admin-only.';
COMMENT ON FUNCTION pgokf.get_config() IS
    'Return the effective catalog configuration as a jsonb object. Available to pgokf_reader and pgokf_admin.';
",
        name = "config_function_hardening",
        requires = [set_config, reset_config, get_config]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_unknown_key_as_invalid_parameter() {
        // Arrange
        let key = "totally_unknown";

        // Act
        let error = ConfigKey::parse(key).expect_err("unknown keys must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains("totally_unknown"));
    }

    #[test]
    fn parse_accepts_every_known_key() {
        // Arrange
        let expected = [
            ("allowed_roots", ConfigKey::AllowedRoots),
            (
                "default_text_search_config",
                ConfigKey::DefaultTextSearchConfig,
            ),
            ("default_strict", ConfigKey::DefaultStrict),
            ("sync_log_retention_days", ConfigKey::SyncLogRetentionDays),
            ("default_exclude", ConfigKey::DefaultExclude),
            ("store_source", ConfigKey::StoreSource),
            ("search_backend", ConfigKey::SearchBackend),
        ];

        for (name, key) in expected {
            // Act
            let parsed = ConfigKey::parse(name).expect("known key must parse");

            // Assert
            assert_eq!(parsed, key);
            assert_eq!(parsed.as_str(), name);
        }
    }

    #[test]
    fn validate_allowed_roots_accepts_absolute_paths() {
        // Arrange
        let roots = vec!["/srv/bundles".to_owned(), "/data/okf".to_owned()];

        // Act
        let result = validate_allowed_roots(&roots);

        // Assert
        assert!(result.is_ok());
    }

    #[test]
    fn validate_allowed_roots_rejects_relative_path() {
        // Arrange
        let roots = vec!["/srv/ok".to_owned(), "relative/dir".to_owned()];

        // Act
        let error = validate_allowed_roots(&roots).expect_err("relative roots must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_allowed_roots_rejects_parent_traversal() {
        // Arrange
        let roots = vec!["/srv/bundles/../secrets".to_owned()];

        // Act
        let error = validate_allowed_roots(&roots).expect_err("traversal must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_retention_days_accepts_zero() {
        // Arrange
        let days = 0;

        // Act
        let coerced = validate_retention_days(days).expect("zero retention is valid");

        // Assert
        assert_eq!(coerced, 0);
    }

    #[test]
    fn validate_retention_days_rejects_negative() {
        // Arrange
        let days = -1;

        // Act
        let error = validate_retention_days(days).expect_err("negative retention must fail");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_retention_days_rejects_out_of_range() {
        // Arrange
        let days = i64::from(i32::MAX) + 1;

        // Act
        let error = validate_retention_days(days).expect_err("out-of-range retention must fail");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_exclude_patterns_accepts_globs() {
        // Arrange
        let patterns = vec!["*.tmp".to_owned(), "drafts/**".to_owned()];

        // Act
        let result = validate_exclude_patterns(&patterns);

        // Assert
        assert!(result.is_ok());
    }

    #[test]
    fn validate_exclude_patterns_rejects_empty_pattern() {
        // Arrange
        let patterns = vec!["ok".to_owned(), String::new()];

        // Act
        let error =
            validate_exclude_patterns(&patterns).expect_err("empty patterns must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_text_search_config_name_rejects_blank() {
        // Arrange
        let name = "   ";

        // Act
        let error =
            validate_text_search_config_name(name).expect_err("blank config name must fail");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }

    #[test]
    fn validate_search_backend_accepts_supported_backends() {
        // Arrange & Act & Assert
        assert!(validate_search_backend("native").is_ok());
        assert!(validate_search_backend("bm25").is_ok());
    }

    #[test]
    fn validate_search_backend_rejects_unknown_backend() {
        // Arrange
        let name = "elasticsearch";

        // Act
        let error = validate_search_backend(name).expect_err("unknown backend must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains("elasticsearch"));
    }

    #[test]
    fn validate_text_search_config_name_accepts_qualified_name() {
        // Arrange
        let name = "pg_catalog.english";

        // Act
        let result = validate_text_search_config_name(name);

        // Assert
        assert!(result.is_ok());
    }
}
