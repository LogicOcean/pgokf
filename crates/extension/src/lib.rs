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
/// [`postgresql_conf_options`] contributes extra `postgresql.conf` lines (none
/// are needed; the defaults exercise the stable SQL surface).
#[cfg(test)]
pub mod pg_test {
    /// Per-suite setup hook; the suite needs no global preparation.
    pub fn setup(_options: Vec<&str>) {}

    /// Extra `postgresql.conf` options for the test instance; none required.
    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        Vec::new()
    }
}
