// SPDX-License-Identifier: AGPL-3.0-only
//! Column-major marshalling for the sync engine's bounded bulk inserts.
//!
//! The register/refresh engine ([`crate::catalog::sync`]) writes concept and
//! metadata rows with array-unnest bulk `INSERT`s rather than one statement per
//! row. This module owns the *pure* half of that transformation: turning a
//! slice of [`StagedConcept`]s into the parallel, column-major parameter arrays
//! those statements bind. Keeping it side-effect free (no SPI, no Postgres
//! types) makes the marshalling directly unit-testable and keeps
//! [`crate::catalog::sync`] focused on transaction orchestration.
//!
//! # Encoding the two non-scalar columns
//!
//! Every concept column binds as a flat `text[]`/`float8[]` except `tags`,
//! which is itself a per-row `text[]`. Postgres arrays must be rectangular, so
//! a ragged "array of tag arrays" cannot be bound directly. Instead every row's
//! tags are concatenated into one flat [`ConceptColumns::tags_flat`] array, and
//! each row carries an inclusive 1-based `[lo, hi]` slice window
//! ([`ConceptColumns::tag_los`] / [`ConceptColumns::tag_his`]) into it. The SQL
//! side rebuilds each row's `text[]` with a native array slice, preserving
//! element order and yielding an empty array - never SQL `NULL` - for a row
//! with no tags (its window is empty, `hi < lo`), exactly as the row-by-row
//! binding did.
//!
//! Producer metadata values ([`MetadataColumns::values`]) are marshalled as the
//! compact JSON text of each value and cast back to `jsonb` in SQL. That text
//! is produced by the same `serde_json` serialization `pgrx::JsonB` performs
//! before `jsonb_in`, so the stored `jsonb` is byte-identical to the row-by-row
//! binding while this module avoids naming `serde_json` (a dependency the crate
//! deliberately does not declare - the JSON types reach it only through
//! `okf_parser` and `pgrx::JsonB`).

use std::path::Path;

use crate::catalog::links::link_kind_text;
use crate::catalog::types::{StagedConcept, count_to_i32};
use crate::errors::CatalogError;

/// Maximum number of rows packed into a single bulk `INSERT`.
///
/// Both concept rows and metadata rows are processed in chunks of this size so
/// that a very large bundle (tens of thousands of concepts) never materializes
/// one unbounded statement or one unbounded parameter array. The value trades
/// round-trips against per-statement memory: 1000 rows keeps each statement's
/// bound arrays small while amortizing SPI planning across the batch.
pub const BATCH_SIZE: usize = 1000;

/// One concept batch, transposed into the column-major parameter arrays bound
/// by the bulk concept `INSERT`.
///
/// The per-row `Vec`s ([`ids`](Self::ids) … [`modified_ats`](Self::modified_ats)
/// except the tag windows) all share the batch's length and ordering.
/// [`tags_flat`](Self::tags_flat) is the concatenation of every row's tags, and
/// [`tag_los`](Self::tag_los)/[`tag_his`](Self::tag_his) give each row's
/// inclusive 1-based slice window into it (an empty window when a row has no
/// tags).
#[derive(Debug, Clone, PartialEq)]
pub struct ConceptColumns {
    /// Path-derived concept IDs (the `(bundle_id, id)` conflict key).
    pub ids: Vec<String>,
    /// Bundle-relative source paths.
    pub paths: Vec<String>,
    /// OKF concept types.
    pub types: Vec<String>,
    /// Concept titles.
    pub titles: Vec<String>,
    /// Optional short descriptions.
    pub descriptions: Vec<Option<String>>,
    /// Every row's tags concatenated in row order; sliced back per row by the
    /// `[lo, hi]` windows.
    pub tags_flat: Vec<String>,
    /// Inclusive 1-based start index of each row's window into
    /// [`tags_flat`](Self::tags_flat).
    pub tag_los: Vec<i32>,
    /// Inclusive end index of each row's window into
    /// [`tags_flat`](Self::tags_flat); one below the matching `lo` when the row
    /// has no tags, which slices to an empty array.
    pub tag_his: Vec<i32>,
    /// Optional resource declarations, serialized to compact JSON text exactly
    /// as the row-by-row path stored them.
    pub resources: Vec<Option<String>>,
    /// Search-indexed plain-text bodies.
    pub body_texts: Vec<String>,
    /// Lowercase hexadecimal BLAKE3 digests of each source file.
    pub file_hashes: Vec<String>,
    /// Filesystem modification times as seconds since the Unix epoch.
    pub modified_ats: Vec<Option<f64>>,
}

/// Every staged concept's outgoing links, transposed into the parallel arrays
/// bound by the bulk `pgokf.links` `INSERT`.
///
/// The `Vec`s share length and ordering: row `i` inserts one link edge whose
/// source is [`source_ids`](Self::source_ids)`[i]`. Nullable columns are
/// carried as `Vec<Option<String>>` so a `NULL` element survives the `text[]`
/// binding (an external or unresolvable link has `target_id` /
/// [`target_paths`](Self::target_paths)` = NULL`), exactly as the row-by-row
/// binding stored them. `resolved` is intentionally absent: it is computed
/// set-based in SQL (a correlated `EXISTS` against `pgokf.concepts`), matching
/// the per-row `INSERT` the row-by-row path used.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LinkColumns {
    /// Source concept ID of each edge, repeated once per outgoing link.
    pub source_ids: Vec<String>,
    /// Internal destination concept ID, or `None` for external/unresolvable
    /// destinations.
    pub target_ids: Vec<Option<String>>,
    /// Plain-text label of each link (`link_text`); always present, possibly
    /// empty, exactly as the row-by-row binding stored it.
    pub link_texts: Vec<String>,
    /// Normalized bundle-relative destination path, or `None` for external
    /// links.
    pub target_paths: Vec<Option<String>>,
    /// `snake_case` `link_kind` token for each edge.
    pub link_kinds: Vec<String>,
    /// Whether each destination is an external URL.
    pub is_externals: Vec<bool>,
    /// Zero-based document-order position of each link.
    pub ordinals: Vec<i32>,
}

/// A batch of producer-metadata triples, transposed into the parallel arrays
/// bound by the bulk metadata `INSERT`.
///
/// The three `Vec`s share length and ordering: row `i` inserts
/// `(concept_ids[i], keys[i], values[i]::jsonb)`. [`values`](Self::values) hold
/// the compact JSON text of each value, cast back to `jsonb` on the SQL side.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MetadataColumns {
    /// Owning concept IDs, in flattened staging order.
    pub concept_ids: Vec<String>,
    /// Frontmatter keys, aligned with [`concept_ids`](Self::concept_ids).
    pub keys: Vec<String>,
    /// Compact JSON text of each retained value, aligned by position.
    pub values: Vec<String>,
}

/// Every staged concept that carries verbatim source bytes, transposed into
/// the parallel arrays bound by the bulk `pgokf.concept_source` `UPSERT`.
///
/// The three `Vec`s share length and ordering: row `i` upserts one
/// `concept_source` row `(concept_ids[i], contents[i], sizes[i])`. Only
/// concepts staged with [`StagedConcept::raw_content`] `= Some(..)` (the
/// `store_source` policy is on) contribute a row; a concept whose
/// `raw_content` is `None` is skipped entirely, so under the default policy
/// this marshaller yields empty arrays and no rows are written. `contents`
/// binds as `bytea[]` - every element is itself a `bytea` - preserving the
/// source bytes exactly, and `sizes` carries each payload's byte length for the
/// `byte_size` column so a reader can size a retrieval without detoasting the
/// content.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceColumns {
    /// Owning concept IDs, one per stored source, in staging order.
    pub concept_ids: Vec<String>,
    /// Verbatim source-file bytes, aligned with
    /// [`concept_ids`](Self::concept_ids); bound as `bytea[]`.
    pub contents: Vec<Vec<u8>>,
    /// Byte length of each payload, aligned by position, for the `byte_size`
    /// column.
    pub sizes: Vec<i32>,
}

/// Flatten the verbatim source bytes of every staged concept that carries them
/// into the column-major arrays bound by the bulk `pgokf.concept_source`
/// `UPSERT`, in staging order.
///
/// A concept staged without source bytes ([`StagedConcept::raw_content`] is
/// `None`, the default `store_source`-off path) contributes nothing, so the
/// returned arrays hold exactly the concepts whose source is to be persisted.
/// Each row's `byte_size` is the payload length clamped into the `integer`
/// column range with [`count_to_i32`]; the source scan already bounds every
/// file at `pgokf.max_file_bytes` (well under `i32::MAX`), so the clamp is
/// unreachable in practice. The result is returned whole and chunked by the
/// caller at [`BATCH_SIZE`] for bounded bulk upserts.
#[must_use]
pub fn marshal_sources(staged: &[StagedConcept]) -> SourceColumns {
    let mut columns = SourceColumns::default();
    for entry in staged {
        let Some(content) = entry.raw_content.as_ref() else {
            continue;
        };
        columns.concept_ids.push(entry.concept.id.clone());
        columns.sizes.push(count_to_i32(content.len()));
        columns.contents.push(content.clone());
    }
    columns
}

/// Transpose a slice of staged concepts into column-major parameter arrays.
///
/// The returned [`ConceptColumns`] preserves batch order across every column.
/// Tags are flattened into [`ConceptColumns::tags_flat`] with per-row slice
/// windows so the SQL side reproduces each row's `text[]` (and thus the same
/// `tsvector`) exactly, including an empty array for a tag-less row. Each
/// resource is serialized with the same compact JSON encoding the row-by-row
/// upsert used (`serde_json::Value`'s `Display`).
///
/// The tag windows are computed against this batch's own `tags_flat`, so the
/// function must be called per chunk (the offsets reset each call).
#[must_use]
pub fn marshal_concepts(batch: &[StagedConcept]) -> ConceptColumns {
    let mut columns = ConceptColumns {
        ids: Vec::with_capacity(batch.len()),
        paths: Vec::with_capacity(batch.len()),
        types: Vec::with_capacity(batch.len()),
        titles: Vec::with_capacity(batch.len()),
        descriptions: Vec::with_capacity(batch.len()),
        tags_flat: Vec::new(),
        tag_los: Vec::with_capacity(batch.len()),
        tag_his: Vec::with_capacity(batch.len()),
        resources: Vec::with_capacity(batch.len()),
        body_texts: Vec::with_capacity(batch.len()),
        file_hashes: Vec::with_capacity(batch.len()),
        modified_ats: Vec::with_capacity(batch.len()),
    };

    let mut offset: usize = 0;
    for staged in batch {
        let concept = &staged.concept;
        columns.ids.push(concept.id.clone());
        columns.paths.push(concept.path.clone());
        columns.types.push(concept.r#type.clone());
        columns.titles.push(concept.title.clone());
        columns.descriptions.push(concept.description.clone());

        // Inclusive 1-based window [offset + 1, offset + len]; an empty tag list
        // yields hi = lo - 1, which slices to an empty array on the SQL side.
        let len = concept.tags.len();
        columns.tag_los.push(count_to_i32(offset + 1));
        columns.tag_his.push(count_to_i32(offset + len));
        columns.tags_flat.extend(concept.tags.iter().cloned());
        offset += len;

        columns
            .resources
            .push(concept.resource.as_ref().map(ToString::to_string));
        columns.body_texts.push(concept.body_text.clone());
        columns.file_hashes.push(staged.file_hash.clone());
        columns.modified_ats.push(staged.modified_at_epoch);
    }

    columns
}

/// Flatten the producer metadata of every staged concept into parallel arrays
/// of `(concept_id, key, value-as-JSON-text)`, in staging order.
///
/// Concepts with no metadata contribute nothing; the sync engine still clears
/// their stored metadata separately by concept ID. The result is chunked by the
/// caller for bounded bulk insertion, so it is returned whole rather than
/// per-chunk.
#[must_use]
pub fn flatten_metadata(staged: &[StagedConcept]) -> MetadataColumns {
    let capacity = staged
        .iter()
        .map(|entry| entry.concept.metadata.len())
        .sum();
    let mut columns = MetadataColumns {
        concept_ids: Vec::with_capacity(capacity),
        keys: Vec::with_capacity(capacity),
        values: Vec::with_capacity(capacity),
    };

    for entry in staged {
        let concept_id = entry.concept.id.as_str();
        for (key, value) in &entry.concept.metadata {
            columns.concept_ids.push(concept_id.to_owned());
            columns.keys.push(key.clone());
            columns.values.push(value.to_string());
        }
    }

    columns
}

/// Flatten the outgoing links of every staged concept into the column-major
/// parameter arrays bound by the bulk `pgokf.links` `INSERT`, in staging order.
///
/// Every row carries the source concept's ID, so one array-unnest `INSERT`
/// replaces the row-by-row loop that ran one statement per link. The per-link
/// `resolved` flag is *not* marshalled: it is derived set-based in SQL from
/// `target_id`, `is_external`, and an `EXISTS` against `pgokf.concepts` -
/// byte-identical to the correlated-`EXISTS` expression the row-by-row
/// `INSERT` evaluated. The result is returned whole and chunked by the caller
/// at [`BATCH_SIZE`] for bounded bulk insertion.
///
/// # Errors
///
/// Returns a [`CatalogError`] if any link's zero-based `ordinal` exceeds the
/// `i32` range of the `ordinal` column - the same failure the row-by-row
/// `INSERT` raised, preserved verbatim so behavior is unchanged.
pub fn marshal_links(staged: &[StagedConcept]) -> Result<LinkColumns, CatalogError> {
    let capacity: usize = staged.iter().map(|entry| entry.concept.links.len()).sum();
    let mut columns = LinkColumns {
        source_ids: Vec::with_capacity(capacity),
        target_ids: Vec::with_capacity(capacity),
        link_texts: Vec::with_capacity(capacity),
        target_paths: Vec::with_capacity(capacity),
        link_kinds: Vec::with_capacity(capacity),
        is_externals: Vec::with_capacity(capacity),
        ordinals: Vec::with_capacity(capacity),
    };

    for entry in staged {
        let source_id = entry.concept.id.as_str();
        for link in &entry.concept.links {
            let ordinal = i32::try_from(link.ordinal).map_err(|_| {
                CatalogError::internal(
                    format!("link ordinal {} exceeds the i32 range", link.ordinal),
                    Path::new(""),
                )
            })?;
            columns.source_ids.push(source_id.to_owned());
            columns.target_ids.push(link.target_id.clone());
            columns.link_texts.push(link.label.clone());
            columns.target_paths.push(link.target_path.clone());
            columns
                .link_kinds
                .push(link_kind_text(link.kind).to_owned());
            columns.is_externals.push(link.is_external);
            columns.ordinals.push(ordinal);
        }
    }

    Ok(columns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use okf_parser::{ParsedConcept, ParserLimits, parse_concept};

    /// Parse OKF frontmatter into a [`StagedConcept`]. The crate does not depend
    /// on `serde_json`, so fixtures are built from real OKF documents (as
    /// [`crate::catalog::provenance`]'s tests do) rather than by constructing
    /// `serde_json` values directly.
    fn staged(path: &str, frontmatter: &str) -> StagedConcept {
        let markdown = format!("---\n{frontmatter}\n---\n\nBody of {path}.\n");
        let concept: ParsedConcept =
            parse_concept(markdown.as_bytes(), path, ParserLimits::default())
                .expect("fixture parses");
        StagedConcept {
            concept,
            file_hash: format!("hash-{path}"),
            modified_at_epoch: Some(1.5),
            raw_content: None,
        }
    }

    /// Parse an OKF document with a caller-supplied Markdown body (so links can
    /// be embedded) into a [`StagedConcept`].
    fn staged_with_body(path: &str, frontmatter: &str, body: &str) -> StagedConcept {
        let markdown = format!("---\n{frontmatter}\n---\n\n{body}\n");
        let concept: ParsedConcept =
            parse_concept(markdown.as_bytes(), path, ParserLimits::default())
                .expect("fixture parses");
        StagedConcept {
            concept,
            file_hash: format!("hash-{path}"),
            modified_at_epoch: Some(1.5),
            raw_content: None,
        }
    }

    fn ids(batch: &[StagedConcept]) -> Vec<String> {
        batch.iter().map(|s| s.concept.id.clone()).collect()
    }

    #[test]
    fn marshal_concepts_transposes_rows_into_aligned_columns() {
        // Arrange
        let batch = vec![
            staged("alpha.md", "type: note\ntitle: Alpha\ndescription: First"),
            staged("beta.md", "type: guide\ntitle: Beta"),
        ];

        // Act
        let columns = marshal_concepts(&batch);

        // Assert: each column mirrors the corresponding concept field in order.
        assert_eq!(columns.ids, ids(&batch));
        assert_eq!(
            columns.paths,
            vec![batch[0].concept.path.clone(), batch[1].concept.path.clone()]
        );
        assert_eq!(columns.types, vec!["note", "guide"]);
        assert_eq!(columns.titles, vec!["Alpha", "Beta"]);
        assert_eq!(columns.descriptions, vec![Some("First".to_owned()), None]);
        assert_eq!(
            columns.body_texts,
            vec![
                batch[0].concept.body_text.clone(),
                batch[1].concept.body_text.clone()
            ]
        );
        assert_eq!(columns.file_hashes, vec!["hash-alpha.md", "hash-beta.md"]);
        assert_eq!(columns.modified_ats, vec![Some(1.5), Some(1.5)]);
    }

    #[test]
    fn marshal_concepts_flattens_tags_with_inclusive_one_based_windows() {
        // Arrange
        let batch = vec![
            staged("a.md", "type: note\ntitle: A\ntags: [x]"),
            staged("b.md", "type: note\ntitle: B\ntags: [y, z]"),
        ];

        // Act
        let columns = marshal_concepts(&batch);

        // Assert
        assert_eq!(columns.tags_flat, vec!["x", "y", "z"]);
        assert_eq!(columns.tag_los, vec![1, 2]);
        assert_eq!(columns.tag_his, vec![1, 3]);
    }

    #[test]
    fn marshal_concepts_encodes_empty_tags_as_an_empty_window() {
        // Arrange: a tag-less row between two tagged rows.
        let batch = vec![
            staged("a.md", "type: note\ntitle: A\ntags: [x]"),
            staged("b.md", "type: note\ntitle: B"),
            staged("c.md", "type: note\ntitle: C\ntags: [y]"),
        ];

        // Act
        let columns = marshal_concepts(&batch);

        // Assert: the empty row's window is [2, 1] (hi < lo => empty slice),
        // and it does not disturb the following row's window.
        assert_eq!(columns.tags_flat, vec!["x", "y"]);
        assert_eq!(columns.tag_los, vec![1, 2, 2]);
        assert_eq!(columns.tag_his, vec![1, 1, 2]);
    }

    #[test]
    fn marshal_concepts_serializes_scalar_resource_to_compact_json() {
        // Arrange
        let batch = vec![staged(
            "a.md",
            "type: note\ntitle: A\nresource: https://example.test",
        )];

        // Act
        let columns = marshal_concepts(&batch);

        // Assert: a scalar string resource marshals to quoted compact JSON,
        // matching serde_json's serialization (what a ::jsonb cast reverses).
        assert_eq!(
            columns.resources,
            vec![Some("\"https://example.test\"".to_owned())]
        );
    }

    #[test]
    fn marshal_concepts_records_absent_resource_as_none() {
        // Arrange
        let batch = vec![staged("a.md", "type: note\ntitle: A")];

        // Act
        let columns = marshal_concepts(&batch);

        // Assert
        assert_eq!(columns.resources, vec![None]);
    }

    #[test]
    fn flatten_metadata_expands_every_key_in_staging_order() {
        // Arrange: producer-defined (unknown) frontmatter keys become metadata.
        let batch = vec![
            staged(
                "a.md",
                "type: note\ntitle: A\ncustom_count: 1\ncustom_flag: true",
            ),
            staged("b.md", "type: note\ntitle: B\ncustom_note: hello"),
        ];

        // Act
        let columns = flatten_metadata(&batch);

        // Assert: three triples total, grouped by concept in staging order.
        assert_eq!(columns.concept_ids.len(), 3);
        assert_eq!(columns.keys.len(), 3);
        assert_eq!(columns.values.len(), 3);
        assert_eq!(&columns.concept_ids[..2], &["a", "a"]);
        assert_eq!(columns.concept_ids[2], "b");
    }

    #[test]
    fn flatten_metadata_serializes_values_as_compact_json_text() {
        // Arrange
        let batch = vec![staged(
            "a.md",
            "type: note\ntitle: A\ncustom_count: 42\ncustom_note: hello",
        )];

        // Act
        let columns = flatten_metadata(&batch);

        // Assert: values are compact JSON text (numbers bare, strings quoted),
        // so a SQL ::jsonb cast reproduces the row-by-row JsonB binding exactly.
        let count = columns
            .keys
            .iter()
            .position(|key| key == "custom_count")
            .expect("custom_count retained");
        let note = columns
            .keys
            .iter()
            .position(|key| key == "custom_note")
            .expect("custom_note retained");
        assert_eq!(columns.values[count], "42");
        assert_eq!(columns.values[note], "\"hello\"");
    }

    #[test]
    fn flatten_metadata_skips_concepts_without_metadata() {
        // Arrange
        let batch = vec![staged("a.md", "type: note\ntitle: A")];

        // Act
        let columns = flatten_metadata(&batch);

        // Assert
        assert!(columns.concept_ids.is_empty());
        assert!(columns.keys.is_empty());
        assert!(columns.values.is_empty());
    }

    #[test]
    fn marshal_links_flattens_every_edge_in_source_order() {
        // Arrange: two concepts, the first with two links, the second with one.
        let batch = vec![
            staged_with_body(
                "a.md",
                "type: note\ntitle: A",
                "See [Bee](b.md) and [Site](https://example.test).",
            ),
            staged_with_body("b.md", "type: note\ntitle: B", "Back to [Ay](a.md)."),
        ];

        // Act
        let columns = marshal_links(&batch).expect("ordinals in range");

        // Assert: one row per link, grouped by source in staging order.
        assert_eq!(columns.source_ids, vec!["a", "a", "b"]);
        assert_eq!(columns.ordinals, vec![0, 1, 0]);
        assert_eq!(columns.link_texts, vec!["Bee", "Site", "Ay"]);
    }

    #[test]
    fn marshal_links_carries_null_target_for_external_edges() {
        // Arrange: an internal edge (resolvable target) beside an external URL.
        let batch = vec![staged_with_body(
            "a.md",
            "type: note\ntitle: A",
            "[Internal](b.md) and [External](https://example.test).",
        )];

        // Act
        let columns = marshal_links(&batch).expect("ordinals in range");

        // Assert: the internal edge keeps its normalized target; the external
        // one carries NULL target_id / target_path and is_external = true,
        // exactly as the row-by-row INSERT bound them.
        assert_eq!(columns.target_ids, vec![Some("b".to_owned()), None]);
        assert_eq!(columns.target_paths, vec![Some("b.md".to_owned()), None]);
        assert_eq!(columns.is_externals, vec![false, true]);
        assert_eq!(columns.link_kinds, vec!["inline", "inline"]);
    }

    /// Attach verbatim source bytes to a staged concept, as the sync engine
    /// does when the `store_source` policy is enabled.
    fn staged_with_source(path: &str, frontmatter: &str, source: &[u8]) -> StagedConcept {
        let mut entry = staged(path, frontmatter);
        entry.raw_content = Some(source.to_vec());
        entry
    }

    #[test]
    fn marshal_sources_flattens_only_concepts_carrying_source_bytes() {
        // Arrange: a source-bearing concept between two without stored source,
        // exactly the mix the non-strict / partial store_source path produces.
        let source = b"---\ntype: note\ntitle: A\n---\n\nBody.\n";
        let batch = vec![
            staged("skip-a.md", "type: note\ntitle: SkipA"),
            staged_with_source("keep.md", "type: note\ntitle: Keep", source),
            staged("skip-b.md", "type: note\ntitle: SkipB"),
        ];

        // Act
        let columns = marshal_sources(&batch);

        // Assert: only the source-bearing concept is marshalled, and its bytes
        // and byte length are carried verbatim.
        assert_eq!(columns.concept_ids, vec!["keep".to_owned()]);
        assert_eq!(columns.contents, vec![source.to_vec()]);
        assert_eq!(columns.sizes, vec![count_to_i32(source.len())]);
    }

    #[test]
    fn marshal_sources_is_empty_when_no_concept_carries_source() {
        // Arrange: the default store_source-off path stages every concept with
        // raw_content = None.
        let batch = vec![
            staged("a.md", "type: note\ntitle: A"),
            staged("b.md", "type: note\ntitle: B"),
        ];

        // Act
        let columns = marshal_sources(&batch);

        // Assert: no rows to upsert, so no concept_source write occurs.
        assert!(columns.concept_ids.is_empty());
        assert!(columns.contents.is_empty());
        assert!(columns.sizes.is_empty());
    }

    #[test]
    fn marshal_sources_preserves_arbitrary_binary_bytes() {
        // Arrange: source bytes including NUL and high bytes must survive as an
        // exact bytea payload, not a lossy string.
        let source = &[0x00_u8, 0xff, 0x10, b'#', 0x00, 0xfe];
        let batch = vec![staged_with_source(
            "bin.md",
            "type: note\ntitle: Bin",
            source,
        )];

        // Act
        let columns = marshal_sources(&batch);

        // Assert
        assert_eq!(columns.contents, vec![source.to_vec()]);
        assert_eq!(columns.sizes, vec![count_to_i32(source.len())]);
    }

    #[test]
    fn marshal_links_is_empty_for_concepts_without_links() {
        // Arrange
        let batch = vec![staged("a.md", "type: note\ntitle: A")];

        // Act
        let columns = marshal_links(&batch).expect("ordinals in range");

        // Assert
        assert!(columns.source_ids.is_empty());
        assert!(columns.ordinals.is_empty());
    }
}
