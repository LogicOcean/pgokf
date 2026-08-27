//! Link-graph projection seam (OKF v0.2 `pgokf.links`).
//!
//! # Seam contract for the links feature wave
//!
//! Everything here lives in **this file only** — the sync engine already calls
//! [`project`] and must not be edited. This module:
//!
//! 1. Defines the `pgokf.links` table in its own `extension_sql!` block
//!    (`name = "links_table"`, `requires = ["catalog_tables"]`), keyed by
//!    `(bundle_id, source_id) REFERENCES pgokf.concepts (bundle_id, id)
//!    ON DELETE CASCADE` so removed concepts drop their outgoing edges
//!    automatically (which is why removals need no seam call). Column names
//!    match the graph examples in `examples/queries/graph.sql`: `source_id`,
//!    `target_id`, `link_text`, `target_path`, `link_kind`, `resolved`,
//!    `is_external`, and `ordinal`.
//! 2. In [`project`], for each [`StagedConcept`] deletes the concept's
//!    existing outgoing edges and re-inserts one row per
//!    [`okf_parser::Link`]. Each link already carries the raw `target`,
//!    `label`, `kind`, `ordinal`, `is_external`, and the normalized
//!    `target_path` / `target_id` for internal destinations, so no re-parsing
//!    is required. An internal edge is marked `resolved` only when its
//!    `target_id` matches a concept that exists in the same bundle; unresolved
//!    internal links are retained (OKF permits broken links). External links
//!    carry `target_id = NULL` and `resolved = false`.
//! 3. In [`reresolve_bundle`], run once by the sync engine after the concept
//!    set is finalized, recomputes `resolved` bundle-wide against the current
//!    concepts. Because [`project`] only reprojects *staged* sources, this pass
//!    is what keeps the inbound edges of *unchanged* concepts correct: an edge
//!    to a target added this sync flips `false` → `true`, and an edge to a
//!    target removed this sync flips `true` → `false` — so the graph
//!    self-corrects and never emits phantom neighbors for deleted targets.
//! 4. Uses parameterized SPI only, surfacing failures as [`CatalogError`] so
//!    the surrounding sync transaction rolls back atomically.
//!
//! Traversal APIs (recursive neighbors) belong in
//! [`crate::catalog::neighbors`], not here.

use std::path::Path;

use okf_parser::LinkKind;
use pgrx::{Spi, extension_sql};

use crate::catalog::batch::{self, BATCH_SIZE};
use crate::catalog::types::StagedConcept;
use crate::errors::CatalogError;

extension_sql!(
    r"
CREATE TABLE pgokf.links (
    bundle_id   bigint  NOT NULL,
    source_id   text    NOT NULL,
    target_id   text,
    link_text   text,
    target_path text,
    link_kind   text    NOT NULL,
    resolved    boolean NOT NULL DEFAULT false,
    is_external boolean NOT NULL DEFAULT false,
    ordinal     integer NOT NULL,
    CONSTRAINT links_pkey PRIMARY KEY (bundle_id, source_id, ordinal),
    CONSTRAINT links_source_fk
        FOREIGN KEY (bundle_id, source_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE INDEX links_target_idx ON pgokf.links (bundle_id, target_id);

COMMENT ON TABLE pgokf.links IS
    'Directed Markdown links extracted per concept during sync: one row per outgoing link, in source order.';
COMMENT ON COLUMN pgokf.links.source_id IS
    'Concept ID of the document the link was extracted from (references pgokf.concepts.id in the same bundle).';
COMMENT ON COLUMN pgokf.links.target_id IS
    'Concept ID of an internal destination (target_path without .md); NULL for external destinations.';
COMMENT ON COLUMN pgokf.links.link_text IS
    'Plain-text label of the Markdown link.';
COMMENT ON COLUMN pgokf.links.target_path IS
    'Normalized bundle-relative destination path (with .md) for internal links; NULL for external ones.';
COMMENT ON COLUMN pgokf.links.link_kind IS
    'Markdown construct that produced the link: inline, reference, autolink, email, or image.';
COMMENT ON COLUMN pgokf.links.resolved IS
    'True only for an internal link whose target_id matches an existing concept in the same bundle; recomputed bundle-wide after every sync so it stays correct as targets are added or removed.';
COMMENT ON COLUMN pgokf.links.is_external IS
    'True when the destination is a scheme-qualified or protocol-relative URL; external links never become graph edges.';
COMMENT ON COLUMN pgokf.links.ordinal IS
    'Zero-based position of the link within its source document, in document order.';

GRANT SELECT ON pgokf.links TO pgokf_reader;
",
    name = "links_table",
    requires = ["catalog_tables"]
);

/// Map a parser [`LinkKind`] to the text value stored in `pgokf.links.link_kind`.
///
/// The variants mirror the `snake_case` serde encoding of [`LinkKind`] so the
/// stored text is stable and matches the producer-facing vocabulary.
#[must_use]
pub fn link_kind_text(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Inline => "inline",
        LinkKind::Reference => "reference",
        LinkKind::Autolink => "autolink",
        LinkKind::Email => "email",
        LinkKind::Image => "image",
    }
}

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Delete every outgoing edge previously projected for the staged source
/// concepts, in bounded batches.
///
/// Each staged concept's ID is a bundle-unique source, so clearing all staged
/// sources' edges up front is equivalent to the row-by-row
/// `delete-then-insert` per concept: no staged source's freshly inserted edges
/// can be deleted by another source's clear. Concept IDs are chunked at
/// [`BATCH_SIZE`] so the `= ANY($2)` list never grows unbounded.
fn delete_staged_outgoing(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    for chunk in staged.chunks(BATCH_SIZE) {
        let source_ids: Vec<&str> = chunk
            .iter()
            .map(|entry| entry.concept.id.as_str())
            .collect();
        Spi::run_with_args(
            "DELETE FROM pgokf.links WHERE bundle_id = $1 AND source_id = ANY($2)",
            &[bundle_id.into(), source_ids.into()],
        )
        .map_err(|error| spi_error("failed to clear concept links", &error))?;
    }
    Ok(())
}

/// Bulk-insert every staged concept's outgoing edges with one array-unnest
/// `INSERT` per [`BATCH_SIZE`] chunk, computing `resolved` set-based.
///
/// `resolved` is derived in SQL exactly as the row-by-row `INSERT` did — an
/// internal (`target_id IS NOT NULL`), non-external edge whose target concept
/// exists in this bundle — via a correlated `EXISTS` against `pgokf.concepts`
/// evaluated per unnested row. The concept rows for the bundle are already
/// written when the seam runs, so the check sees the full current concept set;
/// the inbound edges of concepts that were *not* staged this sync are corrected
/// separately by [`reresolve_bundle`].
fn insert_staged_links(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.links
            (bundle_id, source_id, target_id, link_text, target_path,
             link_kind, resolved, is_external, ordinal)
        SELECT
            $1, d.source_id, d.target_id, d.link_text, d.target_path,
            d.link_kind,
            (d.target_id IS NOT NULL AND NOT d.is_external AND EXISTS (
                 SELECT 1 FROM pgokf.concepts c
                 WHERE c.bundle_id = $1 AND c.id = d.target_id)),
            d.is_external, d.ordinal
        FROM unnest(
                 $2::text[], $3::text[], $4::text[], $5::text[],
                 $6::text[], $7::boolean[], $8::integer[])
             AS d(source_id, target_id, link_text, target_path,
                   link_kind, is_external, ordinal)";

    let columns = batch::marshal_links(staged)?;
    let total = columns.source_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                columns.source_ids[start..end].to_vec().into(),
                columns.target_ids[start..end].to_vec().into(),
                columns.link_texts[start..end].to_vec().into(),
                columns.target_paths[start..end].to_vec().into(),
                columns.link_kinds[start..end].to_vec().into(),
                columns.is_externals[start..end].to_vec().into(),
                columns.ordinals[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert concept links", &error))?;
    }
    Ok(())
}

/// Project the outgoing links of every staged concept into `pgokf.links`.
///
/// Invoked inside the sync transaction after concept rows (and their metadata)
/// are written and before the bundle row is finalized. The staged sources'
/// existing edges are cleared and one row is inserted per extracted
/// [`okf_parser::Link`], preserving document order via `ordinal`. Internal
/// edges are marked `resolved` when their target concept already exists in the
/// bundle; unresolved internal and external edges are still retained. Both
/// phases are set-based and chunked at [`BATCH_SIZE`], replacing the former
/// per-link SPI round-trips while producing byte-identical rows.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure so a partial projection
/// aborts the surrounding sync transaction.
pub fn project(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    delete_staged_outgoing(bundle_id, staged)?;
    insert_staged_links(bundle_id, staged)?;
    Ok(())
}

/// Re-evaluate `resolved` for every internal link in one bundle against the
/// finalized concept set.
///
/// [`project`] only reprojects the outgoing edges of *staged* (added or
/// updated) sources, so the inbound edges of an *unchanged* concept keep the
/// `resolved` value they were given when their source was last written — even
/// after this sync added or removed the target they point at. Run once inside
/// the sync transaction after the concept set is finalized (post upsert and
/// delete), this bundle-wide pass recomputes `resolved` from the current
/// concepts, so:
///
/// - an edge to a target *added* this sync flips stale `false` → `true`, and
/// - an edge to a target *removed* this sync flips `true` → `false` (so a
///   dangling `target_id` can no longer produce a phantom graph edge).
///
/// External edges (`is_external`) and edges without a `target_id` are always
/// left `false`, matching the per-insert rule in [`insert_link`].
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure so a partial projection aborts
/// the surrounding sync transaction.
pub fn reresolve_bundle(bundle_id: i64) -> Result<(), CatalogError> {
    // The desired `resolved` value for a link row. Written once here and
    // interpolated into both the `SET` and the guard so the two can never
    // drift: the guard restricts the write to rows whose value actually
    // changes. `resolved` is `NOT NULL` and this expression is likewise never
    // NULL (`target_id IS NOT NULL`, `is_external`, and `EXISTS` each yield a
    // definite boolean), so `IS DISTINCT FROM` is an exact "changed?" test.
    const RESOLVED_EXPR: &str = "(links.target_id IS NOT NULL
            AND NOT links.is_external
            AND EXISTS (
                SELECT 1 FROM pgokf.concepts c
                WHERE c.bundle_id = links.bundle_id
                  AND c.id = links.target_id))";

    // Only rewrite rows whose resolution flips (F1/F14: adding a target flips
    // false -> true, removing one flips true -> false). A no-op re-resolution
    // touches zero rows, which avoids rewriting the whole heap (dead-tuple
    // bloat) and keeps the statement's planner cost below the JIT threshold.
    let reresolve = format!(
        "UPDATE pgokf.links
         SET resolved = {RESOLVED_EXPR}
         WHERE links.bundle_id = $1
           AND links.resolved IS DISTINCT FROM {RESOLVED_EXPR}"
    );

    Spi::run_with_args(&reresolve, &[bundle_id.into()])
        .map_err(|error| spi_error("failed to re-resolve concept links", &error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_kind_text_maps_every_variant_to_its_snake_case_token() {
        // Arrange & Act & Assert
        assert_eq!(link_kind_text(LinkKind::Inline), "inline");
        assert_eq!(link_kind_text(LinkKind::Reference), "reference");
        assert_eq!(link_kind_text(LinkKind::Autolink), "autolink");
        assert_eq!(link_kind_text(LinkKind::Email), "email");
        assert_eq!(link_kind_text(LinkKind::Image), "image");
    }
}
