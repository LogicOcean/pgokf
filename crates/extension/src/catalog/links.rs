//! Link-graph projection seam (OKF v0.2 `pgokf.links`).
//!
//! # Seam contract for the links feature wave
//!
//! Implement everything in **this file only** — the sync engine already
//! calls [`project`] and must not be edited. The wave that fills this module
//! should:
//!
//! 1. Add a `pgokf.links` table in its own `extension_sql!` block with
//!    `requires = ["catalog_tables"]`, keyed by
//!    `(bundle_id, source_id) REFERENCES pgokf.concepts (bundle_id, id)
//!    ON DELETE CASCADE` so removed concepts drop their edges automatically
//!    (which is why removals need no seam call).
//! 2. In [`project`], for each [`StagedConcept`] delete the concept's
//!    existing outgoing edges and re-insert from
//!    [`okf_parser::ParsedConcept::links`]. Each [`okf_parser::Link`]
//!    already carries the raw `target`, `label`, `kind`, `ordinal`,
//!    `is_external`, and the normalized `target_path` / `target_id` for
//!    internal destinations — no re-parsing is required. Unresolved internal
//!    links are retained (OKF permits broken links; later syncs may resolve
//!    them).
//! 3. Use parameterized SPI only, and surface failures as
//!    [`CatalogError`] so the surrounding sync transaction rolls back
//!    atomically.
//!
//! Traversal APIs (recursive neighbors) belong in
//! [`crate::catalog::neighbors`], not here.

use crate::catalog::types::StagedConcept;
use crate::errors::CatalogError;

/// Project the outgoing links of every staged concept.
///
/// Currently a documented no-op: the base wave persists no link rows, and
/// this function exists so the sync engine's projection order is fixed
/// before the links wave lands. It is invoked inside the sync transaction
/// after concept rows (and their metadata) are written and before the bundle
/// row is finalized.
///
/// # Errors
///
/// Never fails today; the links wave will return [`CatalogError`] on SPI
/// failures so a partial projection aborts the sync transaction.
pub fn project(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    let _ = (bundle_id, staged);
    Ok(())
}
