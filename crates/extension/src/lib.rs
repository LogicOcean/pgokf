//! `pgokf` — a `PostgreSQL` extension that materializes Open Knowledge Format
//! bundles into a queryable catalog.
//!
//! This crate is the `PostgreSQL`-facing shell: it registers configuration
//! variables ([`guc`]), installs the bootstrap schema/role hardening
//! (`sql/bootstrap.sql`), exposes the security ([`security`]) and error
//! ([`errors`]) foundations, and hosts the catalog backbone ([`catalog`]) —
//! the base tables plus `pgokf.register_bundle`, `pgokf.refresh_bundle`, and
//! `pgokf.concept_search`. Feature modules (links, provenance, config, admin)
//! attach through the seams declared in [`catalog`].

pub mod catalog;
pub mod errors;
pub mod guc;
pub mod security;

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
