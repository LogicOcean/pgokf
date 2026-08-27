//! Bundle-administration seam (`bundle_info`, `unregister_bundle`,
//! `list_bundles`).
//!
//! # Seam contract for the admin feature wave
//!
//! This module is intentionally empty. The wave that fills it should add the
//! SQL-facing administration API inside its own
//! `#[pgrx::pg_schema] mod pgokf { ... }` block, without touching the sync
//! engine:
//!
//! - define the `pgokf.bundle_info` composite type
//!   `(id, path, name, okf_version, file_count, last_synced_at, enabled)` in
//!   its own `extension_sql!` block with `requires = ["catalog_tables"]` —
//!   the core deliberately does not create it because neither sync nor
//!   search needs it;
//! - `unregister_bundle(bundle_id)` must authorize with
//!   [`crate::security::Operation::Register`]-level policy (admin), take the
//!   bundle advisory lock via
//!   [`crate::catalog::sync::advisory_lock_key`] on the stored path, and
//!   delete the `pgokf.bundles` row — concept/metadata (and feature) rows
//!   cascade;
//! - `list_bundles()` / `bundle_info(bundle_id)` are reader-level
//!   ([`crate::security::Operation::Search`]) projections over
//!   `pgokf.bundles`;
//! - populate `pgokf.bundles.okf_version` here (or in the config wave) once
//!   the bundle-level `index.md` metadata is surfaced; the core sync engine
//!   leaves it `NULL`.
