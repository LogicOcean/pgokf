// SPDX-License-Identifier: AGPL-3.0-only
//! `pgokf` - a `PostgreSQL` extension that materializes Open Knowledge Format
//! bundles into a queryable catalog.
//!
//! This crate is the `PostgreSQL`-facing shell: it registers configuration
//! variables ([`guc`]), installs the bootstrap schema/role hardening
//! (`sql/bootstrap.sql`), exposes the security ([`security`]) and error
//! ([`errors`]) foundations, and hosts the catalog backbone ([`catalog`]) -
//! the base tables plus `pgokf.register_bundle`, `pgokf.refresh_bundle`, and
//! `pgokf.concept_search`. Feature modules (links, provenance, config, admin)
//! attach through the seams declared in [`catalog`].

pub mod catalog;
pub mod errors;
pub mod guc;
pub mod security;

// Gated on the `pg_test` feature alone (not `test`) so a plain
// `cargo test -p pgokf` stays green: `#[pg_test]`s require a managed cluster
// and only run under `cargo pgrx test`, which enables this feature.
#[cfg(feature = "pg_test")]
mod pg_tests;

use pgrx::{pg_guard, pg_schema};

pgrx::pg_module_magic!();
pgrx::extension_sql_file!("../sql/bootstrap.sql", name = "bootstrap", bootstrap);

/// `PostgreSQL` entry point invoked when the shared library is loaded.
///
/// Registers every `pgokf.*` configuration variable exactly once per backend
/// load, before any SQL-callable function can run.
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    guc::register_gucs();
}

/// SQL-facing namespace of the extension.
///
/// Declaring the schema as a `#[pg_schema]` module registers it in pgrx's
/// SQL entity graph, so `cargo pgrx schema`/`install`/`package` can resolve
/// every function placed here. pgrx emits `CREATE SCHEMA IF NOT EXISTS
/// pgokf;`, which is a no-op after `sql/bootstrap.sql` (installed first via
/// the `bootstrap` marker) has already created and hardened the schema.
#[pg_schema]
mod pgokf {
    use pgrx::pg_extern;

    /// Report the version of the loaded `pgokf` shared library.
    ///
    /// Useful for confirming that the installed SQL extension and the loaded
    /// module agree after an upgrade.
    #[pg_extern(immutable, parallel_safe)]
    fn version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
}

/// Control module required by the pgrx test framework.
///
/// `cargo pgrx test` compiles the crate with the `pg_test` feature, starts a
/// throwaway `PostgreSQL` instance, installs the extension, and then calls
/// `pgrx_tests::run_test` for every `#[pg_test]` in [`pg_tests`]. Each test
/// consults these two hooks: [`setup`] runs once before the suite (nothing to
/// prepare here - every `#[pg_test]` builds its own fixtures), and
/// [`postgresql_conf_options`] contributes extra `postgresql.conf` lines.
///
/// The defaults exercise the stable SQL surface with no optional extension.
/// The tests that cover an optional *preloaded* provider (the BM25 backends
/// `pg_textsearch` and `pg_search`) skip themselves unless that provider is
/// both installed in the cluster's library directory and named in
/// `shared_preload_libraries`; set `PGOKF_TEST_PRELOAD` to a comma-separated
/// list of libraries (for example `pg_textsearch,pg_search`) to preload them
/// into the throwaway instance and turn those tests on - and with it, a
/// provider that then turns out to be unusable fails the test instead of
/// skipping, so a misconfigured local run cannot pass vacuously.
/// Test-only support shared by the `cargo pgrx test` harness hooks (compiled
/// under `cfg(test)` into the test binary) and the `#[pg_test]` bodies
/// (compiled under the `pg_test` feature into the library the throwaway
/// instance loads). Both sides read the same environment variable, so the
/// preload the harness applies and the strictness the tests enforce agree.
#[cfg(any(test, feature = "pg_test"))]
pub mod test_support {
    /// Environment variable naming the libraries to preload into the test
    /// instance (comma-separated); unset or empty preloads nothing extra.
    pub const PRELOAD_ENV: &str = "PGOKF_TEST_PRELOAD";

    /// The validated preload list from [`PRELOAD_ENV`], or `None` when unset
    /// or empty. Only bare library names are accepted (letters, digits,
    /// underscores, separated by commas), so the value cannot break out of
    /// the quoted `postgresql.conf` setting.
    ///
    /// # Panics
    ///
    /// When an entry is not a bare library name: a malformed value is a
    /// setup error in the invoking shell, and failing the run is the only
    /// honest response.
    #[must_use]
    pub fn requested_preload() -> Option<String> {
        let raw = std::env::var(PRELOAD_ENV).ok()?;
        let names: Vec<&str> = raw
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        if names.is_empty() {
            return None;
        }
        for name in &names {
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{PRELOAD_ENV} entry {name:?} is not a bare library name",
            );
        }
        Some(names.join(","))
    }

    /// Whether [`PRELOAD_ENV`] names `library`.
    #[must_use]
    pub fn preload_requested(library: &str) -> bool {
        requested_preload().is_some_and(|list| list.split(',').any(|entry| entry == library))
    }
}

#[cfg(test)]
pub mod pg_test {
    /// Per-suite setup hook; the suite needs no global preparation.
    pub fn setup(_options: Vec<&str>) {}

    /// Extra `postgresql.conf` options for the test instance: a
    /// `shared_preload_libraries` line when `PGOKF_TEST_PRELOAD` asks for one
    /// (see [`crate::test_support`]).
    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        match crate::test_support::requested_preload() {
            Some(libraries) => {
                // pgrx wants `'static` lines; the list lives for the whole
                // test process, so leaking this one string is the intended
                // way to hand it over.
                let line = format!("shared_preload_libraries = '{libraries}'");
                vec![Box::leak(line.into_boxed_str())]
            }
            None => Vec::new(),
        }
    }
}
