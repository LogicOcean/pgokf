//! Cross-bundle content-duplicate detection (`pgokf.duplicate_concepts`).
//!
//! Concepts are stored with a BLAKE3 `file_hash` of their source bytes
//! ([`crate::catalog::schema`]), the same identity the incremental sync uses.
//! Grouping concepts by that hash surfaces byte-identical content wherever it
//! appears — most usefully the *same* runbook or reference copied across
//! several bundles — so an operator can find and de-duplicate it.
//!
//! # Security model
//!
//! `duplicate_concepts` runs with **invoker rights** (no `SECURITY DEFINER`): it
//! reads only `pgokf.concepts`, which `pgokf_reader` already holds `SELECT` on,
//! so row-level security filters the scan to the session's tenant automatically
//! and escalating to the owner would grant nothing. Authorization adds the
//! reader role-policy check ([`crate::security::Operation::Search`]) as defense
//! in depth on top of the `EXECUTE`/`SELECT` grants.

use std::path::Path;

use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi};

use crate::catalog::spi_read::RowReader;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the duplicate-group composite type.
const DUPLICATE_GROUP_TYPE: &str = "pgokf.duplicate_group";

/// One group of byte-identical concepts sharing a `file_hash`.
struct DuplicateGroup {
    file_hash: String,
    occurrences: i64,
    bundle_ids: Vec<i64>,
    concept_ids: Vec<String>,
}

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {DUPLICATE_GROUP_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Validate `min_group`: a duplicate group needs at least two members, so the
/// threshold must be at least 1 (values below 1 are rejected with SQLSTATE
/// `22023`; a threshold of 1 or 2 both surface every real duplicate).
fn validate_min_group(min_group: i32) -> Result<i64, CatalogError> {
    if min_group < 1 {
        return Err(CatalogError::invalid_parameter(
            format!("min_group must be at least 1, got {min_group}"),
            Path::new(""),
        ));
    }
    Ok(i64::from(min_group))
}

/// Group concepts by `file_hash`, keeping groups of at least `min_group`.
///
/// When `bundle_id` is set, only groups that *touch* that bundle are returned —
/// but each such group still lists every occurrence across all bundles, so a
/// concept copied out of the named bundle into others is fully visible.
fn duplicate_concepts_impl(
    bundle_id: Option<i64>,
    min_group: i32,
) -> Result<Vec<DuplicateGroup>, CatalogError> {
    // Invoker rights: the scan is RLS-filtered to the session's tenant. The
    // optional bundle scope keeps a group only if the shared hash appears in the
    // named bundle, while array_agg still reports every occurrence (ordered for a
    // stable listing). Both filter values bind as parameters.
    const QUERY: &str = "
        SELECT c.file_hash,
               pg_catalog.count(*) AS occurrences,
               pg_catalog.array_agg(c.bundle_id ORDER BY c.bundle_id, c.id) AS bundle_ids,
               pg_catalog.array_agg(c.id ORDER BY c.bundle_id, c.id) AS concept_ids
        FROM pgokf.concepts c
        WHERE $1::bigint IS NULL
           OR c.file_hash IN (
                  SELECT c2.file_hash FROM pgokf.concepts c2 WHERE c2.bundle_id = $1)
        GROUP BY c.file_hash
        HAVING pg_catalog.count(*) >= $2
        ORDER BY pg_catalog.count(*) DESC, c.file_hash";

    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let threshold = validate_min_group(min_group)?;
    Spi::connect(|client| {
        let table = client
            .select(QUERY, None, &[bundle_id.into(), threshold.into()])
            .map_err(|error| spi_error("failed to read duplicate concepts", &error))?;
        let mut groups = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(
                &row,
                "failed to read duplicate_group column",
                "duplicate_group",
            );
            groups.push(DuplicateGroup {
                file_hash: reader.required(1, "file_hash")?,
                occurrences: reader.required(2, "occurrences")?,
                bundle_ids: reader.required::<Vec<i64>>(3, "bundle_ids")?,
                concept_ids: reader.required::<Vec<String>>(4, "concept_ids")?,
            });
        }
        Ok(groups)
    })
}

/// Pack a [`DuplicateGroup`] into a `pgokf.duplicate_group` heap tuple.
fn duplicate_group_tuple(
    group: DuplicateGroup,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type(DUPLICATE_GROUP_TYPE).map_err(composite_error)?;
    tuple
        .set_by_name("file_hash", group.file_hash)
        .map_err(composite_error)?;
    tuple
        .set_by_name("occurrences", group.occurrences)
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_ids", group.bundle_ids)
        .map_err(composite_error)?;
    tuple
        .set_by_name("concept_ids", group.concept_ids)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// SQL-facing duplicate-detection entry point, installed into the `pgokf`
/// schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{duplicate_concepts_impl, duplicate_group_tuple};

    extension_sql!(
        r"
CREATE TYPE pgokf.duplicate_group AS (
    file_hash   text,
    occurrences bigint,
    bundle_ids  bigint[],
    concept_ids text[]
);

COMMENT ON TYPE pgokf.duplicate_group IS
    'One group of byte-identical concepts from pgokf.duplicate_concepts: the shared BLAKE3 file_hash, how many concepts share it, and the parallel bundle_ids / concept_ids arrays of every occurrence (ordered by bundle then concept id).';
",
        name = "duplicate_group_type",
        requires = ["catalog_tables"]
    );

    /// Find groups of byte-identical concepts (same BLAKE3 `file_hash`).
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Groups
    /// concepts by `file_hash`, keeping groups with at least `min_group`
    /// members (default 2), so an operator can find the same runbook or
    /// reference copied across bundles. Each group lists every occurrence
    /// (`bundle_ids` / `concept_ids`). When `bundle_id` is given, only groups
    /// that touch that bundle are returned — but they still list occurrences in
    /// every bundle. `min_group` must be at least 1 (SQLSTATE `22023`
    /// otherwise).
    #[pg_extern(stable, parallel_safe, requires = ["duplicate_group_type"])]
    fn duplicate_concepts(
        bundle_id: default!(Option<i64>, "NULL"),
        min_group: default!(i32, 2),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.duplicate_group")> {
        let groups =
            duplicate_concepts_impl(bundle_id, min_group).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = groups
            .into_iter()
            .map(|group| duplicate_group_tuple(group).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.duplicate_concepts(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.duplicate_concepts(bigint, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.duplicate_concepts(bigint, integer) IS
    'Group byte-identical concepts by BLAKE3 file_hash (HAVING count(*) >= min_group, default 2) as pgokf.duplicate_group, so an operator can find the same content copied across bundles. Reader-level, STABLE, invoker rights (RLS-filtered to the tenant). Optional bundle_id keeps only groups touching that bundle (still listing every occurrence). Raises 22023 when min_group < 1.';
",
        name = "duplicate_concepts_hardening",
        requires = [duplicate_concepts]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_min_group_accepts_one_and_above() {
        // Arrange / Act / Assert
        assert_eq!(validate_min_group(1).expect("1 is valid"), 1);
        assert_eq!(validate_min_group(2).expect("2 is valid"), 2);
    }

    #[test]
    fn validate_min_group_rejects_below_one() {
        // Arrange / Act
        let error = validate_min_group(0).expect_err("min_group below 1 must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
    }
}
