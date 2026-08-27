//! Provenance/trust/lifecycle projection seam.
//!
//! # Seam contract for the provenance feature wave
//!
//! Implement everything in **this file only** — the sync engine already
//! calls [`project`] and must not be edited. The wave that fills this module
//! should:
//!
//! 1. Add its storage (for example a `pgokf.concept_provenance` table or
//!    dedicated columns/views) in its own `extension_sql!` block with
//!    `requires = ["catalog_tables"]`, cascading from
//!    `pgokf.concepts (bundle_id, id)` so removed concepts clean up without
//!    a seam call.
//! 2. In [`project`], extract provenance, trust, and lifecycle fields from
//!    each [`StagedConcept`]: OKF v0.2 keys the core parser does not model
//!    explicitly are preserved verbatim in
//!    [`okf_parser::ParsedConcept::metadata`] (and mirrored per-key into
//!    `pgokf.concept_metadata`), and the producer-declared frontmatter `id`
//!    is available as [`okf_parser::ParsedConcept::declared_id`] for
//!    duplicate-ID diagnostics.
//! 3. Use parameterized SPI only, and surface failures as
//!    [`CatalogError`] so the surrounding sync transaction rolls back
//!    atomically.

use crate::catalog::types::StagedConcept;
use crate::errors::CatalogError;

/// Project provenance/trust/lifecycle data for every staged concept.
///
/// Currently a documented no-op invoked inside the sync transaction, after
/// [`crate::catalog::links::project`] and before the bundle row is
/// finalized, so the projection order is fixed before the provenance wave
/// lands.
///
/// # Errors
///
/// Never fails today; the provenance wave will return [`CatalogError`] on
/// SPI failures so a partial projection aborts the sync transaction.
pub fn project(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    let _ = (bundle_id, staged);
    Ok(())
}
