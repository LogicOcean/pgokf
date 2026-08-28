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

use okf_parser::{LinkKind, Value, resolve_reference};
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
    tenant_id   text    NOT NULL DEFAULT 'default',
    -- Appended last (after tenant_id) so a fresh install's column layout matches
    -- the ALTER TABLE ... ADD COLUMN an upgraded 0.1.9 catalog receives.
    link_relation text  NOT NULL DEFAULT 'reference',
    CONSTRAINT links_pkey PRIMARY KEY (bundle_id, source_id, ordinal),
    CONSTRAINT links_source_fk
        FOREIGN KEY (bundle_id, source_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE INDEX links_target_idx ON pgokf.links (bundle_id, target_id);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER sync path bypasses
-- it to project a single-tenant bundle's edges.
ALTER TABLE pgokf.links ENABLE ROW LEVEL SECURITY;
CREATE POLICY links_tenant_isolation ON pgokf.links
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

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
    'Zero-based position of the link within its source document, in document order. Frontmatter-derived attestation edges are numbered after the body links of the same source (from the body link count upward), so they never collide on the (bundle_id, source_id, ordinal) key.';
COMMENT ON COLUMN pgokf.links.link_relation IS
    'Semantic relation the edge represents, distinct from the Markdown construct in link_kind. ''reference'' (the default) for every ordinary Markdown link; for an Attested Computation concept''s type-specific reference fields, ''attestation:computation'', ''attestation:executor'', or ''attestation:attester'', so a reader can SELECT the typed edges while concept_neighbors traverses them like any resolved internal edge.';
COMMENT ON COLUMN pgokf.links.tenant_id IS
    'Multi-tenant owner, denormalized from the edge''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

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
            (bundle_id, tenant_id, source_id, target_id, link_text, target_path,
             link_kind, resolved, is_external, ordinal)
        SELECT
            $1,
            (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
            d.source_id, d.target_id, d.link_text, d.target_path,
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

/// The OKF v0.2 concept type whose type-specific fields carry resource
/// references that become graph edges. Matched case-insensitively so a
/// producer's casing never suppresses the attestation edges.
const ATTESTED_COMPUTATION_TYPE: &str = "attested computation";

/// The reference-bearing keys of an OKF v0.2 Attested Computation and the
/// `link_relation` each projects, in a fixed order so an attested concept's
/// edges are numbered deterministically.
///
/// Only these three keys are resolved: the OKF v0.2 spec defines no other
/// reference-bearing type-specific field, so the set is closed and never
/// invents an edge from producer data.
const ATTESTATION_KEYS: [(&str, &str); 3] = [
    ("computation", "attestation:computation"),
    ("executor", "attestation:executor"),
    ("attester", "attestation:attester"),
];

/// Whether a concept type is the OKF v0.2 Attested Computation type.
fn is_attested_computation(concept_type: &str) -> bool {
    concept_type
        .trim()
        .eq_ignore_ascii_case(ATTESTED_COMPUTATION_TYPE)
}

/// Read the resource reference a type-specific attestation field points at.
///
/// OKF v0.2 permits either a bare string destination (`computation:
/// /computation.md`) or a mapping carrying a `resource` (`executor: {resource:
/// /executor.md, receipt: [...]}`); both spellings are read, and any other
/// shape (absent, numeric, an object without a string `resource`) yields
/// `None`, so a malformed field simply projects no edge and never aborts the
/// sync.
fn reference_string(value: &Value) -> Option<&str> {
    if let Some(text) = value.as_str() {
        return Some(text);
    }
    value.get("resource").and_then(Value::as_str)
}

/// One resolved attestation edge staged for insertion into `pgokf.links`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttestationEdge {
    source_id: String,
    target_id: Option<String>,
    target_path: Option<String>,
    is_external: bool,
    ordinal: i32,
    relation: &'static str,
}

/// Collect the attestation edges of every staged Attested Computation concept,
/// in staging order.
///
/// A non-attested concept contributes nothing, so its link projection is
/// unchanged. For an attested concept each present, resolvable reference key
/// (`computation` / `executor` / `attester`) produces one edge, resolved
/// through [`okf_parser::resolve_reference`] exactly as a Markdown link
/// destination is — an internal reference carries the normalized `target_path`
/// / `target_id`, an external one carries neither and `is_external = true`, and
/// an unresolvable internal one carries neither with `is_external = false`
/// (retained like any broken link). Edge ordinals continue after the source's
/// body links (`concept.links.len()` upward) so they never collide with them on
/// the `(bundle_id, source_id, ordinal)` key.
///
/// # Errors
///
/// Returns a [`CatalogError`] if an edge ordinal exceeds the `i32` range of the
/// `ordinal` column — unreachable in practice (bounded by `max_file_bytes`),
/// mirroring [`batch::marshal_links`].
fn collect_attestation_edges(
    staged: &[StagedConcept],
) -> Result<Vec<AttestationEdge>, CatalogError> {
    let mut edges = Vec::new();
    for entry in staged {
        let concept = &entry.concept;
        if !is_attested_computation(&concept.r#type) {
            continue;
        }
        // Body links occupy ordinals 0..links.len(); attestation edges continue
        // from there so the primary key never collides.
        let mut ordinal = i32::try_from(concept.links.len()).map_err(|_| {
            CatalogError::internal(
                format!(
                    "attested concept {} has more body links than the i32 ordinal range",
                    concept.id
                ),
                Path::new(""),
            )
        })?;
        for (key, relation) in ATTESTATION_KEYS {
            let Some(reference) = concept.metadata.get(key).and_then(reference_string) else {
                continue;
            };
            let resolved = resolve_reference(reference, &concept.path);
            edges.push(AttestationEdge {
                source_id: concept.id.clone(),
                target_id: resolved.target_id,
                target_path: resolved.target_path,
                is_external: resolved.is_external,
                ordinal,
                relation,
            });
            ordinal = ordinal.checked_add(1).ok_or_else(|| {
                CatalogError::internal(
                    format!(
                        "attested concept {} overflows the i32 ordinal range",
                        concept.id
                    ),
                    Path::new(""),
                )
            })?;
        }
    }
    Ok(edges)
}

/// Bulk-insert the staged attestation edges with one array-unnest `INSERT` per
/// [`BATCH_SIZE`] chunk, computing `resolved` set-based exactly as
/// [`insert_staged_links`] does for body links.
///
/// Each edge stores `link_kind = 'reference'` (the closest Markdown construct;
/// the true semantics live in `link_relation`), a `NULL` `link_text` (there is
/// no Markdown label), and its `link_relation`. `resolved` is the same
/// correlated `EXISTS` an ordinary internal link uses, so
/// [`reresolve_bundle`] maintains these edges bundle-wide with no special case.
fn insert_attestation_edges(bundle_id: i64, edges: &[AttestationEdge]) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.links
            (bundle_id, tenant_id, source_id, target_id, link_text, target_path,
             link_kind, resolved, is_external, ordinal, link_relation)
        SELECT
            $1,
            (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
            d.source_id, d.target_id, NULL, d.target_path,
            'reference',
            (d.target_id IS NOT NULL AND NOT d.is_external AND EXISTS (
                 SELECT 1 FROM pgokf.concepts c
                 WHERE c.bundle_id = $1 AND c.id = d.target_id)),
            d.is_external, d.ordinal, d.link_relation
        FROM unnest($2::text[], $3::text[], $4::text[], $5::boolean[], $6::integer[], $7::text[])
             AS d(source_id, target_id, target_path, is_external, ordinal, link_relation)";

    let total = edges.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        let chunk = &edges[start..end];
        let source_ids: Vec<&str> = chunk.iter().map(|e| e.source_id.as_str()).collect();
        let target_ids: Vec<Option<String>> = chunk.iter().map(|e| e.target_id.clone()).collect();
        let target_paths: Vec<Option<String>> =
            chunk.iter().map(|e| e.target_path.clone()).collect();
        let is_externals: Vec<bool> = chunk.iter().map(|e| e.is_external).collect();
        let ordinals: Vec<i32> = chunk.iter().map(|e| e.ordinal).collect();
        let relations: Vec<&str> = chunk.iter().map(|e| e.relation).collect();
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                source_ids.into(),
                target_ids.into(),
                target_paths.into(),
                is_externals.into(),
                ordinals.into(),
                relations.into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert attestation edges", &error))?;
    }
    Ok(())
}

/// Project the outgoing links of every staged concept into `pgokf.links`.
///
/// Invoked inside the sync transaction after concept rows (and their metadata)
/// are written and before the bundle row is finalized. The staged sources'
/// existing edges are cleared and one row is inserted per extracted
/// [`okf_parser::Link`], preserving document order via `ordinal`. For a staged
/// concept whose type is Attested Computation, its type-specific reference
/// fields (`computation` / `executor` / `attester`) are additionally resolved
/// into typed edges (`link_relation = 'attestation:*'`) numbered after the body
/// links, so `concept_neighbors` traverses them like any resolved internal edge
/// and readers can SELECT the relation. Internal edges are marked `resolved`
/// when their target concept already exists in the bundle; unresolved internal
/// and external edges are still retained. All phases are set-based and chunked
/// at [`BATCH_SIZE`].
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure so a partial projection
/// aborts the surrounding sync transaction.
pub fn project(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    delete_staged_outgoing(bundle_id, staged)?;
    insert_staged_links(bundle_id, staged)?;
    let attestation_edges = collect_attestation_edges(staged)?;
    insert_attestation_edges(bundle_id, &attestation_edges)?;
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
    use okf_parser::{ParserLimits, parse_concept};

    /// Parse an OKF document (frontmatter plus an optional body) into a
    /// [`StagedConcept`], as the sync engine stages one for the projection seam.
    fn staged(path: &str, frontmatter: &str, body: &str) -> StagedConcept {
        let markdown = format!("---\n{frontmatter}\n---\n\n{body}\n");
        let concept = parse_concept(markdown.as_bytes(), path, ParserLimits::default())
            .expect("fixture parses");
        StagedConcept {
            concept,
            file_hash: format!("hash-{path}"),
            modified_at_epoch: Some(1.5),
            raw_content: None,
        }
    }

    #[test]
    fn collect_attestation_edges_resolves_the_three_reference_keys() {
        // Arrange: an Attested Computation with a bare-string computation, and
        // executor/attester as {resource} mappings, plus one body link so the
        // ordinals must continue past it.
        let batch = vec![staged(
            "rich-concept.md",
            "type: Attested Computation\n\
             title: Monthly active accounts\n\
             computation: /computation.md\n\
             executor:\n  resource: /executor.md\n  receipt: [query_id]\n\
             attester:\n  resource: /attester.md",
            "See [the computation](/computation.md).",
        )];

        // Act
        let edges = collect_attestation_edges(&batch).expect("ordinals in range");

        // Assert: three typed edges, numbered after the single body link (0),
        // each resolving to its internal target concept id.
        assert_eq!(edges.len(), 3);
        assert_eq!(
            edges.iter().map(|e| e.relation).collect::<Vec<_>>(),
            vec![
                "attestation:computation",
                "attestation:executor",
                "attestation:attester"
            ]
        );
        assert_eq!(
            edges.iter().map(|e| e.ordinal).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            edges
                .iter()
                .map(|e| e.target_id.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("computation".to_owned()),
                Some("executor".to_owned()),
                Some("attester".to_owned())
            ]
        );
        assert!(edges.iter().all(|e| !e.is_external));
    }

    #[test]
    fn collect_attestation_edges_ignores_non_attested_concepts() {
        // Arrange: an ordinary concept that happens to carry a computation key.
        let batch = vec![staged(
            "note.md",
            "type: Reference\ntitle: Note\ncomputation: /computation.md",
            "Body.",
        )];

        // Act
        let edges = collect_attestation_edges(&batch).expect("ordinals in range");

        // Assert: only the Attested Computation type contributes edges.
        assert!(edges.is_empty());
    }

    #[test]
    fn collect_attestation_edges_skips_absent_and_external_references() {
        // Arrange: an attested concept with only an external computation and an
        // absent executor/attester.
        let batch = vec![staged(
            "c.md",
            "type: Attested Computation\ntitle: External\n\
             computation: https://example.test/sql",
            "Body.",
        )];

        // Act
        let edges = collect_attestation_edges(&batch).expect("ordinals in range");

        // Assert: the external reference is retained as an external, unresolved
        // edge (never a concept target); no edges for the absent keys.
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, "attestation:computation");
        assert!(edges[0].is_external);
        assert_eq!(edges[0].target_id, None);
        assert_eq!(edges[0].target_path, None);
    }

    #[test]
    fn reference_string_reads_bare_and_mapping_spellings() {
        // Arrange
        let bare = okf_parser::Value::String("/computation.md".to_owned());
        let mapping: okf_parser::Value = okf_parser::Value::Object(okf_parser::Map::from_iter([(
            "resource".to_owned(),
            okf_parser::Value::String("/executor.md".to_owned()),
        )]));
        let numeric = okf_parser::Value::from(42);

        // Act / Assert
        assert_eq!(reference_string(&bare), Some("/computation.md"));
        assert_eq!(reference_string(&mapping), Some("/executor.md"));
        assert_eq!(reference_string(&numeric), None);
    }

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
