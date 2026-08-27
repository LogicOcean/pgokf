//! Provenance/trust/lifecycle projection seam.
//!
//! # Seam contract for the provenance feature wave
//!
//! Implement everything in **this file only** — the sync engine already
//! calls [`project`] and must not be edited. This wave:
//!
//! 1. Adds the `pgokf.concept_provenance` table in the `provenance_table`
//!    `extension_sql!` block (`requires = ["catalog_tables"]`), keyed by
//!    `(bundle_id, concept_id) REFERENCES pgokf.concepts (bundle_id, id)
//!    ON DELETE CASCADE` so removed concepts drop their provenance row
//!    automatically (which is why removals need no seam call).
//! 2. In [`project`], extracts the OKF v0.2 provenance, trust, and lifecycle
//!    frontmatter the core parser leaves unmodeled in
//!    [`okf_parser::ParsedConcept::metadata`], maps the well-known keys onto
//!    typed columns, and stashes the complete provenance-related subset into
//!    the `details` `jsonb` column for lossless retention.
//! 3. Uses parameterized SPI only and surfaces failures as [`CatalogError`]
//!    so the surrounding sync transaction rolls back atomically.
//!
//! # OKF v0.2 key mapping
//!
//! The typed columns are derived defensively — a key may be absent,
//! wrong-typed, or nested, and any such case is coerced or skipped, never
//! panicked on:
//!
//! | Column                | Source frontmatter                                            |
//! | --------------------- | ------------------------------------------------------------- |
//! | `generated_by`        | scalar `generated_by`, bare `generated`, or `generated.by`    |
//! | `verified`            | `verified` as a bool, or the presence of verification records |
//! | `verification_method` | scalar `verification_method`, bare `verification`, or `verification.method` |
//! | `freshness`           | scalar `freshness`, falling back to the lifecycle `status`     |
//!
//! Every recognized provenance/trust/lifecycle key (see [`PROVENANCE_KEYS`])
//! is retained verbatim in `details`; keys outside that set are already
//! preserved per-key in `pgokf.concept_metadata` by the core engine, so no
//! producer data is lost.
//!
//! # Row semantics
//!
//! A concept carrying **no** recognized provenance/trust/lifecycle key
//! produces **no** `concept_provenance` row: the projection is sparse, so a
//! `LEFT JOIN` distinguishes "concept has no provenance frontmatter" from
//! "concept has provenance but no verification". A concept that carries some
//! provenance keys but none that map to a typed column produces a row with
//! all-`NULL` typed columns and a populated `details`. Projection is
//! delete-then-insert per concept, so re-syncing a concept whose provenance
//! frontmatter was removed correctly drops its stale row.

use std::path::Path;

use pgrx::{Spi, extension_sql};

use crate::catalog::batch::BATCH_SIZE;
use crate::catalog::types::StagedConcept;
use crate::errors::CatalogError;
use okf_parser::ParsedConcept;

extension_sql!(
    r"
CREATE TABLE pgokf.concept_provenance (
    bundle_id           bigint NOT NULL,
    concept_id          text NOT NULL,
    generated_by        text,
    verified            boolean,
    verification_method text,
    freshness           text,
    details             jsonb NOT NULL DEFAULT '{}'::jsonb,
    CONSTRAINT concept_provenance_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT concept_provenance_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE
);

CREATE INDEX concept_provenance_verified_idx
    ON pgokf.concept_provenance (verified)
    WHERE verified;

COMMENT ON TABLE pgokf.concept_provenance IS
    'Provenance, trust, and lifecycle projection of OKF v0.2 concept frontmatter: typed columns plus the full provenance-related key subset as jsonb. Sparse — only concepts carrying such frontmatter have a row.';
COMMENT ON COLUMN pgokf.concept_provenance.generated_by IS
    'Producer/agent that generated the concept, from generated_by, generated, or generated.by; NULL when absent or not a string.';
COMMENT ON COLUMN pgokf.concept_provenance.verified IS
    'True when the concept carries a truthy verified flag or a non-empty set of verification records; NULL when the frontmatter has no verified key.';
COMMENT ON COLUMN pgokf.concept_provenance.verification_method IS
    'Declared verification method, from verification_method, verification, or verification.method; NULL when absent or not a string.';
COMMENT ON COLUMN pgokf.concept_provenance.freshness IS
    'Lifecycle freshness signal, from the freshness key or the lifecycle status; NULL when neither is a string.';
COMMENT ON COLUMN pgokf.concept_provenance.details IS
    'Lossless jsonb copy of every recognized provenance/trust/lifecycle frontmatter key (sources, generated, verified, usage_window, stale_after, parameters, and peers).';

GRANT SELECT ON pgokf.concept_provenance TO pgokf_reader;
",
    name = "provenance_table",
    requires = ["catalog_tables"]
);

/// OKF v0.2 frontmatter keys treated as provenance/trust/lifecycle data and
/// retained verbatim in `concept_provenance.details`.
///
/// The set intentionally mirrors the OKF v0.2 provenance surface (origin,
/// verification, freshness window, and declared inputs). Keys outside it stay
/// in `pgokf.concept_metadata`, so restricting the subset never loses data.
const PROVENANCE_KEYS: &[&str] = &[
    "sources",
    "generated",
    "generated_by",
    "verified",
    "verification",
    "verification_method",
    "freshness",
    "usage_window",
    "stale_after",
    "status",
    "parameters",
];

/// The typed provenance columns extracted from one concept's frontmatter.
///
/// Each field is `None` when the corresponding key is absent or cannot be
/// coerced to the column's type; the projection never panics on malformed
/// input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProvenanceColumns {
    generated_by: Option<String>,
    verified: Option<bool>,
    verification_method: Option<String>,
    freshness: Option<String>,
}

/// Trim a string value, returning an owned copy only when it is non-empty.
fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Whether a frontmatter key belongs to the provenance/trust/lifecycle subset.
fn is_provenance_key(key: &str) -> bool {
    PROVENANCE_KEYS.contains(&key)
}

/// Resolve `generated_by` from the scalar `generated_by`, a bare string
/// `generated`, or the `generated.by` object member, in that order.
fn extract_generated_by(concept: &ParsedConcept) -> Option<String> {
    let metadata = &concept.metadata;
    if let Some(text) = metadata
        .get("generated_by")
        .and_then(|value| value.as_str())
    {
        return non_empty(text);
    }
    let generated = metadata.get("generated")?;
    if let Some(text) = generated.as_str() {
        return non_empty(text);
    }
    let by = generated
        .as_object()
        .and_then(|object| object.get("by"))
        .and_then(|value| value.as_str())?;
    non_empty(by)
}

/// Resolve the `verified` flag defensively across its OKF shapes:
/// an explicit bool, a non-empty array/object of verification records, or a
/// non-empty verification note. Absent, null, or numeric values yield `None`.
fn extract_verified(concept: &ParsedConcept) -> Option<bool> {
    let value = concept.metadata.get("verified")?;
    if let Some(flag) = value.as_bool() {
        return Some(flag);
    }
    if let Some(records) = value.as_array() {
        return Some(!records.is_empty());
    }
    if let Some(object) = value.as_object() {
        return Some(!object.is_empty());
    }
    if let Some(text) = value.as_str() {
        return Some(!text.trim().is_empty());
    }
    None
}

/// Resolve `verification_method` from the scalar `verification_method`, a bare
/// string `verification`, or the `verification.method` object member.
fn extract_verification_method(concept: &ParsedConcept) -> Option<String> {
    let metadata = &concept.metadata;
    if let Some(text) = metadata
        .get("verification_method")
        .and_then(|value| value.as_str())
    {
        return non_empty(text);
    }
    let verification = metadata.get("verification")?;
    if let Some(text) = verification.as_str() {
        return non_empty(text);
    }
    let method = verification
        .as_object()
        .and_then(|object| object.get("method"))
        .and_then(|value| value.as_str())?;
    non_empty(method)
}

/// Resolve the `freshness` lifecycle signal from the `freshness` key, falling
/// back to the lifecycle `status` — the single-value freshness signal OKF
/// v0.2 producers most commonly ship (for example `status: stable`).
fn extract_freshness(concept: &ParsedConcept) -> Option<String> {
    let metadata = &concept.metadata;
    if let Some(text) = metadata.get("freshness").and_then(|value| value.as_str()) {
        return non_empty(text);
    }
    let status = metadata.get("status").and_then(|value| value.as_str())?;
    non_empty(status)
}

/// Extract the typed provenance columns from a concept's frontmatter.
fn extract_columns(concept: &ParsedConcept) -> ProvenanceColumns {
    ProvenanceColumns {
        generated_by: extract_generated_by(concept),
        verified: extract_verified(concept),
        verification_method: extract_verification_method(concept),
        freshness: extract_freshness(concept),
    }
}

/// Build the lossless `details` payload: the concept's frontmatter restricted
/// to the recognized provenance/trust/lifecycle keys, as `jsonb`.
fn extract_details(concept: &ParsedConcept) -> pgrx::JsonB {
    let mut details = concept.metadata.clone();
    let discard: Vec<String> = details
        .keys()
        .filter(|key| !is_provenance_key(key))
        .cloned()
        .collect();
    for key in discard {
        details.remove(&key);
    }
    pgrx::JsonB(details.into())
}

/// Whether a `details` payload carries no provenance keys, meaning the concept
/// should produce no `concept_provenance` row.
fn details_is_empty(details: &pgrx::JsonB) -> bool {
    // A direct `match` rather than `is_none_or(Map::is_empty)`: naming the
    // `serde_json::Map` method would require a direct dependency this crate
    // does not declare (the type reaches us only through `pgrx::JsonB`).
    match details.0.as_object() {
        Some(object) => object.is_empty(),
        None => true,
    }
}

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// The staged provenance rows, transposed into the parallel arrays bound by
/// the bulk `concept_provenance` `INSERT`.
///
/// Only concepts that carry recognized provenance frontmatter contribute a row
/// (the projection is sparse), so every `Vec` shares the same length and
/// ordering: row `i` inserts one `concept_provenance` row. `details` holds the
/// compact JSON text of each lossless payload, cast back to `jsonb` in SQL —
/// the same serialization `pgrx::JsonB` performs before `jsonb_in`, so the
/// stored `jsonb` is byte-identical to the row-by-row binding.
#[derive(Debug, Clone, Default)]
struct ProvenanceRows {
    concept_ids: Vec<String>,
    generated_by: Vec<Option<String>>,
    verified: Vec<Option<bool>>,
    verification_method: Vec<Option<String>>,
    freshness: Vec<Option<String>>,
    details: Vec<String>,
}

/// Extract the sparse provenance rows for every staged concept, in staging
/// order, skipping concepts that carry no recognized provenance frontmatter
/// (they produce no row, exactly as the row-by-row projection did).
fn collect_provenance_rows(staged: &[StagedConcept]) -> ProvenanceRows {
    let mut rows = ProvenanceRows::default();
    for entry in staged {
        let concept = &entry.concept;
        let details = extract_details(concept);
        if details_is_empty(&details) {
            continue;
        }
        let columns = extract_columns(concept);
        rows.concept_ids.push(concept.id.clone());
        rows.generated_by.push(columns.generated_by);
        rows.verified.push(columns.verified);
        rows.verification_method.push(columns.verification_method);
        rows.freshness.push(columns.freshness);
        // Compact JSON text of the lossless payload; `serde_json::Value`'s
        // `Display` matches what `pgrx::JsonB` serializes before `jsonb_in`, so
        // the `::jsonb` cast below reproduces the row-by-row `JsonB` binding.
        rows.details.push(details.0.to_string());
    }
    rows
}

/// Delete any existing provenance rows for the staged concepts, in bounded
/// batches, so re-projection is idempotent and stale rows never linger after
/// provenance is removed.
///
/// Every staged concept is cleared — including ones that now carry no
/// provenance frontmatter and thus contribute no replacement row — exactly as
/// the row-by-row `delete-then-insert` did. Concept IDs are chunked at
/// [`BATCH_SIZE`] so the `= ANY($2)` list never grows unbounded.
fn delete_staged_provenance(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    for chunk in staged.chunks(BATCH_SIZE) {
        let concept_ids: Vec<&str> = chunk
            .iter()
            .map(|entry| entry.concept.id.as_str())
            .collect();
        Spi::run_with_args(
            "DELETE FROM pgokf.concept_provenance WHERE bundle_id = $1 AND concept_id = ANY($2)",
            &[bundle_id.into(), concept_ids.into()],
        )
        .map_err(|error| spi_error("failed to clear concept provenance", &error))?;
    }
    Ok(())
}

/// Bulk-insert the collected provenance rows with one array-unnest `INSERT`
/// per [`BATCH_SIZE`] chunk.
fn insert_provenance_rows(bundle_id: i64, rows: &ProvenanceRows) -> Result<(), CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.concept_provenance
            (bundle_id, concept_id, generated_by, verified,
             verification_method, freshness, details)
        SELECT
            $1, d.concept_id, d.generated_by, d.verified,
            d.verification_method, d.freshness, d.details::jsonb
        FROM unnest(
                 $2::text[], $3::text[], $4::boolean[],
                 $5::text[], $6::text[], $7::text[])
             AS d(concept_id, generated_by, verified,
                   verification_method, freshness, details)";

    let total = rows.concept_ids.len();
    for start in (0..total).step_by(BATCH_SIZE) {
        let end = usize::min(start + BATCH_SIZE, total);
        Spi::run_with_args(
            INSERT,
            &[
                bundle_id.into(),
                rows.concept_ids[start..end].to_vec().into(),
                rows.generated_by[start..end].to_vec().into(),
                rows.verified[start..end].to_vec().into(),
                rows.verification_method[start..end].to_vec().into(),
                rows.freshness[start..end].to_vec().into(),
                rows.details[start..end].to_vec().into(),
            ],
        )
        .map_err(|error| spi_error("failed to insert concept provenance", &error))?;
    }
    Ok(())
}

/// Project provenance/trust/lifecycle data for every staged concept.
///
/// Invoked inside the sync transaction after
/// [`crate::catalog::links::project`] and before the bundle row is finalized.
/// Every staged concept's existing provenance row is cleared, and each concept
/// that carries recognized provenance frontmatter is re-inserted with a freshly
/// extracted row (typed columns plus the lossless `details` payload). Concepts
/// without such frontmatter produce no row. Both phases are set-based and
/// chunked at [`BATCH_SIZE`], replacing the former per-concept SPI round-trips
/// while producing byte-identical rows.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding
/// sync transaction so a partial projection is never committed.
pub fn project(bundle_id: i64, staged: &[StagedConcept]) -> Result<(), CatalogError> {
    delete_staged_provenance(bundle_id, staged)?;
    let rows = collect_provenance_rows(staged);
    insert_provenance_rows(bundle_id, &rows)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use okf_parser::{ParserLimits, parse_concept};

    /// Parse representative OKF frontmatter into a [`ParsedConcept`] so the
    /// extraction helpers can be exercised on the real metadata shape.
    fn parse(frontmatter: &str) -> ParsedConcept {
        let markdown = format!("---\n{frontmatter}---\n\nBody text.\n");
        parse_concept(markdown.as_bytes(), "concept.md", ParserLimits::default())
            .expect("representative fixture parses")
    }

    /// Sorted keys retained in a `details` payload, for order-independent
    /// assertions.
    fn detail_keys(details: &pgrx::JsonB) -> Vec<String> {
        let mut keys: Vec<String> = details
            .0
            .as_object()
            .expect("details is a json object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn extract_columns_maps_rich_okf_v0_2_frontmatter() {
        // Arrange: nested `generated`, a list of `verified` records, and a
        // lifecycle `status`, as OKF v0.2 producers ship them.
        let concept = parse(
            "type: Attested Computation\n\
             title: Monthly active accounts\n\
             status: stable\n\
             generated:\n  by: catalog-agent/1.0\n  at: 2026-07-01T12:00:00Z\n\
             verified:\n  - by: process:metric-validation\n  - by: human:reviewer\n",
        );

        // Act
        let columns = extract_columns(&concept);

        // Assert
        assert_eq!(columns.generated_by.as_deref(), Some("catalog-agent/1.0"));
        assert_eq!(columns.verified, Some(true));
        assert_eq!(columns.freshness.as_deref(), Some("stable"));
        assert_eq!(columns.verification_method, None);
    }

    #[test]
    fn extract_columns_reads_scalar_producer_keys() {
        // Arrange: flat scalar spellings of every column.
        let concept = parse(
            "type: Reference\n\
             title: Scalar provenance\n\
             generated_by: pipeline/9\n\
             verified: true\n\
             verification_method: sql-equality\n\
             freshness: fresh\n",
        );

        // Act
        let columns = extract_columns(&concept);

        // Assert
        assert_eq!(
            columns,
            ProvenanceColumns {
                generated_by: Some("pipeline/9".to_owned()),
                verified: Some(true),
                verification_method: Some("sql-equality".to_owned()),
                freshness: Some("fresh".to_owned()),
            }
        );
    }

    #[test]
    fn extract_columns_reads_nested_verification_method() {
        // Arrange: `verification.method` object member.
        let concept = parse(
            "type: Reference\n\
             title: Nested verification\n\
             verification:\n  method: replayed-receipt\n",
        );

        // Act
        let columns = extract_columns(&concept);

        // Assert
        assert_eq!(
            columns.verification_method.as_deref(),
            Some("replayed-receipt")
        );
    }

    #[test]
    fn extract_columns_is_all_none_without_provenance() {
        // Arrange: a concept with only the required core frontmatter.
        let concept = parse("type: Reference\ntitle: Plain concept\n");

        // Act
        let columns = extract_columns(&concept);

        // Assert
        assert_eq!(columns, ProvenanceColumns::default());
    }

    #[test]
    fn extract_verified_treats_empty_records_as_unverified() {
        // Arrange: an empty verification list is not a verification.
        let concept = parse("type: Reference\ntitle: Empty verified\nverified: []\n");

        // Act
        let verified = extract_verified(&concept);

        // Assert
        assert_eq!(verified, Some(false));
    }

    #[test]
    fn extract_columns_coerces_wrong_typed_values_without_panicking() {
        // Arrange: `generated` as a number and `verified` as null — malformed
        // producer data that must degrade to NULL columns, never a panic.
        let concept = parse(
            "type: Reference\n\
             title: Malformed provenance\n\
             generated: 42\n\
             verified: null\n\
             freshness: []\n",
        );

        // Act
        let columns = extract_columns(&concept);

        // Assert
        assert_eq!(columns.generated_by, None);
        assert_eq!(columns.verified, None);
        assert_eq!(columns.freshness, None);
    }

    #[test]
    fn extract_details_retains_only_provenance_keys() {
        // Arrange: provenance keys mixed with producer-namespaced extras that
        // belong in concept_metadata, not the provenance details.
        let concept = parse(
            "type: Attested Computation\n\
             title: Subset retention\n\
             status: stable\n\
             stale_after: 2027-01-01T00:00:00Z\n\
             generated:\n  by: catalog-agent/1.0\n\
             verified:\n  - by: process:metric-validation\n\
             usage_window:\n  from: 2026-06-01T00:00:00Z\n\
             sources:\n  - id: events-table\n\
             parameters:\n  - name: month_start\n\
             runtime: postgres\n\
             computation: /computation.md\n\
             producer_extension:\n  quality_band: gold\n",
        );

        // Act
        let details = extract_details(&concept);

        // Assert
        assert_eq!(
            detail_keys(&details),
            vec![
                "generated".to_owned(),
                "parameters".to_owned(),
                "sources".to_owned(),
                "stale_after".to_owned(),
                "status".to_owned(),
                "usage_window".to_owned(),
                "verified".to_owned(),
            ]
        );
    }

    /// Wrap a parsed concept as a [`StagedConcept`] for the batch collectors.
    fn stage(concept: ParsedConcept) -> StagedConcept {
        StagedConcept {
            concept,
            file_hash: "hash".to_owned(),
            modified_at_epoch: Some(1.5),
        }
    }

    #[test]
    fn collect_provenance_rows_emits_a_row_only_for_provenance_bearing_concepts() {
        // Arrange: one concept with provenance frontmatter, one without.
        let staged = vec![
            stage(parse(
                "type: Reference\ntitle: With provenance\ngenerated_by: pipeline/9\n",
            )),
            stage(parse("type: Reference\ntitle: Plain concept\n")),
        ];

        // Act
        let rows = collect_provenance_rows(&staged);

        // Assert: the plain concept is skipped (sparse projection), and the
        // provenance-bearing one's typed column and JSON details are marshalled.
        assert_eq!(rows.concept_ids, vec!["concept".to_owned()]);
        assert_eq!(rows.generated_by, vec![Some("pipeline/9".to_owned())]);
        assert_eq!(rows.details.len(), 1);
        assert_eq!(rows.details[0], "{\"generated_by\":\"pipeline/9\"}");
    }

    #[test]
    fn extract_details_is_empty_without_provenance() {
        // Arrange: no provenance frontmatter present.
        let concept = parse("type: Reference\ntitle: No provenance\n");

        // Act
        let details = extract_details(&concept);

        // Assert
        assert!(details_is_empty(&details));
    }

    #[test]
    fn extract_details_preserves_nested_source_structure_losslessly() {
        // Arrange: a structured source entry must survive verbatim.
        let concept = parse(
            "type: Reference\n\
             title: Lossless sources\n\
             sources:\n  - id: events-table\n    usage_count: 18000\n",
        );

        // Act
        let details = extract_details(&concept);

        // Assert
        let source = details
            .0
            .as_object()
            .and_then(|object| object.get("sources"))
            .and_then(|value| value.as_array())
            .and_then(|records| records.first())
            .and_then(|value| value.as_object())
            .expect("first source entry is an object");
        let id = source.get("id").expect("source id present");
        assert_eq!(id.as_str(), Some("events-table"));
        let usage_count = source.get("usage_count").expect("usage_count present");
        assert_eq!(usage_count.as_u64(), Some(18_000));
    }
}
