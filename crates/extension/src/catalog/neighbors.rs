// SPDX-License-Identifier: AGPL-3.0-only
//! Graph-traversal seam (recursive neighbor queries).
//!
//! # Seam contract for the neighbors feature wave
//!
//! This module exposes the SQL-facing traversal API over the `pgokf.links`
//! table populated by [`crate::catalog::links`], without touching the sync
//! engine:
//!
//! - `pgokf.concept_neighbors(concept_id, max_hops, bundle_id)` returns
//!   `SETOF pgokf.concept_neighbor` - the reachable concepts, the shortest
//!   hop count to each, and the path taken;
//! - traversal is a cycle-safe, set-based **breadth-first search** over
//!   `pgokf.links` (one level-expansion query per hop), bundle-scoped and
//!   depth-limited by [`crate::guc::max_graph_hops`] (a hard ceiling that
//!   `max_hops` is capped to); `max_hops < 1` is rejected with SQLSTATE
//!   `22023`. It records the first (minimum-hop) visit of each neighbor and
//!   never re-expands a visited node, so total work is `O(V + E)` - it replaced
//!   a recursive CTE that enumerated every simple path (≈`O(N^hops)`) and let a
//!   dense bundle spin a reader backend for a tiny answer;
//! - only resolved internal edges (`resolved AND NOT is_external`) become
//!   traversal edges; external and unresolved rows never do;
//! - only edges in an **active** bundle are traversed - active meaning
//!   `enabled AND retired_at IS NULL` (both the seed and the recursive step join
//!   `pgokf.bundles ... AND b.enabled AND b.retired_at IS NULL`), matching
//!   [`crate::catalog::search`]; a disabled *or retired* bundle's concepts are
//!   never returned;
//! - authorization is reader-level via
//!   [`crate::security::authorize_current_user`] with
//!   [`crate::security::Operation::Search`];
//! - when `bundle_id` is `NULL` and the concept ID exists in more than one
//!   bundle, the call fails with SQLSTATE `22023` instructing the caller to
//!   disambiguate; a `bundle_id` scopes the traversal to that bundle;
//! - the return type and function SQL are ordered after the links table with
//!   `requires`.

use std::collections::HashMap;
use std::path::Path;

use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::spi_read::{self, RowReader};
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

/// One level's set-based edge expansion: the resolved internal out-edges of an
/// entire frontier of source concepts, returned as sorted `(source, target)`
/// pairs.
///
/// This replaces the recursive term of the former `WITH RECURSIVE` walk. The old
/// query's only cycle guard was "target not on the current path", so it
/// enumerated **every simple path** from the seed (≈`O(N^hops)`); on a dense
/// bundle a single `concept_neighbors(seed, 5)` could spin the backend on
/// millions of walk rows for a tiny answer - an algorithmic-complexity `DoS`.
/// [`breadth_first_neighbors`] instead drives one of these queries per hop level
/// over the *whole* frontier, visiting each node and edge at most once
/// (`O(V + E)`).
///
/// The predicates are byte-for-byte the old walk's edge filter: the bundle is
/// scoped by `$1`, only **active** bundles (`enabled AND retired_at IS NULL`)
/// contribute edges (matching [`crate::catalog::search`]), and only resolved,
/// non-external links are traversed. `ORDER BY` makes the per-level discovery
/// order deterministic, so the shortest path recorded for each neighbor is
/// stable.
const EDGE_QUERY: &str = "
    SELECT DISTINCT l.source_id, l.target_id
    FROM pgokf.links l
    JOIN pgokf.bundles b ON b.id = l.bundle_id AND b.enabled AND b.retired_at IS NULL
    WHERE l.bundle_id = $1
      AND l.source_id = ANY ($2)
      AND l.resolved
      AND NOT l.is_external
    ORDER BY l.source_id, l.target_id";

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// One node's shortest-path record while the breadth-first traversal runs: the
/// hop count of its first (minimum-hop) visit and the path taken to reach it.
#[derive(Debug, Clone)]
struct VisitRecord {
    hops: i32,
    path: Vec<String>,
}

/// Breadth-first traversal core over the resolved internal link graph.
///
/// Expands one hop level at a time from the current frontier, recording the
/// **first** (minimum-hop) visit of each neighbor and never re-expanding a node
/// once it is visited. Each node is therefore expanded at most once and each
/// edge examined at most once, bounding total work to `O(V + E)` - in place of
/// the former recursive CTE that enumerated every simple path (≈`O(N^hops)`).
/// The result is identical to that query's for a normal graph: the set of
/// reachable neighbors, each with its shortest hop distance and a shortest path.
///
/// The traversal is cycle-safe (a visited node is never re-expanded, so a cycle
/// cannot loop) and the seed is pre-marked visited at hop 0, so it is never
/// emitted as its own neighbor even across a self-link. Within a level,
/// neighbors are discovered in the sorted `(source, target)` order
/// [`EDGE_QUERY`] returns, so the shortest path recorded for each neighbor is
/// deterministic.
///
/// `edges_from` yields the resolved internal out-edges of a frontier; injecting
/// it keeps this core unit-testable without a backend. Neighbors are returned in
/// discovery order (the seed excluded); the caller applies the final
/// `(hops, neighbor_id)` ordering after attaching titles.
fn breadth_first_neighbors<F>(
    seed: &str,
    max_hops: i32,
    mut edges_from: F,
) -> Result<Vec<(String, VisitRecord)>, CatalogError>
where
    F: FnMut(&[String]) -> Result<Vec<(String, String)>, CatalogError>,
{
    let mut visited: HashMap<String, VisitRecord> = HashMap::new();
    visited.insert(
        seed.to_owned(),
        VisitRecord {
            hops: 0,
            path: vec![seed.to_owned()],
        },
    );
    // Discovered neighbors in first-visit order (the seed is never pushed);
    // `visited` provides O(1) membership so a node is recorded exactly once.
    let mut discovered: Vec<String> = Vec::new();
    let mut frontier: Vec<String> = vec![seed.to_owned()];
    let mut hop: i32 = 1;

    while hop <= max_hops && !frontier.is_empty() {
        let edges = edges_from(&frontier)?;
        let mut next: Vec<String> = Vec::new();
        for (source, target) in edges {
            if visited.contains_key(&target) {
                continue;
            }
            // `source` is always already visited - it is a frontier member, and
            // every frontier member was recorded when it was discovered - so its
            // shortest path is available to extend by one edge.
            let mut path = visited
                .get(&source)
                .map_or_else(|| vec![seed.to_owned()], |record| record.path.clone());
            path.push(target.clone());
            visited.insert(target.clone(), VisitRecord { hops: hop, path });
            discovered.push(target.clone());
            next.push(target);
        }
        frontier = next;
        hop += 1;
    }

    // Detach each discovered neighbor's record (discovery order; seed excluded).
    Ok(discovered
        .into_iter()
        .map(|id| {
            let record = visited
                .remove(&id)
                .expect("a discovered neighbor is always recorded in visited");
            (id, record)
        })
        .collect())
}

/// Resolve which bundle a traversal should be scoped to.
///
/// An explicit `bundle_id` is used verbatim. Otherwise the concept ID is
/// looked up across **active** bundles only - `enabled AND retired_at IS NULL`,
/// mirroring the traversal's own edge filter: a single match is scoped to it,
/// no match yields `None` (an empty traversal), and multiple matches are
/// rejected with SQLSTATE `22023` so the caller disambiguates. Filtering to
/// active bundles here is essential - counting a disabled or retired duplicate
/// of the concept would raise a spurious `22023` that blocks a traversal the
/// only *active* bundle could answer unambiguously.
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
                "SELECT DISTINCT c.bundle_id
                 FROM pgokf.concepts c
                 JOIN pgokf.bundles b
                   ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
                 WHERE c.id = $1
                 ORDER BY c.bundle_id",
                None,
                &[concept_id.into()],
            )
            .map_err(|error| spi_error("failed to resolve concept bundle", &error))?;
        let mut ids = Vec::with_capacity(table.len());
        for row in table {
            let id = spi_read::required_column::<i64>(
                &row,
                1,
                "failed to read concept bundle id",
                "bundle id is NULL",
            )?;
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

/// Fetch one hop level's resolved internal out-edges for a whole frontier, via
/// the set-based [`EDGE_QUERY`]. Returned pairs are sorted `(source, target)`.
fn expand_frontier(
    bundle_id: i64,
    frontier: &[String],
) -> Result<Vec<(String, String)>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                EDGE_QUERY,
                None,
                &[bundle_id.into(), frontier.to_vec().into()],
            )
            .map_err(|error| spi_error("neighbor traversal edge query failed", &error))?;
        let mut edges = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read neighbor edge", "neighbor edge");
            let source = reader.required::<String>(1, "source_id")?;
            let target = reader.required::<String>(2, "target_id")?;
            edges.push((source, target));
        }
        Ok(edges)
    })
}

/// Look up the titles of the discovered neighbors in one set-based query, keyed
/// by concept id.
///
/// A neighbor absent from the returned map is one whose concept no longer exists
/// in the bundle; the caller drops it, giving the same inner-join semantics the
/// former traversal's `JOIN pgokf.concepts` had - defense in depth alongside the
/// bundle-wide re-resolution that already clears `resolved` on edges to deleted
/// targets ([`crate::catalog::links::reresolve_bundle`]).
fn fetch_neighbor_titles(
    bundle_id: i64,
    neighbor_ids: &[String],
) -> Result<HashMap<String, Option<String>>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT c.id, c.title
                 FROM pgokf.concepts c
                 WHERE c.bundle_id = $1 AND c.id = ANY ($2)",
                None,
                &[bundle_id.into(), neighbor_ids.to_vec().into()],
            )
            .map_err(|error| spi_error("failed to read neighbor titles", &error))?;
        let mut titles = HashMap::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read neighbor title", "neighbor title");
            let id = reader.required::<String>(1, "id")?;
            let title = reader.optional::<String>(2)?;
            titles.insert(id, title);
        }
        Ok(titles)
    })
}

/// Traverse the bundle's resolved internal link graph outward from a concept and
/// return each reachable neighbor with its shortest hop count, path, and title.
///
/// Runs the `O(V + E)` breadth-first [`breadth_first_neighbors`] core, fetching
/// each level's out-edges through the set-based [`EDGE_QUERY`], then attaches
/// titles (dropping any neighbor whose concept no longer exists) and applies the
/// stable `(hops, neighbor_id)` ordering the SQL surface promises.
fn read_neighbor_rows(
    bundle_id: i64,
    concept_id: &str,
    hops: i32,
) -> Result<Vec<NeighborHit>, CatalogError> {
    let visits = breadth_first_neighbors(concept_id, hops, |frontier| {
        expand_frontier(bundle_id, frontier)
    })?;
    if visits.is_empty() {
        return Ok(Vec::new());
    }

    let neighbor_ids: Vec<String> = visits.iter().map(|(id, _)| id.clone()).collect();
    let titles = fetch_neighbor_titles(bundle_id, &neighbor_ids)?;

    let mut hits: Vec<NeighborHit> = visits
        .into_iter()
        .filter_map(|(id, record)| {
            // Present in `titles` ⇔ the concept still exists (inner-join parity).
            titles.get(&id).map(|title| NeighborHit {
                source_id: concept_id.to_owned(),
                neighbor_id: id,
                hops: record.hops,
                path: record.path,
                title: title.clone(),
            })
        })
        .collect();
    hits.sort_by(|left, right| {
        left.hops
            .cmp(&right.hops)
            .then_with(|| left.neighbor_id.cmp(&right.neighbor_id))
    });
    Ok(hits)
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
    'Cycle-safe recursive traversal of resolved internal links from a concept, over active bundles only (enabled AND not retired; matching concept_search). Reader-level; capped at pgokf.max_graph_hops. Raises 22023 on max_hops < 1 or an ambiguous concept_id.';
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

    /// An in-memory directed graph as a sorted adjacency list, used to drive the
    /// pure [`breadth_first_neighbors`] core without a backend. Returns the
    /// out-edges of a whole frontier as sorted `(source, target)` pairs, exactly
    /// as `EDGE_QUERY` does.
    fn edges_from_graph<'a>(
        adjacency: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&[String]) -> Result<Vec<(String, String)>, CatalogError> + 'a {
        move |frontier: &[String]| {
            let mut edges: Vec<(String, String)> = adjacency
                .iter()
                .filter(|(source, _)| frontier.iter().any(|node| node == source))
                .map(|(source, target)| ((*source).to_owned(), (*target).to_owned()))
                .collect();
            edges.sort();
            Ok(edges)
        }
    }

    /// Run the BFS core and flatten it to `(neighbor_id, hops, path)` triples in
    /// the final `(hops, neighbor_id)` order the SQL surface promises.
    fn neighbors_of(
        seed: &str,
        max_hops: i32,
        adjacency: &[(&str, &str)],
    ) -> Vec<(String, i32, Vec<String>)> {
        let visits = breadth_first_neighbors(seed, max_hops, edges_from_graph(adjacency))
            .expect("the in-memory traversal never errors");
        let mut rows: Vec<(String, i32, Vec<String>)> = visits
            .into_iter()
            .map(|(id, record)| (id, record.hops, record.path))
            .collect();
        rows.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
        rows
    }

    #[test]
    fn breadth_first_neighbors_walks_a_chain_with_shortest_paths() {
        // Arrange: a --> b --> c.
        let graph = [("a", "b"), ("b", "c")];

        // Act
        let rows = neighbors_of("a", 5, &graph);

        // Assert: b at hop 1, c at hop 2, each with its shortest path.
        assert_eq!(
            rows,
            vec![
                ("b".to_owned(), 1, vec!["a".to_owned(), "b".to_owned()]),
                (
                    "c".to_owned(),
                    2,
                    vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn breadth_first_neighbors_keeps_the_minimum_hop_on_multiple_paths() {
        // Arrange: a diamond a->b, a->c, b->d, c->d. d is reachable at hop 2 by
        // two paths; the shortest hop (2) and a single deterministic path win.
        let graph = [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")];

        // Act
        let rows = neighbors_of("a", 5, &graph);

        // Assert: b, c at hop 1; d at hop 2 via the sorted-first source (b).
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], ("b".to_owned(), 1, vec!["a".into(), "b".into()]));
        assert_eq!(rows[1], ("c".to_owned(), 1, vec!["a".into(), "c".into()]));
        assert_eq!(
            rows[2],
            ("d".to_owned(), 2, vec!["a".into(), "b".into(), "d".into()])
        );
    }

    #[test]
    fn breadth_first_neighbors_is_cycle_safe() {
        // Arrange: a cycle a -> b -> a. The seed must never re-expand and must
        // not appear as its own neighbor.
        let graph = [("a", "b"), ("b", "a")];

        // Act
        let rows = neighbors_of("a", 5, &graph);

        // Assert: only b, at hop 1; no run-away and no self-neighbor.
        assert_eq!(
            rows,
            vec![("b".to_owned(), 1, vec!["a".into(), "b".into()])]
        );
    }

    #[test]
    fn breadth_first_neighbors_excludes_the_seed_across_a_self_link() {
        // Arrange: a self-link a -> a plus a real edge a -> b. The self-link must
        // not make the seed its own neighbor.
        let graph = [("a", "a"), ("a", "b")];

        // Act
        let rows = neighbors_of("a", 5, &graph);

        // Assert: only b is a neighbor; a (the seed) is excluded.
        assert_eq!(
            rows,
            vec![("b".to_owned(), 1, vec!["a".into(), "b".into()])]
        );
    }

    #[test]
    fn breadth_first_neighbors_honors_the_hop_ceiling() {
        // Arrange: a --> b --> c --> d, capped at one hop.
        let graph = [("a", "b"), ("b", "c"), ("c", "d")];

        // Act
        let rows = neighbors_of("a", 1, &graph);

        // Assert: only the direct neighbor b is returned.
        assert_eq!(
            rows,
            vec![("b".to_owned(), 1, vec!["a".into(), "b".into()])]
        );
    }
}
