//! Composite result construction and the projection-seam payload.
//!
//! The SQL definitions of `pgokf.bundle_sync_result` and
//! `pgokf.concept_search_result` live in the `catalog_tables` block
//! ([`crate::catalog::schema`]); this module owns their Rust-side builders
//! plus [`StagedConcept`], the payload handed to the projection seam
//! ([`crate::catalog::links::project`] /
//! [`crate::catalog::provenance::project`]) after the sync engine has staged
//! concept rows.

use crate::errors::CatalogError;
use okf_parser::ParsedConcept;
use okf_sync::SyncReport;
use pgrx::AllocatedByRust;
use pgrx::heap_tuple::PgHeapTuple;
use std::path::Path;

/// Qualified SQL name of the sync-result composite type.
pub const BUNDLE_SYNC_RESULT_TYPE: &str = "pgokf.bundle_sync_result";
/// Qualified SQL name of the search-result composite type.
pub const CONCEPT_SEARCH_RESULT_TYPE: &str = "pgokf.concept_search_result";

/// One parsed concept staged for (or just written to) the catalog, as handed
/// to the projection seam.
///
/// Feature modules receive the complete [`ParsedConcept`] — including its
/// extracted [`okf_parser::Link`]s and producer `metadata` — together with
/// the content identity the sync engine recorded, so they can project their
/// own tables without re-reading or re-parsing bundle files.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedConcept {
    /// The fully parsed concept (ID, path, frontmatter, links, metadata).
    pub concept: ParsedConcept,
    /// Lowercase hexadecimal BLAKE3 digest of the source file.
    pub file_hash: String,
    /// Filesystem modification time as seconds since the Unix epoch, when
    /// the filesystem reported one.
    pub modified_at_epoch: Option<f64>,
}

/// One ranked hit produced by `pgokf.concept_search`, prior to being packed
/// into the `pgokf.concept_search_result` composite.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Bundle the concept belongs to.
    pub bundle_id: i64,
    /// Path-derived OKF concept ID.
    pub concept_id: String,
    /// Bundle-relative source path.
    pub path: String,
    /// Concept title, when present.
    pub title: Option<String>,
    /// OKF concept type, when present.
    pub concept_type: Option<String>,
    /// `ts_rank_cd` relevance score; comparable only within one query.
    pub rank: f32,
    /// `ts_headline` snippet over title, description, and body text.
    pub headline: Option<String>,
}

/// Clamp a file count into the `integer` range of the SQL composites.
///
/// Sync counts are bounded by `pgokf.max_bundle_files` (an `i32` GUC), so
/// saturation is unreachable in practice; clamping keeps the conversion
/// total without panicking inside a backend.
#[must_use]
pub fn count_to_i32(count: usize) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

fn composite_error(type_name: &str, error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {type_name} composite: {error}"),
        Path::new(""),
    )
}

/// Pack a [`SyncReport`] into a `pgokf.bundle_sync_result` heap tuple.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::Internal`] error when the composite
/// type cannot be resolved or an attribute cannot be set — both indicate a
/// corrupted installation, since `catalog_tables` defines the type.
pub fn bundle_sync_result(
    bundle_id: i64,
    path: &str,
    report: SyncReport,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type(BUNDLE_SYNC_RESULT_TYPE)
        .map_err(|error| composite_error(BUNDLE_SYNC_RESULT_TYPE, error))?;
    let set_error = |error| composite_error(BUNDLE_SYNC_RESULT_TYPE, error);
    tuple
        .set_by_name("bundle_id", bundle_id)
        .map_err(set_error)?;
    tuple.set_by_name("path", path).map_err(set_error)?;
    tuple
        .set_by_name("added", count_to_i32(report.added))
        .map_err(set_error)?;
    tuple
        .set_by_name("updated", count_to_i32(report.updated))
        .map_err(set_error)?;
    tuple
        .set_by_name("removed", count_to_i32(report.removed))
        .map_err(set_error)?;
    tuple
        .set_by_name("unchanged", count_to_i32(report.unchanged))
        .map_err(set_error)?;
    tuple
        .set_by_name("total", count_to_i32(report.total()))
        .map_err(set_error)?;
    Ok(tuple)
}

/// Pack a [`SearchHit`] into a `pgokf.concept_search_result` heap tuple.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::Internal`] error when the composite
/// type cannot be resolved or an attribute cannot be set.
pub fn concept_search_result(
    hit: SearchHit,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type(CONCEPT_SEARCH_RESULT_TYPE)
        .map_err(|error| composite_error(CONCEPT_SEARCH_RESULT_TYPE, error))?;
    let set_error = |error| composite_error(CONCEPT_SEARCH_RESULT_TYPE, error);
    tuple
        .set_by_name("bundle_id", hit.bundle_id)
        .map_err(set_error)?;
    tuple
        .set_by_name("concept_id", hit.concept_id)
        .map_err(set_error)?;
    tuple.set_by_name("path", hit.path).map_err(set_error)?;
    tuple.set_by_name("title", hit.title).map_err(set_error)?;
    tuple
        .set_by_name("type", hit.concept_type)
        .map_err(set_error)?;
    tuple.set_by_name("rank", hit.rank).map_err(set_error)?;
    tuple
        .set_by_name("headline", hit.headline)
        .map_err(set_error)?;
    Ok(tuple)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_to_i32_preserves_values_in_range() {
        // Arrange
        let in_range = 42_usize;

        // Act
        let converted = count_to_i32(in_range);

        // Assert
        assert_eq!(converted, 42_i32);
    }

    #[test]
    fn count_to_i32_saturates_at_i32_max() {
        // Arrange
        let out_of_range = usize::MAX;

        // Act
        let converted = count_to_i32(out_of_range);

        // Assert
        assert_eq!(converted, i32::MAX);
    }
}
