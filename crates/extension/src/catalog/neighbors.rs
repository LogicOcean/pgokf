//! Graph-traversal seam (recursive neighbor queries).
//!
//! # Seam contract for the neighbors feature wave
//!
//! This module exposes the SQL-facing traversal API over the `pgokf.links`
//! table populated by [`crate::catalog::links`], without touching the sync
//! engine:
//!
//! - `pgokf.concept_neighbors(concept_id, max_hops, bundle_id)` returns
//!   `SETOF pgokf.concept_neighbor` — the reachable concepts, the shortest
//!   hop count to each, and the path taken;
//! - traversal is a cycle-safe recursive CTE over `pgokf.links`, bundle-scoped
//!   and depth-limited by [`crate::guc::max_graph_hops`] (a hard ceiling that
//!   `max_hops` is capped to); `max_hops < 1` is rejected with SQLSTATE
//!   `22023`;
//! - only resolved internal edges (`resolved AND NOT is_external`) become
//!   traversal edges; external and unresolved rows never do;
//! - authorization is reader-level via
//!   [`crate::security::authorize_current_user`] with
//!   [`crate::security::Operation::Search`];
//! - when `bundle_id` is `NULL` and the concept ID exists in more than one
//!   bundle, the call fails with SQLSTATE `22023` instructing the caller to
//!   disambiguate; a `bundle_id` scopes the traversal to that bundle;
//! - the return type and function SQL are ordered after the links table with
//!   `requires`.

use std::path::Path;

use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::errors::CatalogError;
use crate::guc;
use crate::security;

/// Qualified SQL name of the neighbor-row composite type.
const CONCEPT_NEIGHBOR_TYPE: &str = "pgokf.concept_neighbor";

extension_sql!(
    r"
CREATE TYPE pgokf.concept_neighbor AS (
    source_id   text,
    neighbor_id text,
    hops        integer,
    path        text[],
    title       text
);

COMMENT ON TYPE pgokf.concept_neighbor IS
    'One concept reachable from a start concept: the neighbor ID, shortest hop count, path taken, and neighbor title.';
",
    name = "concept_neighbor_type",
    requires = ["links_table"]
);

/// One reachable neighbor, prior to being packed into the SQL composite.
#[derive(Debug, Clone, PartialEq)]
struct NeighborHit {
    /// The start concept every path originates from.
    source_id: String,
    /// Concept ID reached from the start concept.
    neighbor_id: String,
    /// Shortest number of resolved internal edges from start to neighbor.
    hops: i32,
    /// Concept IDs on the shortest path, from start through neighbor.
    path: Vec<String>,
    /// Neighbor concept title, when present.
    title: Option<String>,
}

/// Depth-limited, cycle-safe traversal over resolved internal edges.
///
/// The recursive CTE seeds from the start concept's outgoing resolved edges
/// and walks forward, guarding against cycles by refusing to revisit a
/// concept already on the current path. The outer query keeps the shortest
/// path per neighbor.
const TRAVERSAL_QUERY: &str = "
    WITH RECURSIVE walk AS (
        SELECT l.source_id,
               l.target_id AS neighbor_id,
               1 AS hops,
               ARRAY[l.source_id, l.target_id]::text[] AS path
        FROM pgokf.links l
        WHERE l.bundle_id = $1
          AND l.source_id = $2
          AND l.resolved
          AND NOT l.is_external
        UNION ALL
        SELECT w.source_id,
               l.target_id,
               w.hops + 1,
               w.path || l.target_id
        FROM walk w
        JOIN pgokf.links l
          ON l.bundle_id = $1
         AND l.source_id = w.neighbor_id
        WHERE l.resolved
          AND NOT l.is_external
          AND w.hops < $3
          AND NOT (l.target_id = ANY (w.path))
    )
    SELECT s.source_id, s.neighbor_id, s.hops, s.path, s.title
    FROM (
        SELECT DISTINCT ON (w.neighbor_id)
               w.source_id,
               w.neighbor_id,
               w.hops,
               w.path,
               c.title
        FROM walk w
        LEFT JOIN pgokf.concepts c
          ON c.bundle_id = $1 AND c.id = w.neighbor_id
        ORDER BY w.neighbor_id, w.hops
    ) s
    ORDER BY s.hops, s.neighbor_id";

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Resolve which bundle a traversal should be scoped to.
///
/// An explicit `bundle_id` is used verbatim. Otherwise the concept ID is
/// looked up across bundles: a single match is scoped to it, no match yields
/// `None` (an empty traversal), and multiple matches are rejected with
/// SQLSTATE `22023` so the caller disambiguates.
fn resolve_bundle_scope(
    concept_id: &str,
    bundle_id: Option<i64>,
) -> Result<Option<i64>, CatalogError> {
    if let Some(explicit) = bundle_id {
        return Ok(Some(explicit));
    }

    let bundles = Spi::connect(|client| {
        let table = client
            .select(
                "SELECT DISTINCT bundle_id FROM pgokf.concepts WHERE id = $1 ORDER BY bundle_id",
                None,
                &[concept_id.into()],
            )
            .map_err(|error| spi_error("failed to resolve concept bundle", &error))?;
        let mut ids = Vec::with_capacity(table.len());
        for row in table {
            let id = row
                .get::<i64>(1)
                .map_err(|error| spi_error("failed to read concept bundle id", &error))?
                .ok_or_else(|| CatalogError::internal("bundle id is NULL", Path::new("")))?;
            ids.push(id);
        }
        Ok::<_, CatalogError>(ids)
    })?;

    match bundles.as_slice() {
        [] => Ok(None),
        [single] => Ok(Some(*single)),
        many => Err(CatalogError::invalid_parameter(
            format!(
                "concept_id '{concept_id}' exists in {} bundles; pass bundle_id to disambiguate",
                many.len()
            ),
            Path::new(""),
        )),
    }
}

/// Validate `max_hops` and clamp it to a hard `ceiling` (pure core).
///
/// Split from [`effective_max_hops`] so the validation and clamping logic is
/// unit-testable without a running backend (the GUC ceiling is injected).
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error (SQLSTATE
/// `22023`) when `max_hops` is below 1.
fn cap_max_hops(max_hops: i32, ceiling: i32) -> Result<i32, CatalogError> {
    if max_hops < 1 {
        return Err(CatalogError::invalid_parameter(
            format!("max_hops must be at least 1, got {max_hops}"),
            Path::new(""),
        ));
    }
    Ok(max_hops.min(ceiling.max(1)))
}

/// Validate `max_hops` and cap it at the [`crate::guc::max_graph_hops`] ceiling.
///
/// # Errors
///
/// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error (SQLSTATE
/// `22023`) when `max_hops` is below 1.
fn effective_max_hops(max_hops: i32) -> Result<i32, CatalogError> {
    let ceiling = i32::try_from(guc::max_graph_hops()).unwrap_or(i32::MAX);
    cap_max_hops(max_hops, ceiling)
}

fn read_neighbor_rows(
    bundle_id: i64,
    concept_id: &str,
    hops: i32,
) -> Result<Vec<NeighborHit>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                TRAVERSAL_QUERY,
                None,
                &[bundle_id.into(), concept_id.into(), hops.into()],
            )
            .map_err(|error| spi_error("neighbor traversal query failed", &error))?;
        let mut hits = Vec::with_capacity(table.len());
        for row in table {
            let read = |error: pgrx::spi::Error| spi_error("failed to read neighbor row", &error);
            let missing = |column: &str| {
                CatalogError::internal(
                    format!("neighbor result column {column} is unexpectedly NULL"),
                    Path::new(""),
                )
            };
            hits.push(NeighborHit {
                source_id: row
                    .get::<String>(1)
                    .map_err(read)?
                    .ok_or_else(|| missing("source_id"))?,
                neighbor_id: row
                    .get::<String>(2)
                    .map_err(read)?
                    .ok_or_else(|| missing("neighbor_id"))?,
                hops: row
                    .get::<i32>(3)
                    .map_err(read)?
                    .ok_or_else(|| missing("hops"))?,
                path: row
                    .get::<Vec<String>>(4)
                    .map_err(read)?
                    .ok_or_else(|| missing("path"))?,
                title: row.get::<String>(5).map_err(read)?,
            });
        }
        Ok(hits)
    })
}

fn concept_neighbors_impl(
    concept_id: &str,
    max_hops: i32,
    bundle_id: Option<i64>,
) -> Result<Vec<NeighborHit>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let hops = effective_max_hops(max_hops)?;
    let Some(scope) = resolve_bundle_scope(concept_id, bundle_id)? else {
        return Ok(Vec::new());
    };
    read_neighbor_rows(scope, concept_id, hops)
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {CONCEPT_NEIGHBOR_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Pack a [`NeighborHit`] into a `pgokf.concept_neighbor` heap tuple.
fn concept_neighbor_tuple(
    hit: NeighborHit,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type(CONCEPT_NEIGHBOR_TYPE).map_err(composite_error)?;
    tuple
        .set_by_name("source_id", hit.source_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("neighbor_id", hit.neighbor_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("hops", hit.hops)
        .map_err(composite_error)?;
    tuple
        .set_by_name("path", hit.path)
        .map_err(composite_error)?;
    tuple
        .set_by_name("title", hit.title)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// SQL-facing traversal entry point, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{concept_neighbor_tuple, concept_neighbors_impl};

    /// Walk the resolved internal link graph outward from a concept.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Returns each
    /// reachable concept with the shortest hop count and the path taken, over
    /// resolved, non-external edges only. `max_hops` must be at least 1
    /// (SQLSTATE `22023` otherwise) and is capped at `pgokf.max_graph_hops`.
    /// When `bundle_id` is omitted and the concept ID exists in more than one
    /// bundle, the call fails with SQLSTATE `22023`; pass `bundle_id` to scope
    /// the traversal.
    #[pg_extern(stable, parallel_safe, requires = ["concept_neighbor_type"])]
    fn concept_neighbors(
        concept_id: &str,
        max_hops: default!(i32, 2),
        bundle_id: default!(Option<i64>, "NULL"),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_neighbor")> {
        let hits = concept_neighbors_impl(concept_id, max_hops, bundle_id)
            .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = hits
            .into_iter()
            .map(|hit| concept_neighbor_tuple(hit).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.concept_neighbors(text, integer, bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_neighbors(text, integer, bigint) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_neighbors(text, integer, bigint) IS
    'Cycle-safe recursive traversal of resolved internal links from a concept. Reader-level; capped at pgokf.max_graph_hops. Raises 22023 on max_hops < 1 or an ambiguous concept_id.';
",
        name = "concept_neighbors_hardening",
        requires = [concept_neighbors]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;

    #[test]
    fn cap_max_hops_rejects_values_below_one() {
        // Arrange & Act & Assert
        for invalid in [0, -1, i32::MIN] {
            let error = cap_max_hops(invalid, 5).expect_err("max_hops below 1 must be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidParameter);
            assert_eq!(error.sqlstate(), "22023");
        }
    }

    #[test]
    fn cap_max_hops_clamps_requests_above_the_ceiling() {
        // Arrange: a request larger than the hard ceiling.
        let ceiling = 5;

        // Act
        let capped = cap_max_hops(i32::MAX, ceiling).expect("a positive request is valid");

        // Assert
        assert_eq!(capped, ceiling);
    }

    #[test]
    fn cap_max_hops_preserves_requests_within_the_ceiling() {
        // Arrange & Act
        let effective = cap_max_hops(3, 5).expect("a request under the ceiling is valid");

        // Assert
        assert_eq!(effective, 3);
    }
}
